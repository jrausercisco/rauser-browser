//! The dialog child in a `scripted-dialogs` build answers from files and never
//! shows UI. Without the feature this file is empty, so no test can open a
//! real dialog.
#![cfg(feature = "scripted-dialogs")]

use std::io::Write;
use std::process::{Command, Stdio};

const HOST: &str = env!("CARGO_BIN_EXE_brauser");

fn run_dialog(command: &str, directory: &std::path::Path, input: &str) -> std::process::Output {
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
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
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
