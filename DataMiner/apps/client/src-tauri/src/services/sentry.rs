use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use sentry::protocol::{Attachment, Event, Level};

use crate::services::diagnostics::DiagnosticExportResult;

/// Sentry's public client DSN. It identifies the fixed DataMiner project but
/// is not an administrative/API credential.
const SENTRY_DSN: &str = "https://a48462e1102e194c32a1c4ca08e3f050@api.magicneko.com:9043/21";
/// This project currently has one fixed desktop-client channel. Keep the
/// Sentry environment independent from Rust's optimization profile: unsigned
/// development installers are also compiled with `--release`.
const SENTRY_ENVIRONMENT: &str = "dataminer-client";

/// Keep the envelope comfortably below Sentry's compressed-request limit.
/// Diagnostic JSON is already bounded by event count, but event payload sizes
/// can vary with driver and Media Foundation errors.
pub const MAX_DIAGNOSTIC_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);
const _: () = assert!(MAX_DIAGNOSTIC_ATTACHMENT_BYTES < 20 * 1024 * 1024);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticAttachmentStatus {
    Attached,
    OmittedOversize,
    OmittedExportFailed,
    OmittedValidationFailed,
}

impl DiagnosticAttachmentStatus {
    fn tag(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::OmittedOversize => "omitted_oversize",
            Self::OmittedExportFailed => "omitted_export_failed",
            Self::OmittedValidationFailed => "omitted_validation_failed",
        }
    }

    pub fn outcome(self) -> &'static str {
        match self {
            Self::Attached => "drained_with_attachment",
            Self::OmittedOversize => "drained_without_oversize_attachment",
            Self::OmittedExportFailed => "drained_without_diagnostic_export",
            Self::OmittedValidationFailed => "drained_without_valid_attachment",
        }
    }
}

pub struct SentryReporter {
    client: Option<Arc<sentry::Client>>,
    active_reports: Arc<AtomicUsize>,
}

pub struct SentryReportReceipt {
    pub event_id: String,
    pub flushed: bool,
}

pub struct SentryRecordingFailureEvent<'a> {
    pub fault_code: &'a str,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub details_id: &'a str,
    pub diagnostic: Option<&'a DiagnosticExportResult>,
    pub attachment: Option<Vec<u8>>,
    pub attachment_status: DiagnosticAttachmentStatus,
}

/// Reserves one report before a recording state becomes terminal. Safe quit
/// observes the counter, and dropping the reservation marks the report done
/// even when spawning or exporting fails.
pub struct SentryReportReservation {
    active_reports: Arc<AtomicUsize>,
}

impl Drop for SentryReportReservation {
    fn drop(&mut self) {
        let previous = self.active_reports.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "Sentry report reservation underflowed");
    }
}

impl SentryReporter {
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            client: None,
            active_reports: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn for_dataminer(client_version: &str) -> anyhow::Result<Self> {
        let dsn = SENTRY_DSN
            .parse()
            .context("hard-coded Sentry DSN is invalid")?;
        let options = sentry::ClientOptions {
            dsn: Some(dsn),
            release: Some(format!("dataminer-client@{client_version}").into()),
            environment: Some(SENTRY_ENVIRONMENT.into()),
            send_default_pii: false,
            before_send: Some(Arc::new(strip_host_identity)),
            shutdown_timeout: FLUSH_TIMEOUT,
            ..sentry::ClientOptions::default()
        };
        Ok(Self {
            client: Some(Arc::new(sentry::Client::with_options(
                sentry::apply_defaults(options),
            ))),
            active_reports: Arc::new(AtomicUsize::new(0)),
        })
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.client
            .as_ref()
            .is_some_and(|client| client.is_enabled())
    }

