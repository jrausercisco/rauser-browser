//! Chrome native messaging uses a four-byte, native-endian length prefix
//! followed by one UTF-8 JSON value. Stdout contains frames only.

use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use brauser_protocol::{
    AgentChecked, ConfigConfirmed, ConfigResult, ConfigUpdated, ErrorCode, ErrorResponse,
    FolderChosen, HarnessSetupConfirmed, HarnessesDiscovered, HelloResult, NoteConflict,
    NoteLoaded, NoteSaved, PROTOCOL_VERSION, Request, Response, VisitOutcome, VisitRecorded,
};

use crate::brand::NAMESPACE;
use crate::capture::{CaptureOutcome, CapturePolicy, CaptureStore};
use crate::config::{self, ConfigStore};
use crate::consent::ConsentAuthority;
use crate::dialog::{self, DialogText};
use crate::harness::HarnessEnv;
use crate::note;
use crate::privacy;
use crate::vault::Vault;

const MAX_INBOUND_BYTES: usize = 4 * 1024 * 1024;
// Chrome limits host-to-extension messages to 1 MiB. Leave room for the
// framing prefix and future envelope fields. Note limits (`note.rs`) are
// derived from this so a saved note can always be loaded again.
pub(crate) const MAX_OUTBOUND_BYTES: usize = 900 * 1024;
const MAX_REQUEST_ID_CHARS: usize = 128;

pub fn serve(config: ConfigStore) -> Result<()> {
    let input = io::stdin();
    let output = io::stdout();
    serve_with_io(input.lock(), output.lock(), config)
}

/// What harness setup may use from the host process. Built once per
/// connection; tests build it by hand with a fake search path and dialog.
pub struct SetupContext<'a> {
    pub env: &'a HarnessEnv,
    pub confirm: fn(DialogText<'_>) -> Result<bool>,
}

/// Process requests until the browser closes the native messaging pipe.
pub fn serve_with_io<R: Read, W: Write>(input: R, output: W, config: ConfigStore) -> Result<()> {
    let env = HarnessEnv::from_process(config.dir()?);
    let context = SetupContext {
        env: &env,
        confirm: dialog::confirm,
    };
    serve_with_context(input, output, config, &context)
}

pub fn serve_with_context<R: Read, W: Write>(
    mut input: R,
    mut output: W,
    mut config: ConfigStore,
    context: &SetupContext<'_>,
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
        let response = dispatch_json(&body, &mut config, &mut consent, context);
        write_frame(&mut output, &response)?;
    }
}

/// Read the common envelope before the version-specific request. A newer
/// extension may add fields or message types this host cannot deserialize.
fn dispatch_json(
    body: &[u8],
    config: &mut ConfigStore,
    consent: &mut ConsentAuthority,
    context: &SetupContext<'_>,
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
        Ok(request) => dispatch(request, config, consent, context),
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
    context: &SetupContext<'_>,
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
        // An unreadable config file leaves the store inert with an issue.
        // Report that state instead of an error: the extension treats a
        // failed hello as a missing host, and the issue says how to repair.
        Request::Hello(value) => {
            let _ = config.refresh();
            Response::HelloResult(HelloResult {
                protocol_version: PROTOCOL_VERSION,
                request_id: value.request_id,
                host_version: env!("CARGO_PKG_VERSION").to_owned(),
                configured: config.configured(),
                config_issue: config.config_issue().map(str::to_owned),
            })
        }
        Request::GetConfig(value) => {
            let _ = config.refresh();
            Response::ConfigResult(ConfigResult {
                protocol_version: PROTOCOL_VERSION,
                request_id: value.request_id,
                config: config.snapshot().clone(),
                revision: config.revision().to_owned(),
                config_issue: config.config_issue().map(str::to_owned),
                agent_status: config.agent_status(),
            })
        }
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
            let authorized = match consent.authorize_update(
                &previous,
                &previous_revision,
                &value.config,
                value.picker_token.as_deref(),
                value.consent_token.as_deref(),
                value.harness_token.as_deref(),
            ) {
                Ok(authorized) => authorized,
                Err(authority_error) => {
                    return error(
                        &value.request_id,
                        ErrorCode::Unauthorized,
                        &authority_error.to_string(),
                    );
                }
            };
            match config.update_with_grants(
                value.config.clone(),
                &value.expected_revision,
                authorized.selected_identity.as_deref(),
                authorized.harness_commit,
            ) {
                Ok(Some(revision)) => {
                    consent.consume_update(
                        &previous,
                        &previous_revision,
                        &value.config,
                        value.picker_token.as_deref(),
                        value.consent_token.as_deref(),
                        value.harness_token.as_deref(),
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
                Err(choose_error) => {
                    // The failure may come from the picker itself or from
                    // checking the folder the user picked (for example one
                    // macOS privacy settings protect); keep the cause.
                    eprintln!("{NAMESPACE}: folder selection failed: {choose_error:#}");
                    error(
                        &value.request_id,
                        ErrorCode::Internal,
                        "could not open the folder picker or use the selected folder",
                    )
                }
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
        Request::CheckAgent(value) => check_agent(value, config),
        Request::DiscoverHarnesses(value) => {
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
                    "configuration changed; read it again before setting up AI",
                );
            }
            let offers = consent.discover_harnesses(config.revision(), context.env);
            Response::HarnessesDiscovered(HarnessesDiscovered {
                protocol_version: PROTOCOL_VERSION,
                request_id: value.request_id,
                offers,
            })
        }
        Request::ConfirmHarnessSetup(value) => {
            confirm_harness_setup(value, config, consent, context)
        }
    }
}

