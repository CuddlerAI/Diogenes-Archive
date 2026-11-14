use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::time::Duration;

use anyhow::{Context, ensure};
use serde::Deserialize;

use crate::domain::collector::{GameAdmission, GameSummary};
use crate::services::api_endpoint::production_api_base_url;
use crate::services::credentials::{
    self, CredentialKind, CredentialStore, PRODUCTION_CREDENTIAL_SCOPE,
};

const RESPONSE_LIMIT_BYTES: u64 = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdmissionResponse {
    schema_version: String,
    games: Vec<AdmissionRow>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdmissionRow {
    game_id: String,
    admission: ServerAdmission,
    reason: Option<AdmissionReason>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ServerAdmission {
    Admitted,
    Onboarding,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AdmissionReason {
    CaptureNotAllowed,
}

pub fn overlay_server_admission(
    games: &mut [GameSummary],
    credential_store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let Some(credential) = credentials::load(
        credential_store,
        CredentialKind::Device,
        PRODUCTION_CREDENTIAL_SCOPE,
    )?
    else {
        // Before enrollment there is no ingest principal.  Unknown is explicit
        // and lets the UI use the packaged roster only as a temporary fallback.
        return Ok(());
    };
    let base_url = production_api_base_url()?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("DataMiner/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build game admission HTTP client")?;
    let mut response = client
        .get(format!(
            "{}/v1/game-admissions",
            base_url.trim_end_matches('/')
        ))
        .bearer_auth(credential.expose())
        .header("x-dataminer-client-version", env!("CARGO_PKG_VERSION"))
        .send()
        .context("game admission request failed")?;
    let status = response.status();
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(RESPONSE_LIMIT_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("game admission response read failed")?;
    ensure!(
        bytes.len() as u64 <= RESPONSE_LIMIT_BYTES,
        "game admission response exceeds {RESPONSE_LIMIT_BYTES} bytes"
    );
    ensure!(
        status.is_success(),
        "game admission server returned HTTP {status}"
    );
    let payload: AdmissionResponse =
        serde_json::from_slice(&bytes).context("game admission response is not valid JSON")?;
    apply_response(games, payload)
}

fn apply_response(games: &mut [GameSummary], payload: AdmissionResponse) -> anyhow::Result<()> {
    ensure!(
        payload.schema_version == "dm-game-admission-1",
        "unsupported game admission schema {}",
        payload.schema_version
    );
    let mut seen = HashSet::new();
    let mut rows = HashMap::new();
    for row in payload.games {
        ensure!(
            !row.game_id.is_empty() && row.game_id.len() <= 128,
            "game admission contains an invalid game id"
        );
        ensure!(
            seen.insert(row.game_id.clone()),
            "game admission contains duplicate id {}",
            row.game_id
        );
        rows.insert(row.game_id.clone(), row);
    }

    for game in games {
        match rows.get(&game.game_id) {
            Some(row) => match row.admission {
                ServerAdmission::Admitted => {
                    ensure!(
                        row.reason.is_none(),
                        "admitted game must not carry a blocker"
                    );
                    game.admission = GameAdmission::Admitted;
                    game.admission_reason = None;
                }
                ServerAdmission::Onboarding => {
                    ensure!(
                        matches!(row.reason, Some(AdmissionReason::CaptureNotAllowed)),
                        "onboarding game must name capture_not_allowed"
                    );
                    game.admission = GameAdmission::Onboarding;
                    game.admission_reason = Some("capture_not_allowed".to_owned());
                }
            },
            None => {
                game.admission = GameAdmission::Onboarding;
                game.admission_reason = Some("game_not_registered".to_owned());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use capture_core::ingest_contract::SessionContract;

    fn game(game_id: &str) -> GameSummary {
        GameSummary {
            game_id: game_id.to_owned(),
            display_name: game_id.to_owned(),
            recording_profile_id: "recording".to_owned(),
            recording_profile_version: "v1".to_owned(),
            recording_profile_hash: "sha256:recording".to_owned(),
            device_profile_id: "device".to_owned(),
            mapping_id: "mapping".to_owned(),
            mapping_version: "v1".to_owned(),
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
            non_trainable: true,
            session_contract: SessionContract::LocalDevV1,
            admission: GameAdmission::Unknown,
            admission_reason: None,
        }
    }

    #[test]
    fn live_rows_override_the_catalog_and_missing_ids_fail_closed() {
        let mut games = vec![game("ready"), game("paused"), game("missing")];
        apply_response(
            &mut games,
            AdmissionResponse {
                schema_version: "dm-game-admission-1".to_owned(),
                games: vec![
                    AdmissionRow {
                        game_id: "ready".to_owned(),
                        admission: ServerAdmission::Admitted,
                        reason: None,
                    },
                    AdmissionRow {
                        game_id: "paused".to_owned(),
                        admission: ServerAdmission::Onboarding,
                        reason: Some(AdmissionReason::CaptureNotAllowed),
                    },
                ],
            },
        )
        .unwrap();

        assert_eq!(games[0].admission, GameAdmission::Admitted);
        assert_eq!(games[0].admission_reason, None);
        assert_eq!(games[1].admission, GameAdmission::Onboarding);
        assert_eq!(
            games[1].admission_reason.as_deref(),
            Some("capture_not_allowed")
        );
        assert_eq!(games[2].admission, GameAdmission::Onboarding);
        assert_eq!(
            games[2].admission_reason.as_deref(),
            Some("game_not_registered")
        );
    }

    #[test]
    fn duplicate_server_rows_are_rejected() {
        let mut games = vec![game("same")];
        let result = apply_response(
            &mut games,
            AdmissionResponse {
                schema_version: "dm-game-admission-1".to_owned(),
                games: vec![
                    AdmissionRow {
                        game_id: "same".to_owned(),
                        admission: ServerAdmission::Admitted,
                        reason: None,
                    },
                    AdmissionRow {
                        game_id: "same".to_owned(),
                        admission: ServerAdmission::Admitted,
                        reason: None,
                    },
                ],
            },
        );
        assert!(result.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn admission_and_reason_must_agree() {
        let mut games = vec![game("paused")];
        let result = apply_response(
            &mut games,
            AdmissionResponse {
                schema_version: "dm-game-admission-1".to_owned(),
                games: vec![AdmissionRow {
                    game_id: "paused".to_owned(),
                    admission: ServerAdmission::Onboarding,
                    reason: None,
                }],
            },
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("capture_not_allowed")
        );
    }
}
