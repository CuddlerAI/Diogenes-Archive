//! Shared, fail-closed contract for launching a game with the Depth probe
//! resident before its first graphics objects are created.
//!
//! Local trials and formal recording deliberately use separate lookups over one
//! exhaustive target table. A table entry alone does not make a catalog
//! Depth-ready: formal D3D12 recording also needs the reviewed
//! DepthProfile/CameraBinding pair from that game's catalog and the exact
//! executable below. Every target here has such a pair (CP2077 since 2026-09-18).

use std::ffi::OsString;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EarlyLaunchTarget {
    Cyberpunk2077SteamBuild20383525,
    KingdomCome2SteamBuild23914554,
}

impl EarlyLaunchTarget {
    const ALL: &'static [Self] = &[
        Self::Cyberpunk2077SteamBuild20383525,
        Self::KingdomCome2SteamBuild23914554,
    ];

    pub(crate) const fn cp2077() -> Self {
        Self::Cyberpunk2077SteamBuild20383525
    }

    pub(crate) const fn game_id(self) -> &'static str {
        match self {
            Self::Cyberpunk2077SteamBuild20383525 => "cyberpunk-2077-steam",
            Self::KingdomCome2SteamBuild23914554 => "kingdom-come-deliverance-2-steam",
        }
    }

    pub(crate) const fn steam_app_id(self) -> &'static str {
        match self {
            Self::Cyberpunk2077SteamBuild20383525 => "1091500",
            Self::KingdomCome2SteamBuild23914554 => "1771300",
        }
    }

    pub(crate) const fn steam_build_id(self) -> &'static str {
        match self {
            Self::Cyberpunk2077SteamBuild20383525 => "20383525",
            Self::KingdomCome2SteamBuild23914554 => "23914554",
        }
    }

    pub(crate) const fn steam_environment(self) -> [(&'static str, &'static str); 3] {
        let app_id = self.steam_app_id();
        [
            ("SteamAppId", app_id),
            ("SteamGameId", app_id),
            ("SteamOverlayGameId", app_id),
        ]
    }

    pub(crate) const fn relative_executable(self) -> &'static str {
        match self {
            Self::Cyberpunk2077SteamBuild20383525 => r"bin\x64\Cyberpunk2077.exe",
            Self::KingdomCome2SteamBuild23914554 => {
                r"Bin\Win64MasterMasterSteamPGO\KingdomCome.exe"
            }
        }
    }

    pub(crate) const fn executable_sha256(self) -> &'static str {
        match self {
            Self::Cyberpunk2077SteamBuild20383525 => {
                "sha256:a7de82945c03e041fc7339fcf9066224d98db2f5d80fea50f7947bb350a60991"
            }
            Self::KingdomCome2SteamBuild23914554 => {
                "sha256:d85b5355842d16290dc0d97e1d02856c6f2811a2021c8957aa24d7782a771b77"
            }
        }
    }

    /// Resolve only the explicit DEV/test allowlist. Delivered-catalog builds
    /// must not acquire an early-injection target merely from a catalog string.
    pub(crate) fn from_local_trial_game_id(game_id: &str) -> Option<Self> {
        #[cfg(any(feature = "dev-catalog", test))]
        return Self::ALL
            .iter()
            .copied()
            .find(|target| target.game_id() == game_id);
        #[cfg(not(any(feature = "dev-catalog", test)))]
        let _ = game_id;
        #[cfg(not(any(feature = "dev-catalog", test)))]
        None
    }

    /// Formal lookup is intentionally compiled for every `depth-lab` flavor,
    /// including Delivered Catalog. It identifies a known target only; the
    /// catalog pair and [`Self::verify_executable_identity`] decide whether a run
    /// may start.
    pub(crate) fn from_formal_game_id(game_id: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|target| target.game_id() == game_id)
    }

    pub(crate) fn verify_executable_identity(
        self,
        game_id: &str,
        executable_sha256: &str,
    ) -> Result<(), String> {
        if game_id != self.game_id()
            || !executable_sha256.eq_ignore_ascii_case(self.executable_sha256())
        {
            return Err(format!(
                "early-launch target identity mismatch: expected={}/{} actual={game_id}/{executable_sha256}",
                self.game_id(),
                self.executable_sha256()
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EarlyLauncherMode<'a> {
    FormalDepth,
    Discovery {
        config: &'a Path,
    },
    #[cfg(any(feature = "dev-catalog", test))]
    RawDepthDemo {
        config: &'a Path,
    },
}

impl<'a> EarlyLauncherMode<'a> {
    fn config_switch(self) -> Option<(&'static str, &'a Path)> {
        match self {
            Self::FormalDepth => None,
            Self::Discovery { config } => Some(("--discovery-config", config)),
            #[cfg(any(feature = "dev-catalog", test))]
            Self::RawDepthDemo { config } => Some(("--raw-depth-demo-config", config)),
        }
    }
}

pub(crate) struct EarlyLauncherRequest<'a> {
    pub(crate) target: EarlyLaunchTarget,
    pub(crate) executable: &'a Path,
    pub(crate) probe: &'a Path,
    pub(crate) probe_sha256: &'a str,
    pub(crate) mode: EarlyLauncherMode<'a>,
}

impl EarlyLauncherRequest<'_> {
    pub(crate) fn args(&self) -> Result<Vec<OsString>, String> {
        if !self.executable.is_absolute() || !self.probe.is_absolute() {
            return Err("early-launch executable and probe paths must be absolute".into());
        }
        if !is_sha256(self.probe_sha256) {
            return Err("early-launch probe SHA-256 is malformed".into());
        }
        let working_directory = self
            .executable
            .parent()
            .ok_or("early-launch executable has no working directory")?;
        let mut args = vec![
            OsString::from("--exe"),
            self.executable.as_os_str().to_owned(),
            OsString::from("--expected-exe-sha256"),
            OsString::from(self.target.executable_sha256()),
            OsString::from("--dll"),
            self.probe.as_os_str().to_owned(),
            OsString::from("--expected-dll-sha256"),
            OsString::from(self.probe_sha256),
            OsString::from("--working-directory"),
            working_directory.as_os_str().to_owned(),
        ];
        if let Some((switch, config)) = self.mode.config_switch() {
            if !config.is_absolute() {
                return Err("early-launch mode config path must be absolute".into());
            }
            args.push(OsString::from(switch));
            args.push(config.as_os_str().to_owned());
        }
        Ok(args)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EarlyLauncherOutput {
    pub(crate) process_id: u32,
    pub(crate) resident_probe_sha256: String,
    pub(crate) launch_id: String,
}

pub(crate) fn parse_early_launcher_output(
    stdout: &[u8],
    expected_probe_sha256: &str,
) -> Result<EarlyLauncherOutput, String> {
    if !is_sha256(expected_probe_sha256) {
        return Err("expected early-launch probe SHA-256 is malformed".into());
    }
    let stdout = std::str::from_utf8(stdout)
        .map_err(|_| "early launcher returned non-UTF8 status".to_string())?;
    let process_id = unique_value(stdout, "launched_pid=")?
        .parse::<u32>()
        .map_err(|_| "early launcher returned an invalid PID")?;
    if process_id == 0 {
        return Err("early launcher returned PID zero".into());
    }
    let resident_probe_sha256 = unique_value(stdout, "resident_probe_sha256=")?;
    if !resident_probe_sha256.eq_ignore_ascii_case(expected_probe_sha256) {
        return Err(format!(
            "resident probe hash mismatch: expected={expected_probe_sha256} actual={resident_probe_sha256}"
        ));
    }
    let launch_id = unique_value(stdout, "launch_id=")?;
    if !valid_launch_id(launch_id) {
        return Err("early launcher returned an invalid launch identity".into());
    }
    Ok(EarlyLauncherOutput {
        process_id,
        resident_probe_sha256: resident_probe_sha256.to_owned(),
        launch_id: launch_id.to_owned(),
    })
}

fn unique_value<'a>(stdout: &'a str, prefix: &str) -> Result<&'a str, String> {
    let mut values = stdout.lines().filter_map(|line| line.strip_prefix(prefix));
    let value = values
        .next()
        .ok_or_else(|| format!("early launcher did not report {prefix}"))?;
    if value.is_empty() || values.next().is_some() {
        return Err(format!(
            "early launcher reported invalid or duplicate {prefix}"
        ));
    }
    Ok(value)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_hash() -> String {
        format!("sha256:{}", "d".repeat(64))
    }

    #[test]
    fn allowlist_contains_the_exact_kcd2_and_cp2077_targets() {
        let target = EarlyLaunchTarget::from_local_trial_game_id("cyberpunk-2077-steam").unwrap();
        assert_eq!(target, EarlyLaunchTarget::cp2077());
        assert_eq!(target.steam_build_id(), "20383525");
        assert!(EarlyLaunchTarget::from_local_trial_game_id("unsupported-d3d12-game").is_none());
        assert!(EarlyLaunchTarget::from_formal_game_id("unsupported-d3d12-game").is_none());
        assert_eq!(
            EarlyLaunchTarget::from_formal_game_id(target.game_id()),
            Some(target)
        );
        assert!(
            target
                .verify_executable_identity(target.game_id(), &format!("sha256:{}", "0".repeat(64)))
                .is_err()
        );
        let kcd2 = EarlyLaunchTarget::from_local_trial_game_id("kingdom-come-deliverance-2-steam")
            .expect("KCD2 is an explicit local target");
        assert_eq!(kcd2, EarlyLaunchTarget::KingdomCome2SteamBuild23914554);
        assert_eq!(kcd2.steam_app_id(), "1771300");
        assert_eq!(kcd2.steam_build_id(), "23914554");
        assert_eq!(
            kcd2.steam_environment(),
            [
                ("SteamAppId", "1771300"),
                ("SteamGameId", "1771300"),
                ("SteamOverlayGameId", "1771300"),
            ]
        );
        assert_eq!(
            EarlyLaunchTarget::from_formal_game_id(kcd2.game_id()),
            Some(kcd2)
        );

        let mut game_ids: Vec<_> = EarlyLaunchTarget::ALL
            .iter()
            .map(|target| target.game_id())
            .collect();
        game_ids.sort_unstable();
        game_ids.dedup();
        assert_eq!(game_ids.len(), EarlyLaunchTarget::ALL.len());
        let mut app_build_hashes: Vec<_> = EarlyLaunchTarget::ALL
            .iter()
            .map(|target| {
                (
                    target.steam_app_id(),
                    target.steam_build_id(),
                    target.executable_sha256(),
                )
            })
            .collect();
        app_build_hashes.sort_unstable();
        app_build_hashes.dedup();
        assert_eq!(app_build_hashes.len(), EarlyLaunchTarget::ALL.len());
    }

    #[test]
    fn modes_build_structured_arguments_without_game_arguments() {
        let hash = probe_hash();
        for mode in [
            EarlyLauncherMode::FormalDepth,
            EarlyLauncherMode::Discovery {
                config: Path::new(r"C:\ProgramData\DataMiner\discovery.json"),
            },
            EarlyLauncherMode::RawDepthDemo {
                config: Path::new(r"C:\ProgramData\DataMiner\raw.json"),
            },
        ] {
            let request = EarlyLauncherRequest {
                target: EarlyLaunchTarget::cp2077(),
                executable: Path::new(r"C:\Games\Cyberpunk 2077\bin\x64\Cyberpunk2077.exe"),
                probe: Path::new(r"C:\Tools\dm_depth_probe.dll"),
                probe_sha256: &hash,
                mode,
            };
            let rendered: Vec<_> = request
                .args()
                .unwrap()
                .into_iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect();
            assert!(rendered.iter().any(|value| value == "--working-directory"));
            assert!(!rendered.iter().any(|value| value == "--args"));
        }
    }

    #[test]
    fn parser_requires_unique_pid_resident_hash_and_lowercase_launch_id() {
        let hash = probe_hash();
        let launch_id = "1".repeat(32);
        let output =
            format!("launched_pid=735\nresident_probe_sha256={hash}\nlaunch_id={launch_id}\n");
        assert_eq!(
            parse_early_launcher_output(output.as_bytes(), &hash),
            Ok(EarlyLauncherOutput {
                process_id: 735,
                resident_probe_sha256: hash.clone(),
                launch_id,
            })
        );
        let duplicate = format!(
            "launched_pid=735\nlaunched_pid=736\nresident_probe_sha256={hash}\nlaunch_id={}\n",
            "a".repeat(32)
        );
        assert!(parse_early_launcher_output(duplicate.as_bytes(), &hash).is_err());
        assert!(parse_early_launcher_output(b"launched_pid=0\n", &hash).is_err());
    }
}
