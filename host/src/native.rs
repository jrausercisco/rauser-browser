//! Chrome native messaging uses a four-byte, native-endian length prefix
//! followed by one UTF-8 JSON value. Stdout contains frames only.

use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use brauser_protocol::{
    ConfigConfirmed, ConfigResult, ConfigUpdated, ErrorCode, ErrorResponse, FolderChosen,
    HelloResult, NoteConflict, NoteLoaded, NoteSaved, PROTOCOL_VERSION, Request, Response,
    VisitOutcome, VisitRecorded,
};

use crate::capture::{CaptureOutcome, CapturePolicy, CaptureStore};
use crate::config::{self, ConfigStore};
use crate::consent::ConsentAuthority;
use crate::note;
use crate::vault::Vault;

const MAX_INBOUND_BYTES: usize = 4 * 1024 * 1024;
// Chrome limits host-to-extension messages to 1 MiB. Leave room for the
// framing prefix and future envelope fields.
const MAX_OUTBOUND_BYTES: usize = 900 * 1024;
const MAX_REQUEST_ID_CHARS: usize = 128;

pub fn serve(config: ConfigStore) -> Result<()> {
    let input = io::stdin();
    let output = io::stdout();
    serve_with_io(input.lock(), output.lock(), config)
}

/// Process requests until the browser closes the native messaging pipe.
pub fn serve_with_io<R: Read, W: Write>(
    mut input: R,
    mut output: W,
    mut config: ConfigStore,
) -> Result<()> {
    let mut consent = ConsentAuthority::new();
    loop {
        let body = match read_frame(&mut input) {
            Ok(Some(body)) => body,
            Ok(None) => return Ok(()),
            Err(FrameError::TooLarge) => {
                write_frame(
                    &mut output,
                    &error(
                        "",
                        ErrorCode::MessageTooLarge,
                        "native message exceeds the 4 MiB inbound limit",
                    ),
                )?;
                bail!("oversized native message; connection closed");
            }
            Err(FrameError::InvalidLength) => {
                write_frame(
                    &mut output,
                    &error(
                        "",
                        ErrorCode::InvalidRequest,
                        "native message has an empty body",
                    ),
                )?;
                continue;
            }
            Err(FrameError::Io(error)) => return Err(error).context("reading native message"),
        };
        let response = dispatch_json(&body, &mut config, &mut consent);
        write_frame(&mut output, &response)?;
    }
}

/// Read the common envelope before the version-specific request. A newer
/// extension may add fields or message types this host cannot deserialize.
fn dispatch_json(
    body: &[u8],
    config: &mut ConfigStore,
    consent: &mut ConsentAuthority,
) -> Response {
    let envelope: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return error("", ErrorCode::InvalidRequest, "invalid request JSON"),
    };
    let Some(object) = envelope.as_object() else {
        return error(
            "",
            ErrorCode::InvalidRequest,
            "request must be a JSON object",
        );
    };
    let Some(id) = object.get("request_id").and_then(serde_json::Value::as_str) else {
        return error("", ErrorCode::InvalidRequest, "request_id is required");
    };
    if !valid_request_id(id) {
        return error(
            "",
            ErrorCode::InvalidRequest,
            "request_id must be 1–128 non-control characters",
        );
    }
    let Some(version) = object
        .get("protocol_version")
        .and_then(serde_json::Value::as_u64)
    else {
        return error(
            id,
            ErrorCode::InvalidRequest,
            "protocol_version is required",
        );
    };
    if version != u64::from(PROTOCOL_VERSION) {
        return error(
            id,
            ErrorCode::UnsupportedProtocolVersion,
            "extension and host protocol versions differ",
        );
    }
    match serde_json::from_slice::<Request>(body) {
        Ok(request) => dispatch(request, config, consent),
        Err(_) => error(
            id,
            ErrorCode::InvalidRequest,
            "invalid or unsupported request JSON",
        ),
    }
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_REQUEST_ID_CHARS
        && !id.chars().any(char::is_control)
}