/// Native harness setup (§7.3): the host shows its own dialog and runs one
/// test prompt. The returned token authorizes exactly the returned config.
fn confirm_harness_setup(
    value: brauser_protocol::ConfirmHarnessSetupRequest,
    config: &mut ConfigStore,
    consent: &mut ConsentAuthority,
    context: &SetupContext<'_>,
) -> Response {
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
            "configuration changed; read it again before setting up AI",
        );
    }
    // Setup rewrites the whole file from this snapshot, so it must be the
    // file's own content, not a stand-in for one that needs repair.
    if config.config_issue().is_some() {
        return error(
            &value.request_id,
            ErrorCode::InvalidConfig,
            "configuration needs repair before AI setup",
        );
    }
    let current = config.snapshot().clone();
    match consent.confirm_harness_setup_with(
        &current,
        config.revision(),
        &value,
        context.env,
        context.confirm,
    ) {
        Ok(Some(confirmed)) => Response::HarnessSetupConfirmed(HarnessSetupConfirmed {
            protocol_version: PROTOCOL_VERSION,
            request_id: value.request_id,
            harness_token: confirmed.harness_token,
            config: confirmed.config,
            summary: confirmed.summary,
        }),
        Ok(None) => error(
            &value.request_id,
            ErrorCode::Cancelled,
            "harness setup was canceled",
        ),
        Err(setup_error) => error(&value.request_id, setup_error.code, &setup_error.message),
    }
}