    #[must_use]
    pub fn reserve_report(&self) -> Option<SentryReportReservation> {
        self.enabled().then(|| {
            self.active_reports.fetch_add(1, Ordering::AcqRel);
            SentryReportReservation {
                active_reports: Arc::clone(&self.active_reports),
            }
        })
    }

    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.active_reports.load(Ordering::Acquire) == 0
    }

    pub fn report_recording_failure(
        &self,
        event: SentryRecordingFailureEvent<'_>,
    ) -> anyhow::Result<SentryReportReceipt> {
        let SentryRecordingFailureEvent {
            fault_code,
            session_id,
            run_id,
            details_id,
            diagnostic,
            attachment,
            attachment_status,
        } = event;
        let client = self
            .client
            .as_ref()
            .context("Sentry reporter is disabled")?;
        ensure!(
            attachment
                .as_ref()
                .is_none_or(|bytes| bytes.len() <= MAX_DIAGNOSTIC_ATTACHMENT_BYTES),
            "diagnostic export is too large for Sentry attachment: {} bytes",
            attachment.as_ref().map_or(0, Vec::len)
        );
        ensure!(
            attachment.is_some() == (attachment_status == DiagnosticAttachmentStatus::Attached),
            "Sentry diagnostic attachment status does not match its payload"
        );
        ensure!(
            attachment.is_none() || diagnostic.is_some(),
            "Sentry diagnostic attachment has no export metadata"
        );
        let attachment_bytes = attachment.as_ref().map_or(0, Vec::len);
        let attachment_included = attachment_status == DiagnosticAttachmentStatus::Attached;

        let mut scope = sentry::Scope::default();
        scope.set_tag("component", "recording");
        scope.set_tag("fault_code", fault_code);
        scope.set_tag("session_id", session_id);
        scope.set_tag("run_id", run_id);
        scope.set_tag("details_id", details_id);
        scope.set_tag("diagnostic_attachment", attachment_status.tag());
        if let Some(diagnostic) = diagnostic {
            scope.set_tag("diagnostic_export_id", &diagnostic.export_id);
        }
        scope.set_fingerprint(Some(&["dataminer-recording-failure", fault_code]));
        if let Some(attachment) = attachment {
            let diagnostic = diagnostic.context("Sentry attachment export metadata is missing")?;
            scope.add_attachment(Attachment {
                buffer: attachment,
                filename: diagnostic.file_name.clone(),
                content_type: Some("application/json".into()),
                ty: None,
            });
        }

        let event = Event {
            level: Level::Error,
            message: Some(format!("DataMiner recording failed: {fault_code}")),
            ..Event::default()
        };
        let event_id = client.capture_event(event, Some(&scope));
        let flushed = client.flush(Some(FLUSH_TIMEOUT));
        if flushed {
            tracing::info!(
                %event_id,
                fault_code,
                export_id = diagnostic.map_or("unavailable", |value| value.export_id.as_str()),
                diagnostic_sha256 = diagnostic.map_or("unavailable", |value| value.sha256.as_str()),
                attachment_bytes,
                attachment_included,
                attachment_status = attachment_status.tag(),
                "drained Sentry recording-failure report"
            );
        } else {
            tracing::warn!(
                %event_id,
                fault_code,
                timeout_seconds = FLUSH_TIMEOUT.as_secs(),
                "Sentry transport did not drain before the best-effort timeout"
            );
        }
        Ok(SentryReportReceipt {
            event_id: event_id.to_string(),
            flushed,
        })
    }
}