fn dispatch(
    request: Request,
    config: &mut ConfigStore,
    consent: &mut ConsentAuthority,
) -> Response {
    let id = request.request_id();
    if !valid_request_id(id) {
        return error(
            "",
            ErrorCode::InvalidRequest,
            "request_id must be 1–128 non-control characters",
        );
    }
    if request.protocol_version() != PROTOCOL_VERSION {
        return error(
            id,
            ErrorCode::UnsupportedProtocolVersion,
            "extension and host protocol versions differ",
        );
    }
    match request {
        Request::Hello(value) => match config.refresh() {
            Ok(()) => Response::HelloResult(HelloResult {
                protocol_version: PROTOCOL_VERSION,
                request_id: value.request_id,
                host_version: env!("CARGO_PKG_VERSION").to_owned(),
                configured: config.configured(),
                config_issue: config.config_issue().map(str::to_owned),
            }),
            Err(_) => error(
                &value.request_id,
                ErrorCode::InvalidConfig,
                "configuration file is invalid or unavailable",
            ),
        },
        Request::GetConfig(value) => match config.refresh() {
            Ok(()) => Response::ConfigResult(ConfigResult {
                protocol_version: PROTOCOL_VERSION,
                request_id: value.request_id,
                config: config.snapshot().clone(),
                revision: config.revision().to_owned(),
                config_issue: config.config_issue().map(str::to_owned),
            }),
            Err(_) => error(
                &value.request_id,
                ErrorCode::InvalidConfig,
                "configuration file is invalid or unavailable",
            ),
        },
        Request::UpdateConfig(value) => {
            if config.refresh().is_err() {
                return error(
                    &value.request_id,
                    ErrorCode::InvalidConfig,
                    "configuration file is unavailable",
                );
            }
            if value.expected_revision != config.revision() {
                return error(
                    &value.request_id,
                    ErrorCode::Conflict,
                    "configuration changed; read it again before updating",
                );
            }
            if let Err(validation_error) = config::validate(&value.config) {
                return error(
                    &value.request_id,
                    ErrorCode::InvalidConfig,
                    &validation_error.to_string(),
                );
            }
            let previous = config.snapshot().clone();
            let previous_revision = config.revision().to_owned();
            let selected_identity = match consent.authorize_update(
                &previous,
                &previous_revision,
                &value.config,
                value.picker_token.as_deref(),
                value.consent_token.as_deref(),
            ) {
                Ok(identity) => identity,
                Err(authority_error) => {
                    return error(
                        &value.request_id,
                        ErrorCode::Unauthorized,
                        &authority_error.to_string(),
                    );
                }
            };
            match config.update_with_root_identity(
                value.config.clone(),
                &value.expected_revision,
                selected_identity.as_deref(),
            ) {
                Ok(Some(revision)) => {
                    consent.consume_update(
                        &previous,
                        &previous_revision,
                        &value.config,
                        value.picker_token.as_deref(),
                        value.consent_token.as_deref(),
                    );
                    Response::ConfigUpdated(ConfigUpdated {
                        protocol_version: PROTOCOL_VERSION,
                        request_id: value.request_id,
                        config: value.config,
                        revision,
                    })
                }
                Ok(None) => error(
                    &value.request_id,
                    ErrorCode::Conflict,
                    "configuration changed; read it again before updating",
                ),
                Err(_) => error(
                    &value.request_id,
                    ErrorCode::Internal,
                    "could not save configuration",
                ),
            }
        }
        Request::ChooseFolder(value) => {
            if config.refresh().is_err() {
                return error(
                    &value.request_id,
                    ErrorCode::InvalidConfig,
                    "configuration file is unavailable",
                );
            }
            match consent.choose_folder(config.revision()) {
                Ok(Some(chosen)) => Response::FolderChosen(FolderChosen {
                    protocol_version: PROTOCOL_VERSION,
                    request_id: value.request_id,
                    path: chosen.path,
                    picker_token: chosen.picker_token,
                }),
                Ok(None) => error(
                    &value.request_id,
                    ErrorCode::Cancelled,
                    "folder selection was canceled",
                ),
                Err(_) => error(
                    &value.request_id,
                    ErrorCode::Internal,
                    "could not open the folder picker",
                ),
            }
        }
        Request::ConfirmConfig(value) => {
            if config.refresh().is_err() {
                return error(
                    &value.request_id,
                    ErrorCode::InvalidConfig,
                    "configuration file is unavailable",
                );
            }
            if value.expected_revision != config.revision() {
                return error(
                    &value.request_id,
                    ErrorCode::Conflict,
                    "configuration changed; read it again before confirming",
                );
            }
            if let Err(validation_error) = config::validate(&value.config) {
                return error(
                    &value.request_id,
                    ErrorCode::InvalidConfig,
                    &validation_error.to_string(),
                );
            }
            match consent.confirm_config(
                config.snapshot(),
                config.revision(),
                &value.config,
                value.picker_token.as_deref(),
            ) {
                Ok(Some(confirmed)) => Response::ConfigConfirmed(ConfigConfirmed {
                    protocol_version: PROTOCOL_VERSION,
                    request_id: value.request_id,
                    consent_token: confirmed.consent_token,
                    summary: confirmed.summary,
                }),
                Ok(None) => error(
                    &value.request_id,
                    ErrorCode::Cancelled,
                    "configuration confirmation was canceled",
                ),
                Err(authority_error) => error(
                    &value.request_id,
                    ErrorCode::Unauthorized,
                    &authority_error.to_string(),
                ),
            }
        }
        Request::RecordVisit(value) => record_visit(value, config),
        Request::LoadNote(value) => load_note(value, config),
        Request::SaveNote(value) => save_note(value, config),
    }
}