/// The minimal agent-gated request (§7.3): every AI command goes through the
/// same readiness gate first. A URL, when given, is checked in its original
/// form against the agent denylist.
fn check_agent(value: brauser_protocol::CheckAgentRequest, config: &mut ConfigStore) -> Response {
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
    if config.refresh().is_err() {
        return error(
            &value.request_id,
            ErrorCode::InvalidConfig,
            "configuration is unavailable or needs repair",
        );
    }
    let ready = match config.agent_readiness() {
        Ok(ready) => ready,
        Err((code, message)) => return error(&value.request_id, code, &message),
    };
    let url_allowed = value
        .url
        .as_deref()
        .map(|url| !privacy::denylisted(url, &config.snapshot().agent_denylist));
    Response::AgentChecked(AgentChecked {
        protocol_version: PROTOCOL_VERSION,
        request_id: value.request_id,
        harness_id: ready.harness_id,
        harness_version: ready.harness_version,
        url_allowed,
    })
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
        note::NoteRequestError::TooLarge(message) => {
            error(request_id, ErrorCode::MessageTooLarge, &message)
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
    let mut body = serde_json::to_vec(response).context("serializing native response")?;
    if body.len() > MAX_OUTBOUND_BYTES {
        // One oversized answer must not end the connection: that would drop
        // every other in-flight request and show the extension only a
        // disconnect. Answer the same request with a small error instead.
        let fallback = error(
            response.request_id(),
            ErrorCode::MessageTooLarge,
            "response exceeds the host-to-extension size limit",
        );
        body = serde_json::to_vec(&fallback).context("serializing native response")?;
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
    fn an_unreadable_config_answers_hello_and_get_config_with_its_issue() {
        // The extension treats a failed hello as a missing host, so an
        // unreadable config must still answer hello and report the repair.
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();
        let config = ConfigStore::for_test(path);
        let requests = [
            Request::Hello(HelloRequest {
                protocol_version: PROTOCOL_VERSION,
                request_id: "hello".into(),
            }),
            Request::GetConfig(GetConfigRequest {
                protocol_version: PROTOCOL_VERSION,
                request_id: "read".into(),
            }),
            Request::UpdateConfig(UpdateConfigRequest {
                protocol_version: PROTOCOL_VERSION,
                request_id: "write".into(),
                expected_revision: "unreadable".into(),
                config: ConfigSnapshot {
                    storage: None,
                    capture_enabled: false,
                    sites: Vec::new(),
                    strip_params: Vec::new(),
                    near_repeat_secs: 300,
                    agent_denylist: Vec::new(),
                    agent_denylist_confirmed: false,
                    log_incognito: false,
                    agent: None,
                },
                picker_token: None,
                consent_token: None,
                harness_token: None,
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
        match &responses[0] {
            Response::HelloResult(value) => {
                assert!(!value.configured);
                assert!(value.config_issue.is_some());
            }
            other => panic!("expected hello_result, got {other:?}"),
        }
        match &responses[1] {
            Response::ConfigResult(value) => {
                assert!(value.config.storage.is_none());
                assert!(value.config.sites.is_empty());
                assert!(value.config_issue.is_some());
            }
            other => panic!("expected config_result, got {other:?}"),
        }
        match &responses[2] {
            Response::Error(value) => assert_eq!(value.code, ErrorCode::InvalidConfig),
            other => panic!("expected invalid_config, got {other:?}"),
        }
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
            ..crate::config::empty_config()
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
                harness_token: None,
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
                harness_token: None,
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

    #[test]
    fn unauthorized_roots_are_refused_before_any_filesystem_probe() {
        // An extension-supplied root is untrusted. Whether it exists must not
        // change the answer, or the host becomes a directory-existence oracle.
        let folder = tempfile::tempdir().unwrap();
        let existing = folder.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        let mut config = ConfigStore::for_test(folder.path().join("config.toml"));
        let mut consent = ConsentAuthority::new();
        let env = no_harnesses(folder.path());
        let context = SetupContext {
            env: &env,
            confirm: no_dialog,
        };
        for root in [existing.clone(), folder.path().join("absent")] {
            let snapshot = ConfigSnapshot {
                storage: Some(brauser_protocol::StorageConfig {
                    root: root.to_string_lossy().into_owned(),
                    profile: "neutral".into(),
                    log_dir: "log".into(),
                    pages_dir: "pages".into(),
                    later_dir: "later".into(),
                    summaries_dir: None,
                }),
                capture_enabled: false,
                sites: Vec::new(),
                strip_params: Vec::new(),
                near_repeat_secs: 300,
                agent_denylist: Vec::new(),
                agent_denylist_confirmed: false,
                log_incognito: false,
                agent: None,
            };
            let update = dispatch(
                Request::UpdateConfig(UpdateConfigRequest {
                    protocol_version: PROTOCOL_VERSION,
                    request_id: "probe-update".into(),
                    expected_revision: "missing".into(),
                    config: snapshot.clone(),
                    picker_token: None,
                    consent_token: None,
                    harness_token: None,
                }),
                &mut config,
                &mut consent,
                &context,
            );
            let confirm = dispatch(
                Request::ConfirmConfig(brauser_protocol::ConfirmConfigRequest {
                    protocol_version: PROTOCOL_VERSION,
                    request_id: "probe-confirm".into(),
                    expected_revision: "missing".into(),
                    config: snapshot,
                    picker_token: None,
                }),
                &mut config,
                &mut consent,
                &context,
            );
            for response in [update, confirm] {
                match response {
                    Response::Error(value) => assert_eq!(
                        value.code,
                        ErrorCode::Unauthorized,
                        "{} for {}",
                        value.message,
                        root.display()
                    ),
                    other => panic!("expected an authorization error, got {other:?}"),
                }
            }
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
                summaries_dir: None,
            }),
            capture_enabled: false,
            sites: Vec::new(),
            strip_params: Vec::new(),
            near_repeat_secs: 300,
            ..crate::config::empty_config()
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
            &SetupContext {
                env: &no_harnesses(folder.path()),
                confirm: no_dialog,
            },
        );
        match response {
            Response::NoteLoaded(value) => {
                assert!(!value.exists);
                assert_eq!(value.revision, "missing");
            }
            other => panic!("expected note_loaded, got {other:?}"),
        }
    }

    fn responses_from(output: Vec<u8>) -> Vec<Response> {
        let mut reader = Cursor::new(output);
        let mut responses = Vec::new();
        while let Some(body) = read_frame(&mut reader).unwrap() {
            responses.push(serde_json::from_slice::<Response>(&body).unwrap());
        }
        responses
    }

    fn save_request(id: &str, body: &str, revision: &str) -> Request {
        Request::SaveNote(brauser_protocol::SaveNoteRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: id.into(),
            url: "https://example.com/a".into(),
            title: "A Title".into(),
            body: body.into(),
            expected_revision: revision.into(),
        })
    }

    fn load_request(id: &str) -> Request {
        Request::LoadNote(brauser_protocol::LoadNoteRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: id.into(),
            url: "https://example.com/a".into(),
        })
    }

    fn hello_request(id: &str) -> Request {
        Request::Hello(HelloRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: id.into(),
        })
    }

    #[test]
    fn the_largest_valid_note_round_trips_without_ending_the_serve_loop() {
        let folder = tempfile::tempdir().unwrap();
        let notes = folder.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        let config = configured_store(&notes);
        // 98304 quotes, the panel's and the schema's limit, which double
        // once escaped.
        let body = "\"".repeat(98_304);
        let requests = [
            save_request("save-1", &body, "missing"),
            load_request("load-1"),
            // A stale save echoes the whole note back.
            save_request("save-stale", "other", "missing"),
            hello_request("hello-1"),
        ];
        let input: Vec<u8> = requests.into_iter().flat_map(encoded_request).collect();
        let mut output = Vec::new();
        serve_with_io(Cursor::new(input), &mut output, config).unwrap();

        let responses = responses_from(output);
        assert_eq!(responses.len(), 4);
        assert!(matches!(&responses[0], Response::NoteSaved(_)));
        match &responses[1] {
            Response::NoteLoaded(value) => assert_eq!(value.body, body),
            other => panic!("expected note_loaded, got {other:?}"),
        }
        match &responses[2] {
            Response::NoteConflict(value) => assert_eq!(value.body, body),
            other => panic!("expected note_conflict, got {other:?}"),
        }
        assert!(matches!(&responses[3], Response::HelloResult(_)));
    }

    #[test]
    fn a_note_too_large_to_return_gets_a_correlated_error_and_serving_continues() {
        let folder = tempfile::tempdir().unwrap();
        let notes = folder.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        let mut config = configured_store(&notes);
        let saved = match dispatch(
            save_request("save-1", "small", "missing"),
            &mut config,
            &mut ConsentAuthority::new(),
            &SetupContext {
                env: &no_harnesses(folder.path()),
                confirm: no_dialog,
            },
        ) {
            Response::NoteSaved(value) => value,
            other => panic!("expected note_saved, got {other:?}"),
        };
        // The note grows outside the panel (for example in Obsidian) past
        // what one response can carry, while staying under the read limit.
        let path = notes.join(&saved.relative_path);
        let grown = std::fs::read_to_string(&path)
            .unwrap()
            .replace("small", &"x".repeat(MAX_OUTBOUND_BYTES));
        std::fs::write(&path, grown).unwrap();

        let requests = [
            load_request("load-1"),
            save_request("save-stale", "new", "missing"),
            hello_request("hello-1"),
        ];
        let input: Vec<u8> = requests.into_iter().flat_map(encoded_request).collect();
        let mut output = Vec::new();
        serve_with_io(Cursor::new(input), &mut output, config).unwrap();

        let responses = responses_from(output);
        assert_eq!(responses.len(), 3);
        for (response, id) in responses[..2].iter().zip(["load-1", "save-stale"]) {
            match response {
                Response::Error(value) => {
                    assert_eq!(value.code, ErrorCode::MessageTooLarge);
                    assert_eq!(value.request_id, id);
                }
                other => panic!("expected message_too_large, got {other:?}"),
            }
        }
        assert!(matches!(&responses[2], Response::HelloResult(_)));
    }

    #[test]
    fn an_oversized_response_becomes_a_correlated_error_frame() {
        let response = Response::NoteLoaded(NoteLoaded {
            protocol_version: PROTOCOL_VERSION,
            request_id: "load-big".into(),
            exists: true,
            revision: "missing".into(),
            title: String::new(),
            body: "x".repeat(MAX_OUTBOUND_BYTES + 1),
        });
        let mut output = Vec::new();
        write_frame(&mut output, &response).unwrap();
        match decoded_response(output) {
            Response::Error(value) => {
                assert_eq!(value.code, ErrorCode::MessageTooLarge);
                assert_eq!(value.request_id, "load-big");
            }
            other => panic!("expected message_too_large, got {other:?}"),
        }
    }

    #[test]
    fn saving_a_note_then_reusing_a_stale_revision_returns_the_current_note() {
        let folder = tempfile::tempdir().unwrap();
        let notes = folder.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        let mut config = configured_store(&notes);
        let mut consent = ConsentAuthority::new();
        let env = no_harnesses(folder.path());
        let context = SetupContext {
            env: &env,
            confirm: no_dialog,
        };
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
                &context,
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

    /// A harness environment that finds nothing and passes nothing.
    fn no_harnesses(folder: &std::path::Path) -> HarnessEnv {
        HarnessEnv {
            search_path: std::ffi::OsString::new(),
            home: None,
            vars: std::collections::BTreeMap::new(),
            work_root: folder.join("agent-work"),
        }
    }

    fn no_dialog(_: DialogText<'_>) -> Result<bool> {
        bail!("no dialog is expected in this test")
    }

    fn served(requests: &[String], config: ConfigStore) -> Vec<Response> {
        let folder = tempfile::tempdir().unwrap();
        let env = no_harnesses(folder.path());
        served_with(
            requests,
            config,
            &SetupContext {
                env: &env,
                confirm: no_dialog,
            },
        )
    }

    fn served_with(
        requests: &[String],
        config: ConfigStore,
        context: &SetupContext<'_>,
    ) -> Vec<Response> {
        let input: Vec<u8> = requests
            .iter()
            .flat_map(|json| encoded_json(json.as_bytes()))
            .collect();
        let mut output = Vec::new();
        serve_with_context(Cursor::new(input), &mut output, config, context).unwrap();
        let mut reader = Cursor::new(output);
        let mut responses = Vec::new();
        while let Some(body) = read_frame(&mut reader).unwrap() {
            responses.push(serde_json::from_slice::<Response>(&body).unwrap());
        }
        responses
    }

    fn check_agent_json(url: Option<&str>) -> String {
        serde_json::to_string(&Request::CheckAgent(brauser_protocol::CheckAgentRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: "agent-1".into(),
            url: url.map(str::to_owned),
        }))
        .unwrap()
    }

    fn loaded(path: &std::path::Path) -> ConfigStore {
        let mut store = ConfigStore::for_test(path.to_path_buf());
        store.refresh().unwrap();
        store
    }

    fn refused(response: &Response) -> (ErrorCode, &str) {
        match response {
            Response::Error(value) => (value.code, value.message.as_str()),
            other => panic!("expected an agent refusal, got {other:?}"),
        }
    }

    #[test]
    fn check_agent_refused_while_denylist_unconfirmed() {
        let folder = tempfile::tempdir().unwrap();
        let (path, root, binary) = config::fixtures::ready(folder.path());
        // A harness entry and record exist, but the denylist was never
        // confirmed through native setup.
        config::fixtures::write(
            &path,
            &root,
            "summaries_dir = \"summaries\"",
            "agent_denylist_confirmed = false",
            &format!(
                "{}{}",
                config::fixtures::agent_table(&binary),
                config::fixtures::record_table(&binary)
            ),
        );
        let responses = served(
            &[check_agent_json(Some("https://example.com/"))],
            loaded(&path),
        );
        let (code, message) = refused(&responses[0]);
        assert_eq!(code, ErrorCode::NotConfigured);
        assert!(message.contains(&format!("{} settings", crate::brand::APP_NAME)));
    }

    #[test]
    fn check_agent_refused_for_m1_config_missing_key() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        config::fixtures::write(&path, folder.path(), "", "", "");
        let responses = served(&[check_agent_json(None)], loaded(&path));
        let (code, message) = refused(&responses[0]);
        assert_eq!(code, ErrorCode::NotConfigured);
        assert!(message.contains(&format!("{} settings", crate::brand::APP_NAME)));
    }

    #[test]
    fn check_agent_reports_harness_and_denylisted_url() {
        let folder = tempfile::tempdir().unwrap();
        let (path, _, _) = config::fixtures::ready(folder.path());
        let responses = served(
            &[
                check_agent_json(Some("https://www.bank.example/account")),
                check_agent_json(Some("https://example.com/")),
                check_agent_json(None),
            ],
            loaded(&path),
        );
        let allowed: Vec<Option<bool>> = responses
            .iter()
            .map(|response| match response {
                Response::AgentChecked(value) => {
                    assert_eq!(value.harness_id, "claude-code");
                    assert_eq!(value.harness_version, "2.1.284");
                    value.url_allowed
                }
                other => panic!("expected agent_checked, got {other:?}"),
            })
            .collect();
        assert_eq!(allowed, vec![Some(false), Some(true), None]);
    }

    #[test]
    fn update_config_rejects_extension_setting_agent_denylist_confirmed() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        config::fixtures::write(&path, folder.path(), "", "", "");
        let store = loaded(&path);
        let revision = store.revision().to_owned();
        let before = std::fs::read(&path).unwrap();
        let request = Request::UpdateConfig(UpdateConfigRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: "write-1".into(),
            expected_revision: revision.clone(),
            config: ConfigSnapshot {
                agent_denylist_confirmed: true,
                ..store.snapshot().clone()
            },
            picker_token: None,
            consent_token: None,
            harness_token: None,
        });
        let responses = served(&[serde_json::to_string(&request).unwrap()], store);
        assert_eq!(refused(&responses[0]).0, ErrorCode::Unauthorized);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(loaded(&path).revision(), revision);
    }

    #[test]
    fn get_config_reports_agent_status_not_set_up() {
        let folder = tempfile::tempdir().unwrap();
        let config = ConfigStore::for_test(folder.path().join("config.toml"));
        let request = serde_json::to_string(&Request::GetConfig(GetConfigRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: "read-1".into(),
        }))
        .unwrap();
        match &served(&[request], config)[0] {
            Response::ConfigResult(value) => {
                assert_eq!(
                    value.agent_status.state,
                    brauser_protocol::AgentState::NotSetUp
                );
                assert_eq!(value.agent_status.harness_version, None);
                assert!(value.agent_status.message.is_some());
            }
            other => panic!("expected config_result, got {other:?}"),
        }
    }

    #[test]
    fn protocol_v3_request_gets_unsupported_version() {
        let folder = tempfile::tempdir().unwrap();
        let config = ConfigStore::for_test(folder.path().join("config.toml"));
        let request =
            r#"{"type":"get_config","protocol_version":3,"request_id":"old-1"}"#.to_owned();
        match &served(&[request], config)[0] {
            Response::Error(value) => {
                assert_eq!(value.code, ErrorCode::UnsupportedProtocolVersion);
                assert_eq!(value.request_id, "old-1");
            }
            other => panic!("expected version error, got {other:?}"),
        }
    }

    /// Native harness setup against fake harnesses on an injected search
    /// path. No real harness is ever found or run.
    #[cfg(unix)]
    mod harness_setup {
        use super::*;
        use crate::harness::fake::{Fake, PASSING_BODY};
        use brauser_protocol::{
            AgentState, ConfirmHarnessSetupRequest, DiscoverHarnessesRequest, HarnessOffer,
        };

        struct Session {
            fake: Fake,
            env: HarnessEnv,
            path: std::path::PathBuf,
            config: ConfigStore,
            consent: ConsentAuthority,
        }

        fn accept(_: DialogText<'_>) -> Result<bool> {
            Ok(true)
        }

        fn session(body: &str) -> Session {
            let fake = Fake::new();
            fake.claude(body);
            let root = fake.path("notes");
            std::fs::create_dir(&root).unwrap();
            let path = fake.path("config.toml");
            config::fixtures::write(&path, &root, "", "agent_denylist = [\"bank.example\"]", "");
            let env = fake.env(&[]);
            Session {
                config: loaded(&path),
                fake,
                env,
                path,
                consent: ConsentAuthority::new(),
            }
        }

        impl Session {
            fn call(
                &mut self,
                request: Request,
                confirm: fn(DialogText<'_>) -> Result<bool>,
            ) -> Response {
                let context = SetupContext {
                    env: &self.env,
                    confirm,
                };
                dispatch(request, &mut self.config, &mut self.consent, &context)
            }

            fn revision(&mut self) -> String {
                self.config.refresh().unwrap();
                self.config.revision().to_owned()
            }

            fn discover(&mut self, revision: &str) -> Response {
                self.call(
                    Request::DiscoverHarnesses(DiscoverHarnessesRequest {
                        protocol_version: PROTOCOL_VERSION,
                        request_id: "discover-1".into(),
                        expected_revision: revision.into(),
                    }),
                    no_dialog,
                )
            }

            fn offers(&mut self) -> Vec<HarnessOffer> {
                let revision = self.revision();
                match self.discover(&revision) {
                    Response::HarnessesDiscovered(value) => value.offers,
                    other => panic!("expected harnesses_discovered, got {other:?}"),
                }
            }

            fn claude_offer(&mut self) -> String {
                self.offers()
                    .into_iter()
                    .find(|offer| offer.harness_id == "claude-code")
                    .and_then(|offer| offer.offer_id)
                    .expect("the fake claude is offered")
            }

            fn confirm(
                &mut self,
                revision: &str,
                offer_id: &str,
                summaries_dir: Option<&str>,
                confirm: fn(DialogText<'_>) -> Result<bool>,
            ) -> Response {
                self.call(
                    Request::ConfirmHarnessSetup(ConfirmHarnessSetupRequest {
                        protocol_version: PROTOCOL_VERSION,
                        request_id: "confirm-1".into(),
                        expected_revision: revision.into(),
                        offer_id: offer_id.into(),
                        env_names: Vec::new(),
                        agent_denylist: vec!["bank.example".into()],
                        summaries_dir: summaries_dir.map(str::to_owned),
                    }),
                    confirm,
                )
            }

            fn update(&mut self, next: ConfigSnapshot, harness_token: Option<&str>) -> Response {
                let revision = self.revision();
                self.call(
                    Request::UpdateConfig(UpdateConfigRequest {
                        protocol_version: PROTOCOL_VERSION,
                        request_id: "write-1".into(),
                        expected_revision: revision,
                        config: next,
                        picker_token: None,
                        consent_token: None,
                        harness_token: harness_token.map(str::to_owned),
                    }),
                    no_dialog,
                )
            }

            /// Discover, confirm, and commit the fake claude.
            fn set_up(&mut self) {
                let offer = self.claude_offer();
                let revision = self.revision();
                let confirmed = match self.confirm(&revision, &offer, None, accept) {
                    Response::HarnessSetupConfirmed(value) => value,
                    other => panic!("expected harness_setup_confirmed, got {other:?}"),
                };
                match self.update(confirmed.config.clone(), Some(&confirmed.harness_token)) {
                    Response::ConfigUpdated(_) => {}
                    other => panic!("expected config_updated, got {other:?}"),
                }
                // The token was consumed by the commit.
                let again = self.update(confirmed.config, Some(&confirmed.harness_token));
                assert_eq!(refused(&again).0, ErrorCode::Unauthorized);
            }

            fn status(&mut self) -> brauser_protocol::AgentStatus {
                match self.call(
                    Request::GetConfig(GetConfigRequest {
                        protocol_version: PROTOCOL_VERSION,
                        request_id: "read-1".into(),
                    }),
                    no_dialog,
                ) {
                    Response::ConfigResult(value) => value.agent_status,
                    other => panic!("expected config_result, got {other:?}"),
                }
            }
        }

        #[test]
        fn discover_lists_fake_claude_ready_and_codex_refused() {
            let mut session = session(PASSING_BODY);
            session
                .fake
                .install("codex", "codex-cli 0.130.0", "", "exit 0");
            let offers = session.offers();
            assert_eq!(offers.len(), 2);
            let claude = offers
                .iter()
                .find(|offer| offer.harness_id == "claude-code")
                .unwrap();
            assert!(claude.offer_id.is_some());
            assert_eq!(claude.refusal, None);
            assert_eq!(claude.version.as_deref(), Some("2.1.284"));
            assert_eq!(
                claude.binary,
                session.fake.path("bin").join("claude").to_string_lossy()
            );
            let codex = offers
                .iter()
                .find(|offer| offer.harness_id == "codex")
                .unwrap();
            assert_eq!(codex.offer_id, None);
            assert_eq!(
                codex.refusal.as_deref(),
                Some("Codex 0.130.0 has not been reviewed for Brauser")
            );
            // An unreviewed version stops after --version.
            assert_eq!(
                session.fake.log("codex", "argv.log").as_deref(),
                Some("[--version]\n")
            );
            // Discovery never writes config.
            assert_eq!(session.revision(), loaded(&session.path).revision());
            assert_eq!(refused(&session.discover("stale")).0, ErrorCode::Conflict);
        }

        #[test]
        fn confirm_harness_setup_rejects_unknown_or_stale_offer() {
            let mut session = session(PASSING_BODY);
            let revision = session.revision();
            let response = session.confirm(&revision, "forged", None, no_dialog);
            assert_eq!(refused(&response).0, ErrorCode::Unauthorized);

            let offer = session.claude_offer();
            let stale = session.revision();
            let next = ConfigSnapshot {
                near_repeat_secs: 600,
                ..session.config.snapshot().clone()
            };
            assert!(matches!(
                session.update(next, None),
                Response::ConfigUpdated(_)
            ));
            let response = session.confirm(&stale, &offer, None, no_dialog);
            assert_eq!(refused(&response).0, ErrorCode::Conflict);
            // An offer minted before the change is bound to the old revision.
            let current = session.revision();
            let response = session.confirm(&current, &offer, None, no_dialog);
            assert_eq!(refused(&response).0, ErrorCode::Unauthorized);
            assert_eq!(session.fake.log("claude", "stdin.log"), None);
        }

        #[test]
        fn summaries_overlap_at_setup_is_recoverable_invalid_config() {
            let mut session = session(PASSING_BODY);
            let before = std::fs::read(&session.path).unwrap();
            let offer = session.claude_offer();
            let revision = session.revision();
            let response = session.confirm(&revision, &offer, Some("log"), no_dialog);
            let (code, message) = refused(&response);
            assert_eq!(code, ErrorCode::InvalidConfig);
            assert!(message.contains("must not overlap"), "{message}");
            assert_eq!(std::fs::read(&session.path).unwrap(), before);
            // Detect again and choose a different folder.
            let offer = session.claude_offer();
            let response = session.confirm(&revision, &offer, Some("ai"), accept);
            match response {
                Response::HarnessSetupConfirmed(value) => assert_eq!(
                    value.config.storage.unwrap().summaries_dir.as_deref(),
                    Some("ai")
                ),
                other => panic!("expected harness_setup_confirmed, got {other:?}"),
            }
        }

        #[test]
        fn check_agent_reports_denylisted_url() {
            let mut session = session(PASSING_BODY);
            session.set_up();
            let checked: Vec<Option<bool>> = [
                Some("https://www.bank.example/account"),
                Some("https://example.com/"),
                None,
            ]
            .into_iter()
            .map(|url| {
                match session.call(
                    serde_json::from_str(&check_agent_json(url)).unwrap(),
                    no_dialog,
                ) {
                    Response::AgentChecked(value) => {
                        assert_eq!(value.harness_id, "claude-code");
                        assert_eq!(value.harness_version, "2.1.284");
                        value.url_allowed
                    }
                    other => panic!("expected agent_checked, got {other:?}"),
                }
            })
            .collect();
            assert_eq!(checked, vec![Some(false), Some(true), None]);
        }

        #[test]
        fn get_config_reports_agent_status_states() {
            let mut session = session(PASSING_BODY);
            assert_eq!(session.status().state, AgentState::NotSetUp);
            session.set_up();
            let status = session.status();
            assert_eq!(status.state, AgentState::Ready);
            assert_eq!(status.harness_version.as_deref(), Some("2.1.284"));
            assert_eq!(status.message, None);

            // The harness updated itself in place.
            let binary = session.fake.path("bin").join("claude");
            let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
            std::fs::File::options()
                .write(true)
                .open(&binary)
                .unwrap()
                .set_modified(later)
                .unwrap();
            let status = session.status();
            assert_eq!(status.state, AgentState::HarnessProblem);
            assert_eq!(status.harness_version.as_deref(), Some("2.1.284"));

            // A hand-written entry without the native confirmation.
            let root = session.fake.path("notes");
            config::fixtures::write(
                &session.path,
                &root,
                "summaries_dir = \"summaries\"",
                "agent_denylist_confirmed = false",
                &format!(
                    "{}{}",
                    config::fixtures::agent_table(&binary),
                    config::fixtures::record_table(&binary)
                ),
            );
            assert_eq!(session.status().state, AgentState::DenylistUnconfirmed);
        }
    }
}
