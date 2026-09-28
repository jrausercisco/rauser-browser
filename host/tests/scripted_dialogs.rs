//! The dialog child in a `scripted-dialogs` build answers from files and never
//! shows UI. Without the feature this file is empty, so no test can open a
//! real dialog.
#![cfg(feature = "scripted-dialogs")]

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::time::{Duration, Instant};

const HOST: &str = env!("CARGO_BIN_EXE_brauser");

/// Start a dialog child and send its request the way the parent host does:
/// a byte-length line, then the text. The returned stdin is the parent's end
/// of the pipe, which the child watches to learn that its parent is gone.
fn start_dialog(command: &str, directory: &std::path::Path, input: &str) -> (Child, ChildStdin) {
    // Refuse to start a dialog child unless this binary is the scripted build.
    let version = Command::new(HOST).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&version.stdout).contains("(scripted dialogs)"));
    let mut child = Command::new(HOST)
        .arg(command)
        .env("BRAUSER_SCRIPTED_DIALOGS", directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    write!(stdin, "{}\n{input}", input.len()).unwrap();
    stdin.flush().unwrap();
    (child, stdin)
}

/// Run a dialog to completion while its parent stays alive.
fn run_dialog(command: &str, directory: &std::path::Path, input: &str) -> Output {
    let (child, stdin) = start_dialog(command, directory, input);
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    output
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn child_replies_with_the_scripted_folder() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("answer"),
        r#"{"dialog":"pick-folder","reply":"picked:/tmp/notes"}"#,
    )
    .unwrap();
    let output = run_dialog(
        "__dialog-pick-folder",
        directory.path(),
        "Choose notes folder",
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"picked:/tmp/notes");
    let shown = std::fs::read_to_string(directory.path().join("shown.jsonl")).unwrap();
    assert!(shown.contains(r#""dialog":"pick-folder","text":"Choose notes folder""#));
}

#[test]
fn child_fails_closed_on_a_mismatched_answer() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("answer"),
        r#"{"dialog":"pick-folder","reply":"canceled"}"#,
    )
    .unwrap();
    let output = run_dialog("__dialog-confirm", directory.path(), "Title\nBody");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn child_rejects_a_request_without_its_length() {
    let directory = tempfile::tempdir().unwrap();
    let mut child = Command::new(HOST)
        .arg("__dialog-confirm")
        .env("BRAUSER_SCRIPTED_DIALOGS", directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The pre-framing request shape: bare text ended by closing the pipe.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Title\nBody")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!directory.path().join("shown.jsonl").exists());
}

#[test]
fn child_closes_its_dialog_when_the_parent_goes_away() {
    let directory = tempfile::tempdir().unwrap();
    let (mut child, stdin) = start_dialog("__dialog-confirm", directory.path(), "Title\nBody");
    // Wait until the dialog is open, then close the parent's end of the pipe,
    // as a host that was killed or disconnected would. No answer arrives.
    let shown = directory.path().join("shown.jsonl");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !shown.exists() {
        assert!(Instant::now() < deadline, "the dialog never opened");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stdin);
    let status =
        wait_for_exit(&mut child, Duration::from_secs(10)).expect("the dialog outlived its parent");
    assert!(!status.success());
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    assert!(stdout.is_empty());
}

#[test]
fn killing_the_host_closes_its_open_dialog() {
    let home = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut host = Command::new(HOST)
        .arg("serve")
        .env("HOME", home.path())
        .env("BRAUSER_SCRIPTED_DIALOGS", directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let request = format!(
        r#"{{"type":"choose_folder","protocol_version":{},"request_id":"pick"}}"#,
        brauser_protocol::PROTOCOL_VERSION
    );
    let mut stdin = host.stdin.take().unwrap();
    stdin
        .write_all(&(request.len() as u32).to_ne_bytes())
        .unwrap();
    stdin.write_all(request.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let shown = directory.path().join("shown.jsonl");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !shown.exists() {
        assert!(Instant::now() < deadline, "the dialog never opened");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Chrome ends a disconnected host with a signal, so the host gets no
    // chance to clean up after itself.
    host.kill().unwrap();
    host.wait().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    // A dialog still open would consume this answer within 100 ms.
    let answer = directory.path().join("answer");
    std::fs::write(
        &answer,
        r#"{"dialog":"pick-folder","reply":"picked:/tmp/notes"}"#,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    assert!(answer.exists(), "the dialog outlived the host");
    drop(stdin);
}