fn record_visit(value: brauser_protocol::RecordVisitRequest, config: &mut ConfigStore) -> Response {
    let event_id = value.event.event_id.clone();
    if event_id.len() > 64 {
        return error(
            &value.request_id,
            ErrorCode::InvalidRequest,
            "event_id is too long",
        );
    }
    let retryable = |reason: &str| {
        Response::VisitRecorded(VisitRecorded {
            protocol_version: PROTOCOL_VERSION,
            request_id: value.request_id.clone(),
            event_id: event_id.clone(),
            outcome: VisitOutcome::Retryable,
            reason: Some(reason.to_owned()),
            relative_path: None,
        })
    };
    // Serialize with all host config updates, then re-read policy while the
    // lock is held. A revoked site cannot be appended after revocation commits.
    let _config_lock = match config.lock_current() {
        Ok(lock) => lock,
        Err(_) => return retryable("configuration is unavailable"),
    };
    if config.refresh().is_err() || config.config_issue().is_some() {
        return retryable("configuration is unavailable or needs repair");
    }
    if !config.snapshot().capture_enabled {
        return Response::VisitRecorded(VisitRecorded {
            protocol_version: PROTOCOL_VERSION,
            request_id: value.request_id,
            event_id,
            outcome: VisitOutcome::Suppressed,
            reason: Some("capture_disabled".into()),
            relative_path: None,
        });
    }
    let Some(storage) = config.snapshot().storage.as_ref() else {
        return retryable("choose a notes folder before capture");
    };
    let store = match CaptureStore::open_checked(storage, config.root_identity()) {
        Ok(store) => store,
        Err(_) => return retryable("notes folder is unavailable"),
    };
    let policy = CapturePolicy {
        enabled: config.snapshot().capture_enabled,
        sites: &config.snapshot().sites,
        strip_params: &config.snapshot().strip_params,
        near_repeat_secs: u64::from(config.snapshot().near_repeat_secs),
    };
    let (outcome, reason, relative_path) = match store.record(policy, &value.event) {
        CaptureOutcome::Persisted { relative_path } => (
            VisitOutcome::Persisted,
            None,
            Some(relative_path.to_string_lossy().into_owned()),
        ),
        CaptureOutcome::Suppressed { reason } => (VisitOutcome::Suppressed, Some(reason), None),
        CaptureOutcome::Rejected { reason } => (VisitOutcome::Rejected, Some(reason), None),
        CaptureOutcome::Retryable { message } => (VisitOutcome::Retryable, Some(message), None),
    };
    Response::VisitRecorded(VisitRecorded {
        protocol_version: PROTOCOL_VERSION,
        request_id: value.request_id,
        event_id,
        outcome,
        reason,
        relative_path,
    })
}

/// Resolve the canonical URL and vault shared by `load_note` and `save_note`.
/// Notes work on any HTTP(S) page (§5.3): unlike visit logging, this never
/// checks the site allowlist. The error is a small `(code, message)` pair,
/// not a `Response`, so callers pay for the large `Response` enum only once.
fn note_vault(
    config: &mut ConfigStore,
    url: &str,
) -> Result<(String, Vault), (ErrorCode, &'static str)> {
    if config.refresh().is_err() || config.config_issue().is_some() {
        return Err((
            ErrorCode::InvalidConfig,
            "configuration is unavailable or needs repair",
        ));
    }
    let Some(storage) = config.snapshot().storage.as_ref() else {
        return Err((ErrorCode::NotConfigured, "choose a notes folder first"));
    };
    let canonical = crate::capture::canonical_url(url, &config.snapshot().strip_params)
        .map_err(|_| (ErrorCode::InvalidRequest, "page URL is invalid"))?;
    let vault = Vault::open_checked(storage, config.root_identity())
        .map_err(|_| (ErrorCode::Internal, "notes folder is unavailable"))?;
    Ok((canonical, vault))
}

