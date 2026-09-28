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
    if command != PICK_FOLDER_COMMAND && command != CONFIRM_COMMAND {
        bail!("unknown dialog command: {command}");
    }
    #[cfg(feature = "scripted-dialogs")]
    if let Some(directory) = scripted::directory()? {
        let reply = scripted::answer(&directory, command, &input, scripted::ANSWER_TIMEOUT)?;
        return write_reply(&reply);
    }
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
    write_reply(&reply)
}

fn write_reply(reply: &str) -> Result<()> {
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

/// Test-only replacement for the dialog UI, compiled only into debug builds
/// with the `scripted-dialogs` feature. Only the child changes: the parent
/// still spawns it, parses its reply, and mints and checks every grant, so a
/// scripted answer has exactly the authority of a person's click.
///
/// Protocol, in the directory named by `SCRIPTED_DIALOGS_ENV`: the child
/// appends `{"dialog", "text"}` to `shown.jsonl` (the dialog "opens"), waits
/// for an `answer` file of the form `{"dialog", "reply"}`, removes it, and
/// replies with it. The extension cannot reach this directory: it cannot set
/// the host's environment and vault writes stay inside the chosen folder.
#[cfg(feature = "scripted-dialogs")]
pub mod scripted {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use serde::{Deserialize, Serialize};

    use super::{CONFIRM_COMMAND, PICK_FOLDER_COMMAND, parse_confirm_reply, parse_pick_reply};
    use crate::brand::SCRIPTED_DIALOGS_ENV;

    pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
    pub const SHOWN_FILE: &str = "shown.jsonl";
    pub const ANSWER_FILE: &str = "answer";
    const MAX_ANSWER_BYTES: u64 = 64 * 1024;

    #[derive(Serialize)]
    struct Shown<'a> {
        dialog: &'a str,
        text: &'a str,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        dialog: String,
        reply: String,
    }

    /// The scripted-answer directory, if this test build was asked to use one.
    pub fn directory() -> Result<Option<PathBuf>> {
        let Some(value) = std::env::var_os(SCRIPTED_DIALOGS_ENV) else {
            return Ok(None);
        };
        validate_directory(PathBuf::from(value)).map(Some)
    }

    pub fn validate_directory(directory: PathBuf) -> Result<PathBuf> {
        if !directory.is_absolute() {
            bail!("{SCRIPTED_DIALOGS_ENV} must be an absolute path");
        }
        let metadata = fs::symlink_metadata(&directory)
            .with_context(|| format!("inspecting {}", directory.display()))?;
        if !metadata.is_dir() {
            bail!("{SCRIPTED_DIALOGS_ENV} must name a directory, not a link or file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o022 != 0 {
                bail!("{SCRIPTED_DIALOGS_ENV} must not be group- or world-writable");
            }
        }
        Ok(directory)
    }

    fn dialog_name(command: &str) -> Result<&'static str> {
        match command {
            PICK_FOLDER_COMMAND => Ok("pick-folder"),
            CONFIRM_COMMAND => Ok("confirm"),
            other => bail!("unknown dialog command: {other}"),
        }
    }

    /// Record that the dialog opened, then wait for and consume its answer.
    /// Anything unexpected fails closed, as a crashed dialog would.
    pub fn answer(
        directory: &Path,
        command: &str,
        text: &str,
        timeout: Duration,
    ) -> Result<String> {
        let dialog = dialog_name(command)?;
        eprintln!(
            "{}: scripted dialog {dialog}; no UI shown",
            crate::brand::NAMESPACE
        );
        let mut line = serde_json::to_string(&Shown { dialog, text })?;
        line.push('\n');
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(SHOWN_FILE))
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .context("recording the scripted dialog")?;
        let answer_path = directory.join(ANSWER_FILE);
        let deadline = Instant::now() + timeout;
        let raw = loop {
            match fs::symlink_metadata(&answer_path) {
                Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_ANSWER_BYTES => {
                    let raw =
                        fs::read_to_string(&answer_path).context("reading the scripted answer")?;
                    fs::remove_file(&answer_path).context("consuming the scripted answer")?;
                    break raw;
                }
                Ok(_) => bail!("scripted answer is not a small regular file"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspecting the scripted answer"),
            }
            if Instant::now() >= deadline {
                bail!("no scripted answer for the {dialog} dialog");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let answer: Answer = serde_json::from_str(&raw).context("parsing the scripted answer")?;
        if answer.dialog != dialog {
            bail!(
                "scripted answer is for {}, but the {dialog} dialog is open",
                answer.dialog
            );
        }
        let valid = if command == PICK_FOLDER_COMMAND {
            parse_pick_reply(&answer.reply).is_ok()
        } else {
            parse_confirm_reply(&answer.reply).is_ok()
        };
        if !valid {
            bail!("scripted answer is not a valid {dialog} reply");
        }
        Ok(answer.reply)
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

    #[cfg(feature = "scripted-dialogs")]
    mod scripted_answers {
        use super::super::scripted::{ANSWER_FILE, SHOWN_FILE, answer, validate_directory};
        use super::super::{CONFIRM_COMMAND, PICK_FOLDER_COMMAND};
        use std::time::Duration;

        const SHORT: Duration = Duration::from_millis(300);

        fn with_answer(contents: &str) -> tempfile::TempDir {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join(ANSWER_FILE), contents).unwrap();
            directory
        }

        #[test]
        fn consumes_a_matching_answer_and_records_the_dialog_text() {
            let directory = with_answer(r#"{"dialog":"confirm","reply":"confirmed"}"#);
            let reply = answer(directory.path(), CONFIRM_COMMAND, "Title\nBody", SHORT).unwrap();
            assert_eq!(reply, "confirmed");
            assert!(!directory.path().join(ANSWER_FILE).exists());
            let shown = std::fs::read_to_string(directory.path().join(SHOWN_FILE)).unwrap();
            assert_eq!(
                shown,
                "{\"dialog\":\"confirm\",\"text\":\"Title\\nBody\"}\n"
            );
        }

        #[test]
        fn rejects_an_answer_for_another_dialog() {
            let directory = with_answer(r#"{"dialog":"confirm","reply":"confirmed"}"#);
            assert!(answer(directory.path(), PICK_FOLDER_COMMAND, "Pick", SHORT).is_err());
        }

        #[test]
        fn rejects_replies_the_real_dialog_cannot_produce() {
            for contents in [
                r#"{"dialog":"pick-folder","reply":"picked:"}"#,
                r#"{"dialog":"pick-folder","reply":"confirmed"}"#,
                r#"{"dialog":"confirm","reply":"yes"}"#,
                r#"{"dialog":"confirm","reply":"confirmed","extra":1}"#,
                "confirmed",
            ] {
                let directory = with_answer(contents);
                let command = if contents.contains("pick-folder") {
                    PICK_FOLDER_COMMAND
                } else {
                    CONFIRM_COMMAND
                };
                assert!(
                    answer(directory.path(), command, "", SHORT).is_err(),
                    "{contents}"
                );
            }
        }

        #[test]
        fn answer_directory_must_be_a_private_absolute_directory() {
            let directory = tempfile::tempdir().unwrap();
            assert!(validate_directory(directory.path().to_path_buf()).is_ok());
            assert!(validate_directory("relative/answers".into()).is_err());
            let file = directory.path().join("file");
            std::fs::write(&file, "").unwrap();
            assert!(validate_directory(file).is_err());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let link = directory.path().join("link");
                let target = tempfile::tempdir().unwrap();
                std::os::unix::fs::symlink(target.path(), &link).unwrap();
                assert!(validate_directory(link).is_err());
                let shared = std::fs::Permissions::from_mode(0o777);
                std::fs::set_permissions(target.path(), shared).unwrap();
                assert!(validate_directory(target.path().to_path_buf()).is_err());
            }
        }

        #[test]
        fn fails_closed_when_no_answer_arrives() {
            let directory = tempfile::tempdir().unwrap();
            assert!(answer(directory.path(), CONFIRM_COMMAND, "", SHORT).is_err());
        }
    }
}
