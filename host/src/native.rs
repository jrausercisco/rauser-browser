//! Chrome native messaging uses a four-byte, native-endian length prefix
//! followed by one UTF-8 JSON value. Stdout contains frames only.

use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use rauser_protocol::{
    ConfigResult, ConfigUpdated, ErrorCode, ErrorResponse, HelloResult, PROTOCOL_VERSION, Request,
    Response,
};

use crate::config::{self, ConfigStore};

const MAX_INBOUND_BYTES: usize = 4 * 1024 * 1024;
// Chrome limits host-to-extension messages to 1 MiB. Leave room for the
// framing prefix and future envelope fields.
const MAX_OUTBOUND_BYTES: usize = 900 * 1024;
const MAX_REQUEST_ID_BYTES: usize = 128;

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
        let response = match serde_json::from_slice::<Request>(&body) {
            Ok(request) => dispatch(request, &mut config),
            Err(_) => error(
                "",
                ErrorCode::InvalidRequest,
                "invalid or unsupported request JSON",
            ),
        };
        write_frame(&mut output, &response)?;
    }
}

fn dispatch(request: Request, config: &mut ConfigStore) -> Response {
    let id = request.request_id();
    if id.is_empty() || id.len() > MAX_REQUEST_ID_BYTES || id.chars().any(char::is_control) {
        return error(
            "",
            ErrorCode::InvalidRequest,
            "request_id must be 1–128 non-control UTF-8 bytes",
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
        Request::Hello(value) => Response::HelloResult(HelloResult {
            protocol_version: PROTOCOL_VERSION,
            request_id: value.request_id,
            host_version: env!("CARGO_PKG_VERSION").to_owned(),
            configured: config.configured(),
        }),
        Request::GetConfig(value) => Response::ConfigResult(ConfigResult {
            protocol_version: PROTOCOL_VERSION,
            request_id: value.request_id,
            config: config.snapshot().clone(),
        }),
        Request::UpdateConfig(value) => {
            if let Err(validation_error) = config::validate(&value.config) {
                return error(
                    &value.request_id,
                    ErrorCode::InvalidConfig,
                    &validation_error.to_string(),
                );
            }
            match config.update(value.config.clone()) {
                Ok(()) => Response::ConfigUpdated(ConfigUpdated {
                    protocol_version: PROTOCOL_VERSION,
                    request_id: value.request_id,
                    config: value.config,
                }),
                Err(_) => error(
                    &value.request_id,
                    ErrorCode::Internal,
                    "could not save configuration",
                ),
            }
        }
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
    use rauser_protocol::HelloRequest;
    use std::io::Cursor;

    fn encoded_request(request: Request) -> Vec<u8> {
        let json = serde_json::to_vec(&request).unwrap();
        let mut wire = (json.len() as u32).to_ne_bytes().to_vec();
        wire.extend(json);
        wire
    }

    fn decoded_response(wire: Vec<u8>) -> Response {
        let mut input = Cursor::new(wire);
        let json = read_frame(&mut input).unwrap().unwrap();
        serde_json::from_slice(&json).unwrap()
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
}