fn note_error(request_id: &str, error_value: note::NoteRequestError) -> Response {
    match error_value {
        note::NoteRequestError::Invalid(message) => {
            error(request_id, ErrorCode::InvalidRequest, &message)
        }
        note::NoteRequestError::Conflict(note::OwnershipConflict(message)) => {
            error(request_id, ErrorCode::Conflict, &message)
        }
        note::NoteRequestError::Internal(_) => error(
            request_id,
            ErrorCode::Internal,
            "could not access this page's note",
        ),
    }
}

fn load_note(value: brauser_protocol::LoadNoteRequest, config: &mut ConfigStore) -> Response {
    let (canonical, vault) = match note_vault(config, &value.url) {
        Ok(resolved) => resolved,
        Err((code, message)) => return error(&value.request_id, code, message),
    };
    match note::load_note(&vault, &canonical) {
        Ok(loaded) => Response::NoteLoaded(NoteLoaded {
            protocol_version: PROTOCOL_VERSION,
            request_id: value.request_id,
            exists: loaded.exists,
            revision: loaded.revision,
            title: loaded.title,
            body: loaded.body,
        }),
        Err(note_error_value) => note_error(&value.request_id, note_error_value),
    }
}

fn save_note(value: brauser_protocol::SaveNoteRequest, config: &mut ConfigStore) -> Response {
    // Serialize with config updates and other note saves so the version check
    // below and the write it guards happen without a concurrent writer.
    let _config_lock = match config.lock_current() {
        Ok(lock) => lock,
        Err(_) => {
            return error(
                &value.request_id,
                ErrorCode::Internal,
                "configuration is unavailable",
            );
        }
    };
    let (canonical, vault) = match note_vault(config, &value.url) {
        Ok(resolved) => resolved,
        Err((code, message)) => return error(&value.request_id, code, message),
    };
    match note::save_note(
        &vault,
        &canonical,
        &value.title,
        &value.body,
        &value.expected_revision,
    ) {
        Ok(note::SaveOutcome::Saved {
            outcome,
            revision,
            relative_path,
        }) => Response::NoteSaved(NoteSaved {
            protocol_version: PROTOCOL_VERSION,
            request_id: value.request_id,
            outcome,
            revision,
            relative_path,
        }),
        Ok(note::SaveOutcome::Stale {
            revision,
            exists,
            title,
            body,
        }) => Response::NoteConflict(NoteConflict {
            protocol_version: PROTOCOL_VERSION,
            request_id: value.request_id,
            exists,
            revision,
            title,
            body,
        }),
        Err(note_error_value) => note_error(&value.request_id, note_error_value),
    }
}

fn error(request_id: &str, code: ErrorCode, message: &str) -> Response {
    Response::Error(ErrorResponse {
        protocol_version: PROTOCOL_VERSION,
        request_id: request_id.to_owned(),
        code,
        message: message.to_owned(),
    })
}

#[derive(Debug)]
enum FrameError {
    TooLarge,
    InvalidLength,
    Io(io::Error),
}

fn read_frame<R: Read>(input: &mut R) -> std::result::Result<Option<Vec<u8>>, FrameError> {
    let mut prefix = [0u8; 4];
    let received = input.read(&mut prefix[..1]).map_err(FrameError::Io)?;
    if received == 0 {
        return Ok(None);
    }
    input.read_exact(&mut prefix[1..]).map_err(FrameError::Io)?;
    let size = u32::from_ne_bytes(prefix) as usize;
    if size == 0 {
        return Err(FrameError::InvalidLength);
    }
    if size > MAX_INBOUND_BYTES {
        return Err(FrameError::TooLarge);
    }
    let mut body = vec![0u8; size];
    input.read_exact(&mut body).map_err(FrameError::Io)?;
    Ok(Some(body))
}

