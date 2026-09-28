//! Native dialogs run in a short-lived child process.
//!
//! A native messaging host blocks its main thread on stdin between requests.
//! On macOS, showing an AppKit modal registers the process with the window
//! server; once the modal closes, the idle process no longer services its
//! event queue and macOS shows a persistent busy cursor. Running each dialog
//! in a child that exits when the dialog closes avoids leaving an unresponsive
//! GUI process behind, and gives each Windows dialog a fresh COM apartment.
//!
//! The child only reports what the user chose. Grants are minted by the
//! parent, so invoking the child directly cannot authorize anything.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::brand::APP_NAME;
use anyhow::{Context, Result, bail};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

/// Internal subcommand names. They are not a supported CLI surface.
pub const PICK_FOLDER_COMMAND: &str = "__dialog-pick-folder";
pub const CONFIRM_COMMAND: &str = "__dialog-confirm";

const MAX_CHILD_INPUT_BYTES: u64 = 64 * 1024;
const MAX_CHILD_OUTPUT_BYTES: u64 = 64 * 1024;
const PICKED_PREFIX: &str = "picked:";
const CANCELED: &str = "canceled";
const CONFIRMED: &str = "confirmed";

pub struct DialogText<'a> {
    pub title: &'a str,
    pub description: &'a str,
}

/// Show the folder picker in a child process. `None` means canceled.
pub fn pick_folder(title: &str) -> Result<Option<PathBuf>> {
    let reply = run_child(PICK_FOLDER_COMMAND, title)?;
    parse_pick_reply(&reply)
}

/// Show a Yes/No warning in a child process. `false` means declined.
pub fn confirm(text: DialogText<'_>) -> Result<bool> {
    let input = format!("{}\n{}", text.title, text.description);
    let reply = run_child(CONFIRM_COMMAND, &input)?;
    parse_confirm_reply(&reply)
}

/// Entry point for the child process. Reads its request from stdin and
/// writes a single-line reply to stdout.
pub fn run_child_command(command: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .take(MAX_CHILD_INPUT_BYTES)
        .read_to_string(&mut input)
        .context("reading dialog request")?;
    let reply = match command {
        PICK_FOLDER_COMMAND => match FileDialog::new().set_title(input.trim()).pick_folder() {
            Some(path) => {
                let path = path
                    .into_os_string()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("selected folder path is not valid UTF-8"))?;
                format!("{PICKED_PREFIX}{path}")
            }
            None => CANCELED.to_owned(),
        },
        CONFIRM_COMMAND => {
            let (title, description) = input.split_once('\n').unwrap_or((input.as_str(), ""));
            let accepted = MessageDialog::new()
                .set_title(title)
                .set_description(description)
                .set_level(MessageLevel::Warning)
                .set_buttons(MessageButtons::YesNo)
                .show()
                == MessageDialogResult::Yes;
            if accepted { CONFIRMED } else { CANCELED }.to_owned()
        }
        other => bail!("unknown dialog command: {other}"),
    };
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(reply.as_bytes())
        .and_then(|()| stdout.flush())
        .context("writing dialog reply")
}

fn run_child(command: &str, input: &str) -> Result<String> {
    let exe =
        std::env::current_exe().with_context(|| format!("locating the {APP_NAME} host binary"))?;
    let mut child = Command::new(exe)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Never inherit the native messaging stdout; the child's stderr is
        // Chrome's host log, which is where diagnostics belong.
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting the native dialog")?;
    {
        let mut stdin = child.stdin.take().context("opening dialog input")?;
        stdin
            .write_all(input.as_bytes())
            .context("sending dialog request")?;
    }
    let mut reply = String::new();
    child
        .stdout
        .take()
        .context("opening dialog output")?
        .take(MAX_CHILD_OUTPUT_BYTES)
        .read_to_string(&mut reply)
        .context("reading dialog reply")?;
    let status = child.wait().context("waiting for the native dialog")?;
    if !status.success() {
        bail!("native dialog exited unsuccessfully ({status})");
    }
    Ok(reply)
}

fn parse_pick_reply(reply: &str) -> Result<Option<PathBuf>> {
    if reply == CANCELED {
        return Ok(None);
    }
    match reply.strip_prefix(PICKED_PREFIX) {
        Some(path) if !path.is_empty() => Ok(Some(PathBuf::from(path))),
        _ => bail!("native folder picker returned an unexpected reply"),
    }
}

fn parse_confirm_reply(reply: &str) -> Result<bool> {
    match reply {
        CONFIRMED => Ok(true),
        CANCELED => Ok(false),
        _ => bail!("native confirmation returned an unexpected reply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_reply_distinguishes_cancel_selection_and_garbage() {
        assert_eq!(parse_pick_reply("canceled").unwrap(), None);
        assert_eq!(
            parse_pick_reply("picked:/tmp/notes").unwrap(),
            Some(PathBuf::from("/tmp/notes"))
        );
        assert!(parse_pick_reply("picked:").is_err());
        assert!(parse_pick_reply("").is_err());
        assert!(parse_pick_reply("/tmp/notes").is_err());
    }

    #[test]
    fn confirm_reply_fails_closed_on_unexpected_output() {
        assert!(parse_confirm_reply("confirmed").unwrap());
        assert!(!parse_confirm_reply("canceled").unwrap());
        assert!(parse_confirm_reply("").is_err());
        assert!(parse_confirm_reply("confirmed\n").is_err());
    }
}
