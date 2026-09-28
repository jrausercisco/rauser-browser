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
//!
//! A dialog never outlives its request. The parent sends a length-prefixed
//! request and keeps the child's stdin open; the child exits, closing its
//! dialog, as soon as that pipe closes, which happens however the parent
//! ends, including when Chrome kills it on disconnect. The parent also kills
//! a child still open at `DIALOG_TIMEOUT`.
//!
//! On macOS the confirmation is drawn by the system UserNotificationCenter,
//! not by the child, so killing the child would leave it on screen. The child
//! gives it a timeout shorter than `DIALOG_TIMEOUT` and cancels it before
//! exiting, and the parent closes the child's stdin and waits briefly before
//! it resorts to a kill.

use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::brand::APP_NAME;
use anyhow::{Context, Result, bail};
use rfd::FileDialog;
#[cfg(not(target_os = "macos"))]
use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

/// Internal subcommand names. They are not a supported CLI surface.
pub const PICK_FOLDER_COMMAND: &str = "__dialog-pick-folder";
pub const CONFIRM_COMMAND: &str = "__dialog-confirm";

/// How long a dialog may stay open. The extension waits five minutes for a
/// dialog reply, so the host closes the dialog and answers first; an answer
/// the extension has stopped waiting for would authorize nothing.
const DIALOG_TIMEOUT: Duration = Duration::from_secs(270);
/// How long a dialog the child shows itself may stay open. Shorter than
/// `DIALOG_TIMEOUT`, so the dialog closes before the parent gives up.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const CHILD_DIALOG_TIMEOUT: Duration = Duration::from_secs(260);
/// How long the parent waits, after closing a child's stdin, for the child to
/// close its dialog and exit before killing it.
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(2);
const MAX_CHILD_INPUT_BYTES: u64 = 64 * 1024;
const MAX_LENGTH_LINE_BYTES: u64 = 16;
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
    if command != PICK_FOLDER_COMMAND && command != CONFIRM_COMMAND {
        bail!("unknown dialog command: {command}");
    }
    let input = read_request(&mut std::io::stdin().lock())?;
    exit_when_parent_goes_away();
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
            if confirm_in_child(title, description)? {
                CONFIRMED
            } else {
                CANCELED
            }
            .to_owned()
        }
        other => bail!("unknown dialog command: {other}"),
    };
    write_reply(&reply)
}

/// Read `<byte length>\n<text>` without waiting for end of input, which the
/// parent holds back for as long as it wants the dialog open.
fn read_request(input: &mut impl BufRead) -> Result<String> {
    let mut length = String::new();
    input
        .take(MAX_LENGTH_LINE_BYTES)
        .read_line(&mut length)
        .context("reading dialog request length")?;
    let length: u64 = length
        .strip_suffix('\n')
        .and_then(|digits| digits.parse().ok())
        .filter(|length| *length <= MAX_CHILD_INPUT_BYTES)
        .context("dialog request has no valid length")?;
    let mut text = Vec::new();
    input
        .take(length)
        .read_to_end(&mut text)
        .context("reading dialog request")?;
    if text.len() as u64 != length {
        bail!("dialog request ended early");
    }
    String::from_utf8(text).context("dialog request is not UTF-8")
}

/// A dialog drawn outside this process, which exiting would not close.
/// Guarded with whether the parent has gone, so such a dialog is either never
/// shown or is canceled before this process exits.
struct ExternalDialog {
    parent_gone: bool,
    cancel: Option<Box<dyn FnOnce() + Send>>,
}

static EXTERNAL_DIALOG: Mutex<ExternalDialog> = Mutex::new(ExternalDialog {
    parent_gone: false,
    cancel: None,
});

/// End this process, and with it the dialog, once the parent's end of stdin
/// closes. The parent sends nothing more after the request, so any read that
/// returns means the parent is gone or has given up on the dialog.
fn exit_when_parent_goes_away() {
    std::thread::spawn(|| {
        let mut buffer = [0; 64];
        let mut stdin = std::io::stdin().lock();
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        // Held until exit, so no external dialog can open after this point.
        let mut external = EXTERNAL_DIALOG
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        external.parent_gone = true;
        if let Some(cancel) = external.cancel.take() {
            cancel();
        }
        eprintln!(
            "{}: dialog closed because its request ended",
            crate::brand::NAMESPACE
        );
        std::process::exit(1);
    });
}