fn write_frame<W: Write>(output: &mut W, response: &Response) -> Result<()> {
    let body = serde_json::to_vec(response).context("serializing native response")?;
    if body.len() > MAX_OUTBOUND_BYTES {
        bail!("native response exceeds host-to-extension size limit");
    }
    let size = u32::try_from(body.len()).context("native response length overflow")?;
    output.write_all(&size.to_ne_bytes())?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use brauser_protocol::{ConfigSnapshot, GetConfigRequest, HelloRequest, UpdateConfigRequest};
    use std::io::Cursor;

    fn encoded_request(request: Request) -> Vec<u8> {
        let json = serde_json::to_vec(&request).unwrap();
        encoded_json(&json)
    }

    fn encoded_json(json: &[u8]) -> Vec<u8> {
        let mut wire = (json.len() as u32).to_ne_bytes().to_vec();
        wire.extend_from_slice(json);
        wire
    }

    fn decoded_response(wire: Vec<u8>) -> Response {
        let mut input = Cursor::new(wire);
        let json = read_frame(&mut input).unwrap().unwrap();
        serde_json::from_slice(&json).unwrap()
    }

    #[test]
    fn config_revision_round_trips_and_rejects_a_stale_update() {
        let folder = tempfile::tempdir().unwrap();
        let config = ConfigStore::for_test(folder.path().join("config.toml"));
        let snapshot = ConfigSnapshot {
            storage: None,
            capture_enabled: false,
            sites: Vec::new(),
            strip_params: vec!["utm_*".into(), "fbclid".into(), "gclid".into()],
            near_repeat_secs: 300,
        };
        let requests = [
            Request::GetConfig(GetConfigRequest {
                protocol_version: PROTOCOL_VERSION,
                request_id: "read-1".into(),
            }),
            Request::UpdateConfig(UpdateConfigRequest {
                protocol_version: PROTOCOL_VERSION,
                request_id: "write-1".into(),
                expected_revision: "missing".into(),
                config: snapshot.clone(),
                picker_token: None,
                consent_token: None,
            }),
            Request::GetConfig(GetConfigRequest {
                protocol_version: PROTOCOL_VERSION,
                request_id: "read-2".into(),
            }),
            Request::UpdateConfig(UpdateConfigRequest {
                protocol_version: PROTOCOL_VERSION,
                request_id: "write-stale".into(),
                expected_revision: "missing".into(),
                config: snapshot,
                picker_token: None,
                consent_token: None,
            }),
        ];
        let input: Vec<u8> = requests.into_iter().flat_map(encoded_request).collect();
        let mut output = Vec::new();
        serve_with_io(Cursor::new(input), &mut output, config).unwrap();

        let mut reader = Cursor::new(output);
        let mut responses = Vec::new();
        while let Some(body) = read_frame(&mut reader).unwrap() {
            responses.push(serde_json::from_slice::<Response>(&body).unwrap());
        }
        assert_eq!(responses.len(), 4);
        match &responses[0] {
            Response::ConfigResult(value) => assert_eq!(value.revision, "missing"),
            other => panic!("expected initial config, got {other:?}"),
        }
        let revision = match &responses[1] {
            Response::ConfigUpdated(value) => value.revision.clone(),
            other => panic!("expected config update, got {other:?}"),
        };
        assert!(revision.starts_with("sha256:"));
        match &responses[2] {
            Response::ConfigResult(value) => assert_eq!(value.revision, revision),
            other => panic!("expected saved config, got {other:?}"),
        }
        match &responses[3] {
            Response::Error(value) => {
                assert_eq!(value.code, ErrorCode::Conflict);
                assert_eq!(value.request_id, "write-stale");
            }
            other => panic!("expected stale-update conflict, got {other:?}"),
        }
    }

    #[test]
    fn mismatched_protocol_returns_a_correlated_error() {
        let folder = tempfile::tempdir().unwrap();
        let config = ConfigStore::for_test(folder.path().join("config.toml"));
        let request = Request::Hello(HelloRequest {
            protocol_version: PROTOCOL_VERSION + 1,
            request_id: "request-1".into(),
        });
        let mut output = Vec::new();
        serve_with_io(Cursor::new(encoded_request(request)), &mut output, config).unwrap();
        match decoded_response(output) {
            Response::Error(value) => {
                assert_eq!(value.code, ErrorCode::UnsupportedProtocolVersion);
                assert_eq!(value.request_id, "request-1");
            }
            other => panic!("expected protocol error, got {other:?}"),
        }
    }

    #[test]
    fn future_message_type_reports_version_before_shape_error() {
        let folder = tempfile::tempdir().unwrap();
        let config = ConfigStore::for_test(folder.path().join("config.toml"));
        let request = format!(
            r#"{{"type":"future_operation","protocol_version":{},"request_id":"future-1","new_field":true}}"#,
            PROTOCOL_VERSION + 1
        );
        let mut output = Vec::new();
        serve_with_io(
            Cursor::new(encoded_json(request.as_bytes())),
            &mut output,
            config,
        )
        .unwrap();
        match decoded_response(output) {
            Response::Error(value) => {
                assert_eq!(value.code, ErrorCode::UnsupportedProtocolVersion);
                assert_eq!(value.request_id, "future-1");
            }
            other => panic!("expected version error, got {other:?}"),
        }
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocating_its_body() {
        let folder = tempfile::tempdir().unwrap();
        let config = ConfigStore::for_test(folder.path().join("config.toml"));
        let prefix = ((MAX_INBOUND_BYTES + 1) as u32).to_ne_bytes();
        let mut output = Vec::new();
        assert!(serve_with_io(Cursor::new(prefix), &mut output, config).is_err());
        match decoded_response(output) {
            Response::Error(value) => assert_eq!(value.code, ErrorCode::MessageTooLarge),
            other => panic!("expected size error, got {other:?}"),
        }
    }

    fn configured_store(root: &std::path::Path) -> ConfigStore {
        let mut store = ConfigStore::for_test(root.join("config.toml"));
        let snapshot = brauser_protocol::ConfigSnapshot {
            storage: Some(brauser_protocol::StorageConfig {
                root: root.to_string_lossy().into_owned(),
                profile: "neutral".into(),
                log_dir: "log".into(),
                pages_dir: "pages".into(),
                later_dir: "later".into(),
            }),
            capture_enabled: false,
            sites: Vec::new(),
            strip_params: Vec::new(),
            near_repeat_secs: 300,
        };
        let identity = crate::vault::selected_root_identity(root).unwrap();
        store
            .update_with_root_identity(snapshot, "missing", Some(&identity))
            .unwrap()
            .unwrap();
        store
    }

    #[test]
    fn loading_a_note_works_on_any_http_page_without_site_authorization() {
        let folder = tempfile::tempdir().unwrap();
        let notes = folder.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        let mut config = configured_store(&notes);
        // No site is enabled for logging; notes must still work (§5.3).
        let response = dispatch(
            Request::LoadNote(brauser_protocol::LoadNoteRequest {
                protocol_version: PROTOCOL_VERSION,
                request_id: "load-1".into(),
                url: "https://example.com/a".into(),
            }),
            &mut config,
            &mut ConsentAuthority::new(),
        );
        match response {
            Response::NoteLoaded(value) => {
                assert!(!value.exists);
                assert_eq!(value.revision, "missing");
            }
            other => panic!("expected note_loaded, got {other:?}"),
        }
    }

    #[test]
    fn saving_a_note_then_reusing_a_stale_revision_returns_the_current_note() {
        let folder = tempfile::tempdir().unwrap();
        let notes = folder.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        let mut config = configured_store(&notes);
        let mut consent = ConsentAuthority::new();
        let save = |config: &mut ConfigStore,
                    consent: &mut ConsentAuthority,
                    body: &str,
                    revision: &str| {
            dispatch(
                Request::SaveNote(brauser_protocol::SaveNoteRequest {
                    protocol_version: PROTOCOL_VERSION,
                    request_id: "save-1".into(),
                    url: "https://example.com/a".into(),
                    title: "A Title".into(),
                    body: body.into(),
                    expected_revision: revision.into(),
                }),
                config,
                consent,
            )
        };
        let first = match save(&mut config, &mut consent, "hello", "missing") {
            Response::NoteSaved(value) => value,
            other => panic!("expected note_saved, got {other:?}"),
        };
        assert!(matches!(
            first.outcome,
            brauser_protocol::NoteSaveOutcome::Created
        ));

        // Reusing the old (now stale) revision must be refused, not clobber
        // the note that was just saved.
        match save(&mut config, &mut consent, "goodbye", "missing") {
            Response::NoteConflict(value) => {
                assert!(value.exists);
                assert_eq!(value.body, "hello");
                assert_eq!(value.revision, first.revision);
            }
            other => panic!("expected note_conflict, got {other:?}"),
        }

        // The correct revision replaces the note whole.
        match save(&mut config, &mut consent, "goodbye", &first.revision) {
            Response::NoteSaved(value) => {
                assert!(matches!(
                    value.outcome,
                    brauser_protocol::NoteSaveOutcome::Replaced
                ));
                assert_ne!(value.revision, first.revision);
            }
            other => panic!("expected note_saved, got {other:?}"),
        }
    }
}