fn strip_host_identity(mut event: Event<'static>) -> Option<Event<'static>> {
    // The diagnostic export already owns the upload allowlist. Do not let a
    // future SDK integration add machine identity beside that attachment.
    event.server_name = None;
    event.user = None;
    event.request = None;
    Some(event)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sentry::protocol::EnvelopeItem;
    use sentry::{ClientOptions, Envelope, Transport};

    use super::*;

    struct CapturingTransport {
        envelopes: Mutex<Vec<Envelope>>,
        flush_result: bool,
    }

    impl CapturingTransport {
        fn new(flush_result: bool) -> Arc<Self> {
            Arc::new(Self {
                envelopes: Mutex::new(Vec::new()),
                flush_result,
            })
        }
    }

    impl Transport for CapturingTransport {
        fn send_envelope(&self, envelope: Envelope) {
            self.envelopes.lock().unwrap().push(envelope);
        }

        fn flush(&self, _timeout: Duration) -> bool {
            self.flush_result
        }
    }

    fn reporter(transport: &Arc<CapturingTransport>) -> SentryReporter {
        let factory_transport = Arc::clone(transport);
        let options = ClientOptions {
            dsn: Some("https://public@sentry.invalid/1".parse().unwrap()),
            environment: Some(SENTRY_ENVIRONMENT.into()),
            send_default_pii: false,
            before_send: Some(Arc::new(strip_host_identity)),
            transport: Some(Arc::new(
                move |_options: &ClientOptions| -> Arc<dyn Transport> {
                    Arc::clone(&factory_transport) as Arc<dyn Transport>
                },
            )),
            ..ClientOptions::default()
        };
        SentryReporter {
            client: Some(Arc::new(sentry::Client::with_options(options))),
            active_reports: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn diagnostic() -> DiagnosticExportResult {
        DiagnosticExportResult {
            export_id: "export-1".into(),
            file_name: "diagnostic.json".into(),
            sha256: "abc123".into(),
            event_count: 3,
        }
    }

    #[test]
    fn event_and_attachment_share_one_sanitized_envelope() {
        let transport = CapturingTransport::new(true);
        let event_id = reporter(&transport)
            .report_recording_failure(SentryRecordingFailureEvent {
                fault_code: "ENCODER_PIPELINE_FAILED",
                session_id: "dev-123-0000",
                run_id: "r0000",
                details_id: "details-123-0000",
                diagnostic: Some(&diagnostic()),
                attachment: Some(br#"{"schema":"dm-diagnostic-export-1"}"#.to_vec()),
                attachment_status: DiagnosticAttachmentStatus::Attached,
            })
            .unwrap();
        assert!(!event_id.event_id.is_empty());
        assert!(event_id.flushed);

        let envelopes = transport.envelopes.lock().unwrap();
        assert_eq!(envelopes.len(), 1);
        let envelope = &envelopes[0];
        let event = envelope.event().unwrap();
        assert_eq!(event.server_name, None);
        assert_eq!(event.user, None);
        assert_eq!(event.request, None);
        assert_eq!(
            event.tags.get("diagnostic_attachment").map(String::as_str),
            Some("attached")
        );
        assert_eq!(
            event.tags.get("session_id").map(String::as_str),
            Some("dev-123-0000")
        );
        assert_eq!(event.tags.get("run_id").map(String::as_str), Some("r0000"));
        assert_eq!(
            event.tags.get("details_id").map(String::as_str),
            Some("details-123-0000")
        );
        assert_eq!(
            event.tags.get("diagnostic_export_id").map(String::as_str),
            Some("export-1")
        );
        let attachment = envelope
            .items()
            .find_map(|item| match item {
                EnvelopeItem::Attachment(attachment) => Some(attachment),
                _ => None,
            })
            .unwrap();
        assert_eq!(attachment.filename, "diagnostic.json");
        assert_eq!(attachment.content_type.as_deref(), Some("application/json"));
        assert_eq!(
            attachment.buffer,
            br#"{"schema":"dm-diagnostic-export-1"}"#.to_vec()
        );
    }

    #[test]
    fn event_without_oversize_attachment_is_still_sent() {
        let transport = CapturingTransport::new(true);
        reporter(&transport)
            .report_recording_failure(SentryRecordingFailureEvent {
                fault_code: "CAPTURE_SOURCE_FAILED",
                session_id: "dev-123-0000",
                run_id: "r0000",
                details_id: "details-123-0000",
                diagnostic: Some(&diagnostic()),
                attachment: None,
                attachment_status: DiagnosticAttachmentStatus::OmittedOversize,
            })
            .unwrap();

        let envelopes = transport.envelopes.lock().unwrap();
        assert_eq!(envelopes.len(), 1);
        let event = envelopes[0].event().unwrap();
        assert_eq!(
            event.tags.get("diagnostic_attachment").map(String::as_str),
            Some("omitted_oversize")
        );
        assert!(
            envelopes[0]
                .items()
                .all(|item| !matches!(item, EnvelopeItem::Attachment(_)))
        );
    }

    #[test]
    fn event_is_sent_when_diagnostic_export_is_unavailable() {
        let transport = CapturingTransport::new(true);
        reporter(&transport)
            .report_recording_failure(SentryRecordingFailureEvent {
                fault_code: "JOURNAL_COMMIT_FAILED",
                session_id: "dev-123-0000",
                run_id: "r0000",
                details_id: "details-123-0000",
                diagnostic: None,
                attachment: None,
                attachment_status: DiagnosticAttachmentStatus::OmittedExportFailed,
            })
            .unwrap();

        let envelopes = transport.envelopes.lock().unwrap();
        assert_eq!(envelopes.len(), 1);
        let event = envelopes[0].event().unwrap();
        assert_eq!(
            event.tags.get("diagnostic_attachment").map(String::as_str),
            Some("omitted_export_failed")
        );
    }

    #[test]
    fn flush_timeout_preserves_the_event_id_for_local_correlation() {
        let transport = CapturingTransport::new(false);
        let receipt = reporter(&transport)
            .report_recording_failure(SentryRecordingFailureEvent {
                fault_code: "CAPTURE_SOURCE_FAILED",
                session_id: "dev-123-0000",
                run_id: "r0000",
                details_id: "details-123-0000",
                diagnostic: None,
                attachment: None,
                attachment_status: DiagnosticAttachmentStatus::OmittedExportFailed,
            })
            .unwrap();

        assert!(!receipt.event_id.is_empty());
        assert!(!receipt.flushed);
    }

    #[test]
    fn privacy_filter_removes_identity_added_outside_the_export() {
        let event = Event {
            server_name: Some("alice-laptop".into()),
            user: Some(Default::default()),
            request: Some(Default::default()),
            ..Event::default()
        };

        let event = strip_host_identity(event).unwrap();
        assert_eq!(event.server_name, None);
        assert_eq!(event.user, None);
        assert_eq!(event.request, None);
    }

    #[test]
    fn report_reservation_tracks_quiescence() {
        let transport = CapturingTransport::new(true);
        let reporter = reporter(&transport);
        assert!(reporter.is_quiescent());
        let reservation = reporter.reserve_report().unwrap();
        assert!(!reporter.is_quiescent());
        drop(reservation);
        assert!(reporter.is_quiescent());
    }
}
