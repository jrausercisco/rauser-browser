//! Native harness setup end to end: the built host serves real frames, its
//! confirmation dialog answers from scripted files, and the only harness on
//! its search path is a fake `#!/bin/sh` script. The host runs with a cleared
//! environment and a temp `HOME`, so it never sees the user's real config,
//! harness, or screen. Without the feature this file is empty.
#![cfg(all(feature = "scripted-dialogs", unix))]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

const HOST: &str = env!("CARGO_BIN_EXE_brauser");
const CLAUDE_HELP: &str = include_str!("fixtures/harness/claude_help.txt");
const PASSING_BODY: &str = r#"cat > "$log/stdin.log"
printf '%s\n' '{"type":"system","subtype":"init","tools":[]}' '{"type":"result","subtype":"success","is_error":false,"result":"OK"}'"#;
const SECRET_NAME: &str = "ANTHROPIC_API_KEY";
const SECRET: &str = "sk-test-secret-value";

/// A temp home, fake harness directory, scripted-dialog directory, notes
/// folder, and the config path the host resolves under that home.
struct World {
    root: tempfile::TempDir,
    config: PathBuf,
}

impl World {
    fn new(body: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in ["home", "bin", "log", "dialogs", "notes"] {
            fs::create_dir(root.path().join(dir)).unwrap();
        }
        fs::set_permissions(
            root.path().join("dialogs"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let world = Self {
            config: PathBuf::new(),
            root,
        };
        world.install_claude(body);
        let config = world.config_path();
        assert!(
            config.starts_with(world.path("home")),
            "the host must resolve its config under the temp HOME, not {}",
            config.display()
        );
        let world = Self { config, ..world };
        world.write_config();
        world
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn install_claude(&self, body: &str) {
        let help = self.path("claude-help.txt");
        fs::write(&help, CLAUDE_HELP).unwrap();
        let script = self.path("bin").join("claude");
        let text = format!(
            r#"#!/bin/sh
log='{log}'
for arg in "$@"; do printf '[%s]' "$arg"; done >> "$log/argv.log"
echo >> "$log/argv.log"
env | cut -d= -f1 | sort | tr '\n' ' ' >> "$log/env.log"
echo >> "$log/env.log"
case "$1" in
  --version) echo '2.1.284 (Claude Code)'; exit 0 ;;
  --help) cat '{help}'; exit 0 ;;
esac
{body}
"#,
            log = self.path("log").display(),
            help = help.display(),
        );
        fs::write(&script, text).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(HOST);
        command
            .env_clear()
            .env("HOME", self.path("home"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path("bin").display()),
            )
            .env("BRAUSER_SCRIPTED_DIALOGS", self.path("dialogs"))
            .env(SECRET_NAME, SECRET);
        command
    }

    fn config_path(&self) -> PathBuf {
        let output = self.command().arg("--config-path").output().unwrap();
        assert!(output.status.success());
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    }

    /// A config with a notes folder chosen and no harness.
    fn write_config(&self) {
        let root = fs::canonicalize(self.path("notes")).unwrap();
        let metadata = fs::metadata(&root).unwrap();
        let identity = format!("unix:{:016x}:{:016x}", metadata.dev(), metadata.ino());
        fs::create_dir_all(self.config.parent().unwrap()).unwrap();
        fs::write(
            &self.config,
            format!(
                "root_picker_confirmed = true\nroot_identity = {:?}\nagent_denylist = [\"bank.example\"]\n\n[storage]\nroot = {:?}\nprofile = \"neutral\"\nlog_dir = \"log\"\npages_dir = \"pages\"\nlater_dir = \"later\"\n",
                identity,
                root.to_str().unwrap(),
            ),
        )
        .unwrap();
    }

    fn answer(&self, reply: &str) {
        fs::write(
            self.path("dialogs").join("answer"),
            format!(r#"{{"dialog":"confirm","reply":"{reply}"}}"#),
        )
        .unwrap();
    }

    fn shown(&self) -> Vec<Value> {
        fs::read_to_string(self.path("dialogs").join("shown.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn log(&self, file: &str) -> Option<String> {
        fs::read_to_string(self.path("log").join(file)).ok()
    }

    fn serve(&self) -> Host {
        // Refuse to start unless this binary is the scripted build, so no
        // test can open a real dialog.
        let version = self.command().arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&version.stdout).contains("(scripted dialogs)"));
        let mut child = self
            .command()
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Host {
            input: Some(child.stdin.take().unwrap()),
            output: child.stdout.take().unwrap(),
            child,
            next_id: 0,
            fake_bin: self.path("bin"),
        }
    }
}

struct Host {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    next_id: u32,
    fake_bin: PathBuf,
}

impl Host {
    fn call(&mut self, mut request: Value) -> Value {
        self.next_id += 1;
        let id = format!("r{}", self.next_id);
        request["protocol_version"] = json!(4);
        request["request_id"] = json!(id);
        let body = serde_json::to_vec(&request).unwrap();
        let input = self.input.as_mut().unwrap();
        input
            .write_all(&u32::try_from(body.len()).unwrap().to_ne_bytes())
            .unwrap();
        input.write_all(&body).unwrap();
        input.flush().unwrap();
        let mut prefix = [0u8; 4];
        self.output.read_exact(&mut prefix).unwrap();
        let mut reply = vec![0u8; u32::from_ne_bytes(prefix) as usize];
        self.output.read_exact(&mut reply).unwrap();
        let reply: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(reply["request_id"], json!(id));
        reply
    }

    fn revision(&mut self) -> String {
        let reply = self.call(json!({"type": "get_config"}));
        assert_eq!(reply["type"], "config_result", "{reply}");
        reply["revision"].as_str().unwrap().to_owned()
    }

    /// The fake claude's offer. Every offer must come from the fake
    /// directory; anything else means a real harness was found.
    fn claude_offer(&mut self, revision: &str) -> String {
        let reply = self.call(json!({"type": "discover_harnesses", "expected_revision": revision}));
        assert_eq!(reply["type"], "harnesses_discovered", "{reply}");
        let offers = reply["offers"].as_array().unwrap();
        for offer in offers {
            let binary = Path::new(offer["binary"].as_str().unwrap());
            assert!(
                binary.starts_with(&self.fake_bin),
                "a harness outside the fake directory was found: {}",
                binary.display()
            );
        }
        let claude = offers
            .iter()
            .find(|offer| offer["harness_id"] == "claude-code")
            .unwrap();
        claude["offer_id"].as_str().unwrap().to_owned()
    }

    fn confirm(&mut self, revision: &str, offer_id: &str) -> Value {
        self.call(json!({
            "type": "confirm_harness_setup",
            "expected_revision": revision,
            "offer_id": offer_id,
            "env_names": [],
            "agent_denylist": ["bank.example"],
            "summaries_dir": null,
        }))
    }

    fn close(mut self) {
        drop(self.input.take());
        assert!(self.child.wait().unwrap().success());
    }
}

/// The probe's environment names: the last line of the fake's env.log.
fn probe_env(world: &World) -> Vec<String> {
    world
        .log("env.log")
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn assert_secret_absent(world: &World) {
    for file in ["argv.log", "env.log", "stdin.log"] {
        assert!(
            !world.log(file).unwrap_or_default().contains(SECRET),
            "{file}"
        );
    }
    let shown = fs::read_to_string(world.path("dialogs").join("shown.jsonl")).unwrap_or_default();
    assert!(!shown.contains(SECRET));
    assert!(!fs::read_to_string(&world.config).unwrap().contains(SECRET));
}

#[test]
fn harness_setup_confirms_and_commits_flag() {
    let world = World::new(PASSING_BODY);
    let mut host = world.serve();
    let revision = host.revision();
    let offer = host.claude_offer(&revision);
    world.answer("confirmed");
    let confirmed = host.confirm(&revision, &offer);
    assert_eq!(confirmed["type"], "harness_setup_confirmed", "{confirmed}");

    let shown = world.shown();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0]["dialog"], "confirm");
    let text = shown[0]["text"].as_str().unwrap();
    // The fixture has no summaries folder yet, so setting it is listed first.
    assert!(
        text.starts_with(
            "Set up Claude Code for Brauser AI commands\nThis setup also makes these changes:\n  Set summaries folder to \"summaries\".\n\nHarness: Claude Code 2.1.284\n"
        ),
        "{text}"
    );
    assert!(text.contains(&format!(
        "Program: {:?}",
        world.path("bin").join("claude").to_str().unwrap()
    )));
    assert!(text.contains("  \"bank.example\""));
    assert_eq!(
        confirmed["summary"].as_str(),
        text.split_once('\n').map(|(_, body)| body)
    );

    // The probe ran with only the allowlisted names.
    let names = probe_env(&world);
    assert!(names.contains(&"HOME".to_owned()) && names.contains(&"PATH".to_owned()));
    assert!(!names.contains(&"BRAUSER_SCRIPTED_DIALOGS".to_owned()));
    assert!(!names.contains(&SECRET_NAME.to_owned()));
    assert_eq!(
        world.log("stdin.log").as_deref(),
        Some("Brauser setup test.")
    );

    let updated = host.call(json!({
        "type": "update_config",
        "expected_revision": revision,
        "config": confirmed["config"],
        "picker_token": null,
        "consent_token": null,
        "harness_token": confirmed["harness_token"],
    }));
    assert_eq!(updated["type"], "config_updated", "{updated}");
    let checked = host.call(json!({"type": "check_agent", "url": "https://mail.bank.example/"}));
    assert_eq!(checked["type"], "agent_checked", "{checked}");
    assert_eq!(checked["harness_version"], "2.1.284");
    assert_eq!(checked["url_allowed"], false);
    host.close();

    let saved = fs::read_to_string(&world.config).unwrap();
    assert!(saved.contains("agent_denylist_confirmed = true"), "{saved}");
    assert!(saved.contains("[harness_record]"), "{saved}");
    assert!(saved.contains("summaries_dir = \"summaries\""), "{saved}");
    assert_secret_absent(&world);
}

#[test]
fn canceled_harness_setup_leaves_config_unchanged_and_runs_no_probe() {
    let world = World::new(PASSING_BODY);
    let before = fs::read(&world.config).unwrap();
    let mut host = world.serve();
    let revision = host.revision();
    let offer = host.claude_offer(&revision);
    world.answer("canceled");
    let reply = host.confirm(&revision, &offer);
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["code"], "cancelled");
    assert_eq!(world.shown().len(), 1);
    // The canceled offer is spent.
    let again = host.confirm(&revision, &offer);
    assert_eq!(again["code"], "unauthorized", "{again}");
    let status = host.call(json!({"type": "get_config"}));
    assert_eq!(status["agent_status"]["state"], "not_set_up");
    host.close();

    assert_eq!(fs::read(&world.config).unwrap(), before);
    assert_eq!(world.log("stdin.log"), None);
    // Only --version and --help ran.
    assert_eq!(world.log("argv.log").unwrap().lines().count(), 2);
    assert_secret_absent(&world);
}

#[test]
fn failing_probe_is_refused_after_confirmation() {
    let world = World::new("exit 3");
    let before = fs::read(&world.config).unwrap();
    let mut host = world.serve();
    let revision = host.revision();
    let offer = host.claude_offer(&revision);
    world.answer("confirmed");
    let reply = host.confirm(&revision, &offer);
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["code"], "invalid_config");
    assert_eq!(
        reply["message"],
        "Claude Code failed its test run (test run failed (exit 3)); AI commands stay off"
    );
    assert_eq!(world.shown().len(), 1);
    let status = host.call(json!({"type": "get_config"}));
    assert_eq!(status["agent_status"]["state"], "not_set_up");
    host.close();

    assert_eq!(fs::read(&world.config).unwrap(), before);
    assert_eq!(world.log("argv.log").unwrap().lines().count(), 3);
    assert_secret_absent(&world);
}