/// Show the Yes/No confirmation. On macOS the system draws it, so it is shown
/// as an alert that can time out and be canceled.
#[cfg(target_os = "macos")]
fn confirm_in_child(title: &str, description: &str) -> Result<bool> {
    use brauser_macos_alert::{Alert, AlertText};
    use std::sync::Arc;

    let alert = {
        let mut external = EXTERNAL_DIALOG
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if external.parent_gone {
            bail!("the dialog request ended before the dialog opened");
        }
        let text = AlertText {
            title,
            message: description,
            yes: "Yes",
            no: "No",
        };
        let alert =
            Arc::new(Alert::show(text, CHILD_DIALOG_TIMEOUT).map_err(|code| {
                anyhow::anyhow!("showing the macOS confirmation failed ({code})")
            })?);
        let shared = Arc::clone(&alert);
        external.cancel = Some(Box::new(move || shared.cancel()));
        alert
    };
    let accepted = alert.wait();
    EXTERNAL_DIALOG
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .cancel = None;
    Ok(accepted)
}

#[cfg(not(target_os = "macos"))]
fn confirm_in_child(title: &str, description: &str) -> Result<bool> {
    Ok(MessageDialog::new()
        .set_title(title)
        .set_description(description)
        .set_level(MessageLevel::Warning)
        .set_buttons(MessageButtons::YesNo)
        .show()
        == MessageDialogResult::Yes)
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
    let mut child = Command::new(exe);
    child.arg(command);
    run_dialog_process(child, input, DIALOG_TIMEOUT)
}

/// Run one dialog child to completion, killing it if it is still open at
/// `timeout`. The child's stdin stays open until it has exited, so if this
/// process ends first the child sees end of input and closes its dialog.
fn run_dialog_process(mut command: Command, input: &str, timeout: Duration) -> Result<String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Never inherit the native messaging stdout; the child's stderr is
        // Chrome's host log, which is where diagnostics belong.
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting the native dialog")?;
    let result = wait_for_reply(&mut child, input, timeout);
    if result.is_err() {
        // A child that failed or ran out of time may still be showing its
        // dialog. Its stdin is closed now, so give it a moment to close the
        // dialog itself, which a kill cannot do for one drawn elsewhere.
        // Killing a child that already exited is harmless.
        let deadline = Instant::now() + CHILD_EXIT_GRACE;
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn wait_for_reply(child: &mut Child, input: &str, timeout: Duration) -> Result<String> {
    let mut stdin = child.stdin.take().context("opening dialog input")?;
    write!(stdin, "{}\n{input}", input.len())
        .and_then(|()| stdin.flush())
        .context("sending dialog request")?;
    let stdout = child.stdout.take().context("opening dialog output")?;
    // Read on another thread so the wait for the person can time out.
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reply = String::new();
        let result = stdout
            .take(MAX_CHILD_OUTPUT_BYTES)
            .read_to_string(&mut reply)
            .map(|_| reply);
        let _ = sender.send(result);
    });
    let reply = match receiver.recv_timeout(timeout) {
        Ok(result) => result.context("reading dialog reply")?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!("native dialog was not answered in time and was closed")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("reading dialog reply failed"),
    };
    let status = child.wait().context("waiting for the native dialog")?;
    drop(stdin);
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

    #[test]
    fn request_is_length_prefixed_and_needs_no_end_of_input() {
        let mut input = "10\nTitle\nBodyextra".as_bytes();
        assert_eq!(read_request(&mut input).unwrap(), "Title\nBody");
        assert_eq!(read_request(&mut "0\n".as_bytes()).unwrap(), "");
        for bad in ["", "Title\nBody", "10\nshort", "x\n", "-1\n", "65537\n"] {
            assert!(read_request(&mut bad.as_bytes()).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_dialog_open_past_its_timeout_is_killed() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(r#"echo $$ > "$0"; exec sleep 30"#)
            .arg(&pid_file);
        let started = std::time::Instant::now();
        let error = run_dialog_process(command, "Title", Duration::from_millis(500)).unwrap_err();
        assert!(format!("{error:#}").contains("not answered in time"));
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.trim())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!alive.success(), "the timed-out dialog is still running");
    }

    #[cfg(unix)]
    #[test]
    fn a_timed_out_dialog_may_close_itself_before_it_is_killed() {
        // Like the macOS confirmation child, this one closes its dialog when
        // its stdin ends, which a kill would not give it the chance to do.
        let directory = tempfile::tempdir().unwrap();
        let closed = directory.path().join("closed");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(r#"head -c 7 >/dev/null; cat >/dev/null; echo closed > "$0""#)
            .arg(&closed);
        let error = run_dialog_process(command, "Title", Duration::from_millis(300)).unwrap_err();
        assert!(format!("{error:#}").contains("not answered in time"));
        assert!(
            closed.exists(),
            "the child was killed before it could close its dialog"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dialog_that_answers_in_time_keeps_its_reply() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("printf canceled");
        let reply = run_dialog_process(command, "Title", Duration::from_secs(10)).unwrap();
        assert_eq!(reply, "canceled");
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
