//! AI harness discovery and setup checks (DESIGN §7.1). Native harness setup
//! calls these. Every search and
//! process run takes an injected [`HarnessEnv`], so tests never read the
//! host's own `PATH` or environment.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use brauser_protocol::HarnessAdapter;
use sha2::{Digest, Sha256};

use crate::brand::APP_NAME;
use crate::config::PROMPT_PLACEHOLDER;

pub const UNSUPPORTED_PLATFORM: &str = "AI harness setup is not supported on this platform yet";

/// The setup probe's prompt and stdin. Neither harness has a max-tokens
/// flag, so the "one-token probe" is approximated by a prompt whose answer
/// is one word.
pub const PROBE_PROMPT: &str = "Reply with the single word OK.";
pub const PROBE_STDIN: &[u8] = concat!(crate::app_name!(), " setup test.").as_bytes();

/// Every harness gets these; the harness finds its credentials under `HOME`.
pub const REQUIRED_ENV: &[&str] = &["HOME", "PATH"];
/// Where Codex keeps its login and config; unset, it uses `$HOME/.codex`.
/// Codex runs with the user's own (§14), so it is passed whenever the host
/// has it.
const CODEX_HOME: &str = "CODEX_HOME";
/// Names setup may propose for Claude Code's Bedrock, Vertex, or API-key
/// authentication. Only presence is ever reported, never a value.
const CLAUDE_AUTH_NAMES: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "AWS_PROFILE",
    "AWS_REGION",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
];

const CLAUDE_TEMPLATE: &[&str] = &[
    "-p",
    "{prompt}",
    "--tools",
    "",
    "--disallowedTools",
    "*",
    "--strict-mcp-config",
    "--setting-sources",
    "",
    "--permission-mode",
    "dontAsk",
    "--no-session-persistence",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
];
const CODEX_TEMPLATE: &[&str] = &[
    "exec",
    "--sandbox",
    "read-only",
    "--ignore-user-config",
    "--ignore-rules",
    "--strict-config",
    "--ephemeral",
    "--skip-git-repo-check",
    "--json",
    "-c",
    "approval_policy=\"never\"",
    "-c",
    "web_search=\"disabled\"",
    "-c",
    "project_doc_max_bytes=0",
    "--disable",
    "shell_tool",
    "--disable",
    "unified_exec",
    "--disable",
    "apps",
    "--disable",
    "plugins",
    "--disable",
    "multi_agent",
    "--disable",
    "hooks",
    "--disable",
    "memories",
    "--disable",
    "browser_use",
    "--disable",
    "computer_use",
    "--disable",
    "image_generation",
    "{prompt}",
];

/// Choice values that must appear in their own flag's `--help` block.
const CLAUDE_CHOICES: &[(&str, &str)] = &[
    ("--permission-mode", "dontAsk"),
    ("--output-format", "stream-json"),
];
const CODEX_CHOICES: &[(&str, &str)] = &[("--sandbox", "read-only")];

// NEEDS SPEC REVIEW: both Codex tables below were read off codex-cli 0.144.4's
// `codex features list` on 2026-09-28. A version missing here is refused.
/// Features `codex features list` reports enabled that are not needed for
/// text output, disabled on top of the template's `--disable` pairs.
const REVIEWED_EXTRA_DISABLES: &[(&str, &[&str])] = &[(
    "0.144.4",
    &[
        "in_app_browser",
        "browser_use_external",
        "browser_use_full_cdp_access",
        "code_mode_host",
        "goals",
        "remote_plugin",
        "plugin_sharing",
        "tool_suggest",
    ],
)];
/// Features reviewed as safe to leave enabled: 11 stable, then 7 whose stage
/// is `removed` but which still report `true`.
const REVIEWED_KEEP_ENABLED: &[(&str, &[&str])] = &[(
    "0.144.4",
    &[
        "auth_elicitation",
        "enable_request_compression",
        "fast_mode",
        "guardian_approval",
        "mentions_v2",
        "personality",
        "remote_compaction_v2",
        "shell_snapshot",
        "skill_mcp_dependency_install",
        "tool_call_mcp_elicitation",
        "workspace_dependencies",
        "collaboration_modes",
        "resize_all_images",
        "sqlite",
        "steer",
        "terminal_resize_reflow",
        "tool_search_always_defer_mcp_tools",
        "tui_app_server",
    ],
)];

/// Help text is indented at most this far on a flag's definition line
/// (Claude Code uses 2, clap uses 2 or 6). Deeper lines are descriptions,
/// even when they wrap to start with a flag name.
const MAX_DEFINITION_INDENT: usize = 6;
const STDERR_CAP: usize = 64 * 1024;
const PROBE_STDOUT_CAP: usize = 1024 * 1024;
const MAX_PROBE_SECS: u32 = 60;
/// How much stderr a failed check writes to the host's own log.
const STDERR_LOG_BYTES: usize = 2 * 1024;

pub const VERSION_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(10),
    stdout_cap: 4 * 1024,
    stderr_cap: STDERR_CAP,
};
pub const HELP_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(10),
    stdout_cap: 256 * 1024,
    stderr_cap: STDERR_CAP,
};

pub fn probe_limits(timeout_secs: u32) -> Limits {
    Limits {
        timeout: Duration::from_secs(u64::from(timeout_secs.clamp(1, MAX_PROBE_SECS))),
        stdout_cap: PROBE_STDOUT_CAP,
        stderr_cap: STDERR_CAP,
    }
}

/// Where harness setup looks and what a harness run may inherit. There is
/// deliberately no `Debug`: `vars` holds the host's environment values.
#[derive(Clone)]
pub struct HarnessEnv {
    pub search_path: OsString,
    pub home: Option<PathBuf>,
    pub vars: BTreeMap<String, OsString>,
    /// Host-owned; each run gets a fresh `run` directory inside it.
    pub work_root: PathBuf,
}

impl HarnessEnv {
    pub fn from_process(config_dir: &Path) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
        Self {
            search_path: search_path(std::env::var_os("PATH").as_deref(), home.as_deref()),
            home,
            vars: std::env::vars_os()
                .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
                .collect(),
            work_root: config_dir.join(WORK_ROOT_NAME),
        }
    }
}

/// The directory under the config directory that holds per-run work
/// directories.
pub const WORK_ROOT_NAME: &str = "agent-work";
/// Prefixes of the work directories [`WorkDir`] creates.
const WORK_DIR_PREFIXES: [&str; 2] = ["run", "codex-home"];

/// The absolute entries of `PATH` plus `$HOME/.local/bin`, first occurrence
/// only. No other install location is added, so a test `PATH` finds only
/// what the test put there.
fn search_path(path: Option<&OsStr>, home: Option<&Path>) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let listed = path.map(|path| std::env::split_paths(path).collect::<Vec<_>>());
    for dir in listed
        .unwrap_or_default()
        .into_iter()
        .chain(home.map(|home| home.join(".local").join("bin")))
    {
        if dir.is_absolute() && !dirs.contains(&dir) && std::env::join_paths([&dir]).is_ok() {
            dirs.push(dir);
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    ClaudeCode,
    Codex,
}

impl Adapter {
    pub const ALL: [Self; 2] = [Self::ClaudeCode, Self::Codex];

    pub fn harness_id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    fn program(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// The command whose `--help` lists the template's flags.
    fn help_command(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex exec",
        }
    }

    fn choices(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::ClaudeCode => CLAUDE_CHOICES,
            Self::Codex => CODEX_CHOICES,
        }
    }

    /// Names always passed when the host has them: where the harness keeps
    /// its login and config, which a run must share with the user's own.
    pub(crate) fn home_names(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode => &[],
            Self::Codex => &[CODEX_HOME],
        }
    }

    fn auth_names(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode => CLAUDE_AUTH_NAMES,
            // Codex uses the user's own login under their normal HOME or
            // CODEX_HOME (§14).
            Self::Codex => &[],
        }
    }
}

impl From<HarnessAdapter> for Adapter {
    fn from(value: HarnessAdapter) -> Self {
        match value {
            HarnessAdapter::ClaudeCode => Self::ClaudeCode,
            HarnessAdapter::Codex => Self::Codex,
        }
    }
}

impl From<Adapter> for HarnessAdapter {
    fn from(value: Adapter) -> Self {
        match value {
            Adapter::ClaudeCode => Self::ClaudeCode,
            Adapter::Codex => Self::Codex,
        }
    }
}

/// The exact §7.1 argument template. Codex also needs the reviewed extra
/// `--disable` pairs for its version; see [`codex_args`].
pub fn template_args(adapter: Adapter) -> Vec<String> {
    let template = match adapter {
        Adapter::ClaudeCode => CLAUDE_TEMPLATE,
        Adapter::Codex => CODEX_TEMPLATE,
    };
    template.iter().map(|arg| (*arg).to_owned()).collect()
}

/// The Codex template plus the extra `--disable` pairs reviewed for this
/// version, inserted before `{prompt}`.
pub fn codex_args(version: &str) -> Result<Vec<String>> {
    let Some(extra) = reviewed(REVIEWED_EXTRA_DISABLES, version) else {
        bail!("Codex {version} has not been reviewed for {APP_NAME}");
    };
    let mut args = template_args(Adapter::Codex);
    let prompt = args
        .iter()
        .position(|arg| arg == PROMPT_PLACEHOLDER)
        .context("the Codex template has no prompt")?;
    let pairs = extra
        .iter()
        .flat_map(|feature| ["--disable".to_owned(), (*feature).to_owned()]);
    args.splice(prompt..prompt, pairs);
    Ok(args)
}

/// Whether a stored harness entry's arguments are exactly what setup would
/// write. Without a recorded Codex version, any reviewed version matches.
pub fn template_matches(adapter: Adapter, args: &[String], version: Option<&str>) -> bool {
    match (adapter, version) {
        (Adapter::ClaudeCode, _) => args == template_args(adapter).as_slice(),
        (Adapter::Codex, Some(version)) => {
            codex_args(version).is_ok_and(|expected| args == expected)
        }
        (Adapter::Codex, None) => REVIEWED_EXTRA_DISABLES
            .iter()
            .any(|(version, _)| codex_args(version).is_ok_and(|expected| args == expected)),
    }
}

fn reviewed(
    table: &[(&str, &'static [&'static str])],
    version: &str,
) -> Option<&'static [&'static str]> {
    table
        .iter()
        .find(|(reviewed, _)| *reviewed == version)
        .map(|(_, features)| *features)
}

/// A harness binary's resolved real path, size, and modification time. Both
/// harnesses update themselves in place, so any change means a new binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryIdentity {
    pub real_path: String,
    pub size: u64,
    pub mtime_ns: String,
    /// `dev:ino` on Unix.
    pub file_id: Option<String>,
}

pub fn identity(path: &Path) -> Result<BinaryIdentity> {
    let real = fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let metadata =
        fs::symlink_metadata(&real).with_context(|| format!("reading {}", real.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", real.display());
    }
    let mtime_ns = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .context("harness modification time predates 1970")?
        .as_nanos()
        .to_string();
    #[cfg(unix)]
    let file_id = {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}:{}", metadata.dev(), metadata.ino()))
    };
    #[cfg(not(unix))]
    let file_id = None;
    Ok(BinaryIdentity {
        real_path: real
            .to_str()
            .context("harness path cannot be represented as UTF-8")?
            .to_owned(),
        size: metadata.len(),
        mtime_ns,
        file_id,
    })
}

/// An environment variable name setup proposes, and whether the host has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvName {
    pub name: String,
    pub present: bool,
}

/// Every name setup may allowlist for this adapter. Others are refused.
pub fn known_env_names(adapter: Adapter) -> impl Iterator<Item = &'static str> {
    REQUIRED_ENV
        .iter()
        .chain(adapter.home_names())
        .chain(adapter.auth_names())
        .copied()
}

/// The names every run of this candidate gets: `HOME`, `PATH`, and each of
/// the adapter's home names the host has.
pub fn required_env(candidate: &Candidate) -> Vec<String> {
    let homes = candidate.adapter.home_names();
    REQUIRED_ENV
        .iter()
        .map(|name| (*name).to_owned())
        .chain(
            candidate
                .env_names
                .iter()
                .filter(|env| env.present && homes.contains(&env.name.as_str()))
                .map(|env| env.name.clone()),
        )
        .collect()
}

pub fn proposed_env_names(adapter: Adapter, env: &HarnessEnv) -> Vec<EnvName> {
    known_env_names(adapter)
        .map(|name| EnvName {
            name: name.to_owned(),
            present: env.vars.contains_key(name),
        })
        .collect()
}

/// A harness found on the search path. `refusal` is host-authored text; a
/// candidate with a refusal must not be set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub adapter: Adapter,
    pub harness_id: String,
    pub found_at: PathBuf,
    pub identity: Option<BinaryIdentity>,
    pub version: Option<String>,
    pub args: Vec<String>,
    pub env_names: Vec<EnvName>,
    pub help_sha256: Option<String>,
    pub confirmed_flags: Vec<String>,
    pub refusal: Option<String>,
}

/// Look for each known harness on the injected search path and check it.
/// This runs `--version` and `--help` only (for Codex, `exec --help` and
/// `features list`), with `HOME`, `PATH`, and the user's `CODEX_HOME`.
pub fn discover(env: &HarnessEnv) -> Vec<Candidate> {
    Adapter::ALL
        .into_iter()
        .filter_map(|adapter| {
            let found_at = find_on_path(&env.search_path, adapter.program())?;
            let mut candidate = Candidate {
                adapter,
                harness_id: adapter.harness_id().to_owned(),
                found_at,
                identity: None,
                version: None,
                args: template_args(adapter),
                env_names: proposed_env_names(adapter, env),
                help_sha256: None,
                confirmed_flags: Vec::new(),
                refusal: None,
            };
            if let Err(refusal) = examine(&mut candidate, env) {
                candidate.refusal = Some(refusal);
            }
            Some(candidate)
        })
        .collect()
}

/// The first executable file with this name, like a shell's lookup.
fn find_on_path(search_path: &OsStr, program: &str) -> Option<PathBuf> {
    std::env::split_paths(search_path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|path| fs::metadata(path).is_ok_and(|metadata| executable(&metadata)))
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
}

fn examine(candidate: &mut Candidate, env: &HarnessEnv) -> Result<(), String> {
    let adapter = candidate.adapter;
    let identity = identity(&candidate.found_at)
        .map_err(|_| format!("{} could not be read", candidate.found_at.display()))?;
    let real = PathBuf::from(&identity.real_path);
    check_install(&real)?;
    candidate.identity = Some(identity);
    let program = adapter.program();
    let required = required_env(candidate);
    let stdout = run_check(
        &real,
        program,
        &["--version"],
        &required,
        env,
        VERSION_LIMITS,
    )?;
    let version = match adapter {
        Adapter::ClaudeCode => parse_claude_version(&stdout),
        Adapter::Codex => parse_codex_version(&stdout),
    }
    .ok_or_else(|| format!("{program} --version output was not recognized"))?;
    candidate.version = Some(version.clone());
    if adapter == Adapter::Codex {
        // Only a reviewed version has a known set of extra disables.
        candidate.args = codex_args(&version).map_err(|error| error.to_string())?;
    }
    let help_argv: &[&str] = match adapter {
        Adapter::ClaudeCode => &["--help"],
        Adapter::Codex => &["exec", "--help"],
    };
    let help = run_check(&real, program, help_argv, &required, env, HELP_LIMITS)?;
    let text = std::str::from_utf8(&help).map_err(|_| {
        format!(
            "{} --help output was not recognized",
            adapter.help_command()
        )
    })?;
    let confirmed = check_help(adapter, &version, text, &candidate.args)?;
    if adapter == Adapter::Codex {
        // Listed with the same --disable pairs the runs pass, so the list
        // shows what a run would have enabled. Runs also pass
        // --ignore-user-config, which features list lacks, so it gets an
        // empty CODEX_HOME: the user's [features] must not change the result.
        let disabled = disabled_features(&candidate.args);
        let mut argv = vec!["features".to_owned(), "list".to_owned()];
        for feature in &disabled {
            argv.extend(["--disable".to_owned(), feature.clone()]);
        }
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let empty_home = WorkDir::new(&env.work_root, "codex-home").map_err(|error| {
            eprintln!("{APP_NAME}: preparing codex features list: {error:#}");
            "codex features list could not run".to_owned()
        })?;
        let mut list_env = env.clone();
        list_env.vars.insert(
            CODEX_HOME.to_owned(),
            empty_home.path().as_os_str().to_owned(),
        );
        let list_allow: Vec<String> = REQUIRED_ENV
            .iter()
            .chain([&CODEX_HOME])
            .map(|name| (*name).to_owned())
            .collect();
        let listed = run_check(&real, program, &argv, &list_allow, &list_env, HELP_LIMITS)?;
        drop(empty_home);
        let listed = std::str::from_utf8(&listed)
            .map_err(|_| format!("Codex {version} features list output was not recognized"))?;
        check_codex_features(&version, listed, &disabled)?;
    }
    candidate.confirmed_flags = confirmed;
    candidate.help_sha256 = Some(hex::encode(Sha256::digest(&help)));
    Ok(())
}

/// The real path must be an executable regular file that neither it nor
/// its directory lets another user replace.
#[cfg(unix)]
fn check_install(real: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let unreadable = |path: &Path| format!("{} could not be read", path.display());
    let metadata = fs::symlink_metadata(real).map_err(|_| unreadable(real))?;
    if !executable(&metadata) || metadata.file_type().is_symlink() {
        return Err(format!("{} is not an executable file", real.display()));
    }
    let parent = real.parent().ok_or_else(|| unreadable(real))?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|_| unreadable(parent))?;
    let uid = rustix::process::getuid().as_raw();
    for (path, metadata) in [(real, &metadata), (parent, &parent_metadata)] {
        if metadata.mode() & 0o022 != 0 {
            return Err(format!("{} is writable by other users", path.display()));
        }
        if metadata.uid() != uid && metadata.uid() != 0 {
            return Err(format!("{} is owned by another user", path.display()));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_install(_real: &Path) -> Result<(), String> {
    Err(UNSUPPORTED_PLATFORM.to_owned())
}

/// Run one setup check and return its stdout, or a host-authored refusal.
fn run_check(
    real: &Path,
    program: &str,
    argv: &[&str],
    env_allow: &[String],
    env: &HarnessEnv,
    limits: Limits,
) -> Result<Vec<u8>, String> {
    // Named by its subcommand only; the --disable pairs would crowd the error.
    let shown: Vec<&str> = argv
        .iter()
        .copied()
        .take_while(|arg| *arg != "--disable")
        .collect();
    let command = format!("{program} {}", shown.join(" "));
    let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
    let output = run_bounded(real, &argv, env_allow, env, &[], limits).map_err(|error| {
        eprintln!("{APP_NAME}: {command} could not run: {error:#}");
        format!("{command} could not run")
    })?;
    log_stderr(&command, &output.stderr);
    match output.status {
        RunStatus::Exited(0) => Ok(output.stdout),
        RunStatus::Exited(code) => Err(format!("{command} failed (exit {code})")),
        RunStatus::Signaled => Err(format!("{command} failed (killed by a signal)")),
        RunStatus::TimedOut => Err(format!("{command} timed out")),
        RunStatus::OutputTooLarge => Err(format!("{command} produced too much output")),
    }
}

fn log_stderr(command: &str, stderr: &[u8]) {
    if !stderr.is_empty() {
        let shown = &stderr[..stderr.len().min(STDERR_LOG_BYTES)];
        eprintln!(
            "{APP_NAME}: {command} stderr: {}",
            String::from_utf8_lossy(shown)
        );
    }
}

/// Exactly `<N.N.N> (Claude Code)`, returning `N.N.N`.
pub fn parse_claude_version(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?.trim();
    let version = text.strip_suffix(" (Claude Code)")?;
    numeric_version(version).then(|| version.to_owned())
}

/// Exactly `codex-cli <N.N.N>`, returning `N.N.N`.
pub fn parse_codex_version(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?.trim();
    let version = text.strip_prefix("codex-cli ")?;
    numeric_version(version).then(|| version.to_owned())
}

fn numeric_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            (1..=9).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// The flags a `--help` text defines. Only a definition line counts: its
/// trimmed start is `-`, its indent is shallow, and its definition part
/// ends at two or more spaces. A flag named only inside a description does
/// not count.
pub fn defined_flags(help: &str) -> BTreeSet<String> {
    flag_blocks(help)
        .into_iter()
        .flat_map(|block| block.flags)
        .collect()
}

/// A definition line and its description, up to the next definition line.
struct FlagBlock<'a> {
    flags: Vec<String>,
    lines: Vec<&'a str>,
}

fn flag_blocks(help: &str) -> Vec<FlagBlock<'_>> {
    let mut blocks: Vec<FlagBlock<'_>> = Vec::new();
    for line in help.lines() {
        if let Some(flags) = definition_flags(line) {
            blocks.push(FlagBlock {
                flags,
                lines: vec![line],
            });
        } else if let Some(block) = blocks.last_mut() {
            block.lines.push(line);
        }
    }
    blocks
}

fn definition_flags(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > MAX_DEFINITION_INDENT || !trimmed.starts_with('-') {
        return None;
    }
    let definition = trimmed.split("  ").next().unwrap_or(trimmed).trim_end();
    let flags: Vec<String> = definition
        .split(", ")
        .filter(|token| token.starts_with('-'))
        .filter_map(|token| token.split([' ', '<', '=', '[']).next())
        .filter(|flag| flag.len() > 1 && *flag != "--")
        .map(str::to_owned)
        .collect();
    (!flags.is_empty()).then_some(flags)
}

/// Whether `value` appears as a whole word in one of `flag`'s own blocks.
fn block_lists_choice(blocks: &[FlagBlock<'_>], flag: &str, value: &str) -> bool {
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_');
    blocks
        .iter()
        .filter(|block| block.flags.iter().any(|defined| defined == flag))
        .flat_map(|block| block.lines.iter())
        .any(|line| {
            line.match_indices(value).any(|(start, _)| {
                let before = line.as_bytes()[..start].last().copied();
                let after = line.as_bytes().get(start + value.len()).copied();
                !before.is_some_and(is_word) && !after.is_some_and(is_word)
            })
        })
}

/// Confirm every flag in `args` is defined in the help text, and every
/// required choice value is listed in its own flag's block. Returns the
/// confirmed flags. There is no fallback to other flags.
pub fn check_help(
    adapter: Adapter,
    version: &str,
    help: &str,
    args: &[String],
) -> Result<Vec<String>, String> {
    let blocks = flag_blocks(help);
    let defined: BTreeSet<&str> = blocks
        .iter()
        .flat_map(|block| block.flags.iter().map(String::as_str))
        .collect();
    let required: BTreeSet<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| arg.starts_with('-'))
        .collect();
    let name = adapter.display_name();
    let command = adapter.help_command();
    if let Some(flag) = required.iter().find(|flag| !defined.contains(**flag)) {
        return Err(format!(
            "{name} {version} does not list {flag} in {command} --help"
        ));
    }
    for (flag, value) in adapter.choices() {
        if !block_lists_choice(&blocks, flag, value) {
            return Err(format!(
                "{name} {version} does not list {value} for {flag} in {command} --help"
            ));
        }
    }
    Ok(required.into_iter().map(str::to_owned).collect())
}

/// Check `codex exec --help` (not the top-level help) for every flag the
/// reviewed arguments for this version pass.
pub fn codex_help_flags(version: &str, exec_help: &str) -> Result<Vec<String>, String> {
    let args = codex_args(version).map_err(|error| error.to_string())?;
    check_help(Adapter::Codex, version, exec_help, &args)
}

/// The features the arguments pass to `--disable`.
pub fn disabled_features(args: &[String]) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == "--disable")
        .map(|pair| pair[1].clone())
        .collect()
}

/// Check `codex features list`, run with the same `--disable` pairs. A
/// line is `name stage state`; the stage may contain a space.
pub fn check_codex_features(
    version: &str,
    list_stdout: &str,
    disabled: &[String],
) -> Result<(), String> {
    let Some(keep) = reviewed(REVIEWED_KEEP_ENABLED, version) else {
        return Err(format!(
            "Codex {version} has not been reviewed for {APP_NAME}"
        ));
    };
    let unrecognized = || format!("Codex {version} features list output was not recognized");
    let mut seen = 0;
    for line in list_stdout.lines().filter(|line| !line.trim().is_empty()) {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let (Some(name), Some(state)) = (tokens.first(), tokens.last()) else {
            return Err(unrecognized());
        };
        let enabled = match (tokens.len() >= 3, *state) {
            (true, "true") => true,
            (true, "false") => false,
            _ => return Err(unrecognized()),
        };
        seen += 1;
        if !enabled {
            continue;
        }
        if disabled.iter().any(|feature| feature == name) {
            return Err(format!(
                "Codex {version} still reports {name} enabled after --disable {name}"
            ));
        }
        if !keep.contains(name) {
            return Err(format!(
                "Codex {version} reports {name} enabled, which is not on the reviewed list"
            ));
        }
    }
    if seen == 0 {
        return Err(unrecognized());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    pub stdout_cap: usize,
    pub stderr_cap: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Exited(i32),
    Signaled,
    /// The deadline passed, or a process outside the group kept a pipe open.
    TimedOut,
    /// stdout or stderr passed its cap; the process group was killed.
    OutputTooLarge,
}

#[derive(Debug)]
pub struct RunOutput {
    pub status: RunStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run a harness binary directly (no shell) with a cleared environment
/// plus the allowlisted names, in a new empty host-owned directory of its
/// own (removed afterwards),
/// with capped output and a deadline. On timeout or overflow the whole
/// process group is killed; it is also killed after a normal exit so no
/// background child outlives the run.
#[cfg(unix)]
pub fn run_bounded(
    real_path: &Path,
    argv: &[String],
    env_allow: &[String],
    env: &HarnessEnv,
    stdin: &[u8],
    limits: Limits,
) -> Result<RunOutput> {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use rustix::process::{Pid, Signal, kill_process_group};

    const POLL: Duration = Duration::from_millis(10);
    const DRAIN_GRACE: Duration = Duration::from_secs(2);

    let run_dir = WorkDir::new(&env.work_root, "run")?;
    let mut command = Command::new(real_path);
    command
        .args(argv)
        .env_clear()
        .current_dir(run_dir.path())
        .stdin(if stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for name in env_allow {
        if let Some(value) = env.vars.get(name) {
            command.env(name, value);
        }
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", real_path.display()))?;
    let group = Pid::from_child(&child);
    let kill_group = || {
        let _ = kill_process_group(group, Signal::KILL);
    };

    if let Some(mut input) = child.stdin.take() {
        let buffer = stdin.to_vec();
        // Dropping the pipe after the write closes the harness's stdin.
        thread::spawn(move || {
            let _ = input.write_all(&buffer);
        });
    }
    let overflow = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    fn drain<R: std::io::Read + Send + 'static>(
        stream: Option<R>,
        index: usize,
        cap: usize,
        overflow: &Arc<AtomicBool>,
        sender: &mpsc::Sender<(usize, Vec<u8>)>,
    ) {
        let overflow = Arc::clone(overflow);
        let sender = sender.clone();
        thread::spawn(move || {
            let captured = stream.map(|stream| read_capped(stream, cap, &overflow));
            let _ = sender.send((index, captured.unwrap_or_default()));
        });
    }
    drain(
        child.stdout.take(),
        0,
        limits.stdout_cap,
        &overflow,
        &sender,
    );
    drain(
        child.stderr.take(),
        1,
        limits.stderr_cap,
        &overflow,
        &sender,
    );
    drop(sender);

    let deadline = Instant::now() + limits.timeout;
    let mut status = loop {
        match child.try_wait() {
            Ok(Some(exit)) => {
                break match exit.code() {
                    Some(code) => RunStatus::Exited(code),
                    None => RunStatus::Signaled,
                };
            }
            Ok(None) => {}
            Err(error) => {
                kill_group();
                let _ = child.wait();
                return Err(error).context("waiting for the AI harness");
            }
        }
        if overflow.load(Ordering::SeqCst) {
            break RunStatus::OutputTooLarge;
        }
        if Instant::now() >= deadline {
            break RunStatus::TimedOut;
        }
        thread::sleep(POLL);
    };
    kill_group();
    let _ = child.wait();

    let mut outputs = [Vec::new(), Vec::new()];
    let drain_deadline = Instant::now() + DRAIN_GRACE;
    for _ in 0..outputs.len() {
        let wait = drain_deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(wait) {
            Ok((index, captured)) => outputs[index] = captured,
            Err(_) => {
                status = RunStatus::TimedOut;
                break;
            }
        }
    }
    if overflow.load(Ordering::SeqCst) {
        status = RunStatus::OutputTooLarge;
    }
    let [stdout, stderr] = outputs;
    Ok(RunOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(not(unix))]
pub fn run_bounded(
    _real_path: &Path,
    _argv: &[String],
    _env_allow: &[String],
    _env: &HarnessEnv,
    _stdin: &[u8],
    _limits: Limits,
) -> Result<RunOutput> {
    bail!(UNSUPPORTED_PLATFORM)
}

/// Read up to `cap` bytes. Past it, flag the overflow and stop reading so
/// the caller kills the process group.
#[cfg(unix)]
fn read_capped(
    mut stream: impl std::io::Read,
    cap: usize,
    overflow: &std::sync::atomic::AtomicBool,
) -> Vec<u8> {
    let mut captured = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if captured.len() + read > cap {
                    captured.extend_from_slice(&chunk[..cap - captured.len()]);
                    overflow.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                captured.extend_from_slice(&chunk[..read]);
            }
        }
    }
    captured
}

/// A new empty 0700 directory under the work root, removed on drop. Each
/// Chrome port runs its own host process on the same work root, so every
/// run gets a directory no other run shares.
struct WorkDir(PathBuf);

impl WorkDir {
    #[cfg(unix)]
    fn new(work_root: &Path, prefix: &str) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;

        prepare_work_root(work_root)?;
        let dir = work_root.join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self(dir))
    }

    #[cfg(not(unix))]
    fn new(_work_root: &Path, _prefix: &str) -> Result<Self> {
        bail!(UNSUPPORTED_PLATFORM)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("{APP_NAME}: removing {}: {error}", self.0.display());
        }
    }
}

/// Create the work root with mode 0700 if needed. It must be a real
/// directory owned by this user, never a symlink.
#[cfg(unix)]
fn prepare_work_root(work_root: &Path) -> Result<()> {
    use std::io::ErrorKind;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let uid = rustix::process::getuid().as_raw();
    match fs::symlink_metadata(work_root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != uid {
                bail!(
                    "{} is not a directory owned by {APP_NAME}",
                    work_root.display()
                );
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if let Some(parent) = work_root.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            match fs::DirBuilder::new().mode(0o700).create(work_root) {
                // Another host process created it first; check it again.
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    return prepare_work_root(work_root);
                }
                result => result.with_context(|| format!("creating {}", work_root.display()))?,
            }
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", work_root.display()));
        }
    }
    fs::set_permissions(work_root, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("securing {}", work_root.display()))
}

/// Remove work directories a killed host left behind: real directories named
/// as [`WorkDir`] names them and not modified for `max_age`. A work root that
/// is missing, a symlink, or not a directory is left alone. Returns how many
/// were removed.
pub fn sweep_stale_work_dirs(work_root: &Path, max_age: Duration) -> Result<usize> {
    match fs::symlink_metadata(work_root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", work_root.display()));
        }
    }
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in
        fs::read_dir(work_root).with_context(|| format!("listing {}", work_root.display()))?
    {
        let entry = entry.with_context(|| format!("listing {}", work_root.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let generated = WORK_DIR_PREFIXES.iter().any(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        });
        if !generated {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let stale = metadata.is_dir()
            && metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|elapsed| elapsed >= max_age);
        if stale && fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Why the setup probe refused a harness. The text is host-authored; the
/// harness's own output is never returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFailure {
    Unavailable(String),
    Failed(Option<i32>),
    TimedOut,
    ReportedError,
    ToolUse,
    UnexpectedOutput,
}

impl fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => f.write_str(message),
            Self::Failed(Some(code)) => write!(f, "test run failed (exit {code})"),
            Self::Failed(None) => f.write_str("test run failed (killed by a signal)"),
            Self::TimedOut => f.write_str("timed out"),
            Self::ReportedError => f.write_str("reported an error"),
            Self::ToolUse => f.write_str("tried to use a tool"),
            Self::UnexpectedOutput => f.write_str("produced unexpected output"),
        }
    }
}

/// Run the setup probe: the full template argv with [`PROBE_PROMPT`] and
/// [`PROBE_STDIN`], under the harness's timeout capped at 60 seconds.
pub fn probe(
    candidate: &Candidate,
    env_allow: &[String],
    env: &HarnessEnv,
    timeout_secs: u32,
) -> Result<(), ProbeFailure> {
    probe_with_limits(candidate, env_allow, env, probe_limits(timeout_secs))
}

fn probe_with_limits(
    candidate: &Candidate,
    env_allow: &[String],
    env: &HarnessEnv,
    limits: Limits,
) -> Result<(), ProbeFailure> {
    if let Some(refusal) = &candidate.refusal {
        return Err(ProbeFailure::Unavailable(refusal.clone()));
    }
    let Some(checked) = &candidate.identity else {
        return Err(ProbeFailure::Unavailable(
            "the harness was not checked".to_owned(),
        ));
    };
    // The binary must still be the one --version and --help checked.
    let real = Path::new(&checked.real_path);
    if identity(real).ok().as_ref() != Some(checked) {
        return Err(ProbeFailure::Unavailable(
            "the harness changed on disk; run setup again".to_owned(),
        ));
    }
    check_install(real).map_err(ProbeFailure::Unavailable)?;
    let argv: Vec<String> = candidate
        .args
        .iter()
        .map(|arg| {
            if arg == PROMPT_PLACEHOLDER {
                PROBE_PROMPT.to_owned()
            } else {
                arg.clone()
            }
        })
        .collect();
    let output =
        run_bounded(real, &argv, env_allow, env, PROBE_STDIN, limits).map_err(|error| {
            eprintln!("{APP_NAME}: the harness test run could not start: {error:#}");
            ProbeFailure::Unavailable("the test run could not start".to_owned())
        })?;
    log_stderr("the harness test run", &output.stderr);
    match candidate.adapter {
        Adapter::ClaudeCode => judge_probe(&output),
        Adapter::Codex => judge_codex_probe(&output),
    }
}

fn exited_cleanly(status: RunStatus) -> Result<(), ProbeFailure> {
    match status {
        RunStatus::Exited(0) => Ok(()),
        RunStatus::Exited(code) => Err(ProbeFailure::Failed(Some(code))),
        RunStatus::Signaled => Err(ProbeFailure::Failed(None)),
        RunStatus::TimedOut => Err(ProbeFailure::TimedOut),
        RunStatus::OutputTooLarge => Err(ProbeFailure::UnexpectedOutput),
    }
}

/// The `codex exec --json` events a text-only run may print.
const CODEX_EVENTS: &[&str] = &[
    "thread.started",
    "turn.started",
    "item.started",
    "item.updated",
    "item.completed",
    "turn.completed",
];
/// The item types a text-only run may produce; any other (a command, a file
/// change, an MCP or web search call) is a tool.
const CODEX_TEXT_ITEMS: &[&str] = &["agent_message", "reasoning"];

/// A Codex `--json` run passes only if it exited 0, every line is a JSON
/// object with a known event type, every item is text, at least one agent
/// message completed, and the last line is `turn.completed`.
fn judge_codex_probe(output: &RunOutput) -> Result<(), ProbeFailure> {
    exited_cleanly(output.status)?;
    let text = std::str::from_utf8(&output.stdout).map_err(|_| ProbeFailure::UnexpectedOutput)?;
    let str_field = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let mut last = None;
    let mut answered = false;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|_| ProbeFailure::UnexpectedOutput)?;
        let kind = str_field(&value, "type").ok_or(ProbeFailure::UnexpectedOutput)?;
        if matches!(kind.as_str(), "turn.failed" | "error") {
            return Err(ProbeFailure::ReportedError);
        }
        if !CODEX_EVENTS.contains(&kind.as_str()) {
            return Err(ProbeFailure::UnexpectedOutput);
        }
        if kind.starts_with("item.") {
            let item = value.get("item").ok_or(ProbeFailure::UnexpectedOutput)?;
            let item_type = str_field(item, "type").ok_or(ProbeFailure::UnexpectedOutput)?;
            if !CODEX_TEXT_ITEMS.contains(&item_type.as_str()) {
                return Err(ProbeFailure::ToolUse);
            }
            answered |= kind == "item.completed" && item_type == "agent_message";
        }
        last = Some(kind);
    }
    if last.as_deref() != Some("turn.completed") || !answered {
        return Err(ProbeFailure::UnexpectedOutput);
    }
    Ok(())
}

/// A Claude Code `stream-json` run passes only if it exited 0, every line
/// is a JSON object, no tool-use block appears anywhere, and the last line
/// is a `result` with `is_error: false`.
fn judge_probe(output: &RunOutput) -> Result<(), ProbeFailure> {
    exited_cleanly(output.status)?;
    let text = std::str::from_utf8(&output.stdout).map_err(|_| ProbeFailure::UnexpectedOutput)?;
    let mut last = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|_| ProbeFailure::UnexpectedOutput)?;
        if !value.is_object() {
            return Err(ProbeFailure::UnexpectedOutput);
        }
        if mentions_tool_use(&value) {
            return Err(ProbeFailure::ToolUse);
        }
        last = Some(value);
    }
    let last = last.ok_or(ProbeFailure::UnexpectedOutput)?;
    if last.get("type").and_then(serde_json::Value::as_str) != Some("result") {
        return Err(ProbeFailure::UnexpectedOutput);
    }
    match last.get("is_error") {
        Some(serde_json::Value::Bool(false)) => Ok(()),
        Some(serde_json::Value::Bool(true)) => Err(ProbeFailure::ReportedError),
        _ => Err(ProbeFailure::UnexpectedOutput),
    }
}

/// Any object, at any depth, whose `type` is a tool-use block
/// (`tool_use`, `server_tool_use`, and the like).
fn mentions_tool_use(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind.ends_with("tool_use"))
                || object.values().any(mentions_tool_use)
        }
        serde_json::Value::Array(items) => items.iter().any(mentions_tool_use),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const CLAUDE_HELP: &str = include_str!("../tests/fixtures/harness/claude_help.txt");
    pub(crate) const CODEX_EXEC_HELP: &str =
        include_str!("../tests/fixtures/harness/codex_exec_help.txt");
    const CODEX_HELP: &str = include_str!("../tests/fixtures/harness/codex_help.txt");
    pub(crate) const CODEX_FEATURES: &str =
        include_str!("../tests/fixtures/harness/codex_features.txt");

    fn env_with(vars: &[(&str, &str)]) -> HarnessEnv {
        HarnessEnv {
            search_path: OsString::new(),
            home: None,
            vars: vars
                .iter()
                .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
                .collect(),
            work_root: PathBuf::from("/nonexistent/agent-work"),
        }
    }

    /// The fixture's features list as `codex features list --disable ...`
    /// would print it: every disabled feature reads `false`.
    pub(crate) fn features_after_disables(disabled: &[String]) -> String {
        CODEX_FEATURES
            .lines()
            .map(|line| {
                let name = line.split_whitespace().next().unwrap_or_default();
                if disabled.iter().any(|feature| feature == name) {
                    line.replace("true", "false")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Unix paths, so `/usr/bin` is absolute.
    #[cfg(unix)]
    #[test]
    fn search_path_keeps_absolute_entries_and_adds_local_bin() {
        let joined =
            std::env::join_paths(["/usr/bin", "relative/bin", "", "/bin", "/usr/bin"]).unwrap();
        let path = search_path(Some(&joined), Some(Path::new("/home/user")));
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(
            dirs,
            [
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
                Path::new("/home/user").join(".local").join("bin"),
            ]
        );
        assert!(search_path(None, None).is_empty());
    }

    #[test]
    fn templates_have_one_prompt_and_match_their_adapter() {
        for adapter in Adapter::ALL {
            let args = template_args(adapter);
            assert_eq!(
                args.iter().filter(|arg| *arg == PROMPT_PLACEHOLDER).count(),
                1
            );
        }
        let claude = template_args(Adapter::ClaudeCode);
        assert!(template_matches(Adapter::ClaudeCode, &claude, None));
        assert!(!template_matches(
            Adapter::ClaudeCode,
            &claude[..claude.len() - 1],
            None
        ));
        let codex = codex_args("0.144.4").unwrap();
        assert!(template_matches(Adapter::Codex, &codex, None));
        assert!(template_matches(Adapter::Codex, &codex, Some("0.144.4")));
        assert!(!template_matches(Adapter::Codex, &codex, Some("0.145.0")));
        assert!(!template_matches(
            Adapter::Codex,
            &template_args(Adapter::Codex),
            None
        ));
    }

    #[test]
    fn claude_version_must_parse_exactly() {
        assert_eq!(
            parse_claude_version(b"2.1.284 (Claude Code)\n").as_deref(),
            Some("2.1.284")
        );
        for bad in [
            &b"2.1.284"[..],
            b"2.1 (Claude Code)",
            b"2.1.x (Claude Code)",
            b"v2.1.284 (Claude Code)",
            b"note\n2.1.284 (Claude Code)",
            b"2.1.284 (Claude Code) beta",
            b"",
        ] {
            assert_eq!(parse_claude_version(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn claude_help_fixture_confirms_every_template_flag() {
        let args = template_args(Adapter::ClaudeCode);
        let confirmed = check_help(Adapter::ClaudeCode, "2.1.284", CLAUDE_HELP, &args).unwrap();
        assert_eq!(
            confirmed,
            [
                "--disallowedTools",
                "--include-partial-messages",
                "--no-session-persistence",
                "--output-format",
                "--permission-mode",
                "--setting-sources",
                "--strict-mcp-config",
                "--tools",
                "--verbose",
                "-p",
            ]
        );
        let defined = defined_flags(CLAUDE_HELP);
        assert!(defined.contains("--print") && defined.contains("--allowed-tools"));
        // Wrapped description lines that start with a flag are not definitions.
        assert!(!defined.contains("--permission-prompt-tool"));
    }

    #[test]
    fn flag_mentioned_only_in_description_does_not_count() {
        let help: String = CLAUDE_HELP
            .lines()
            .filter(|line| !line.trim_start().starts_with("--strict-mcp-config "))
            .map(|line| format!("{line}\n"))
            .collect();
        assert!(help.contains("add --strict-mcp-config to skip"));
        assert!(!defined_flags(&help).contains("--strict-mcp-config"));
        let refusal = check_help(
            Adapter::ClaudeCode,
            "2.1.284",
            &help,
            &template_args(Adapter::ClaudeCode),
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "Claude Code 2.1.284 does not list --strict-mcp-config in claude --help"
        );
    }

    #[test]
    fn choice_value_must_be_in_own_flag_block() {
        let help = CLAUDE_HELP
            .replace("\"dontAsk\", \"plan\"", "\"plan\"")
            .replace(
                "--print: \"host\" (the SDK host or",
                "--print: \"host\" (the SDK host, dontAsk, or",
            );
        assert!(help.contains("dontAsk"));
        let refusal = check_help(
            Adapter::ClaudeCode,
            "2.1.284",
            &help,
            &template_args(Adapter::ClaudeCode),
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "Claude Code 2.1.284 does not list dontAsk for --permission-mode in claude --help"
        );
    }

    #[test]
    fn codex_flags_checked_against_exec_help_not_top_level() {
        let confirmed = codex_help_flags("0.144.4", CODEX_EXEC_HELP).unwrap();
        assert!(confirmed.contains(&"--ephemeral".to_owned()));
        assert!(confirmed.contains(&"-c".to_owned()));
        let refusal = codex_help_flags("0.144.4", CODEX_HELP).unwrap_err();
        assert!(refusal.ends_with("in codex exec --help"), "{refusal}");
        let no_sandbox_choice = CODEX_EXEC_HELP.replace("read-only, ", "");
        assert!(
            codex_help_flags("0.144.4", &no_sandbox_choice)
                .unwrap_err()
                .contains("read-only for --sandbox")
        );
    }

    #[test]
    fn codex_features_pass_after_reviewed_disables() {
        let disabled = disabled_features(&codex_args("0.144.4").unwrap());
        assert_eq!(disabled.len(), 18);
        let listed = features_after_disables(&disabled);
        check_codex_features("0.144.4", &listed, &disabled).unwrap();
    }

    #[test]
    fn codex_features_unreviewed_enabled_feature_refuses() {
        let disabled = disabled_features(&codex_args("0.144.4").unwrap());
        let listed = format!(
            "{}\nnew_tool                             under development  true\n",
            features_after_disables(&disabled)
        );
        assert_eq!(
            check_codex_features("0.144.4", &listed, &disabled).unwrap_err(),
            "Codex 0.144.4 reports new_tool enabled, which is not on the reviewed list"
        );
    }

    #[test]
    fn codex_features_disabled_feature_still_true_refuses() {
        let disabled = disabled_features(&codex_args("0.144.4").unwrap());
        assert_eq!(
            check_codex_features("0.144.4", CODEX_FEATURES, &disabled).unwrap_err(),
            "Codex 0.144.4 still reports apps enabled after --disable apps"
        );
        for garbled in ["", "apps true", "apps stable yes"] {
            assert!(check_codex_features("0.144.4", garbled, &disabled).is_err());
        }
    }

    #[test]
    fn codex_unreviewed_version_refuses() {
        assert!(codex_args("0.145.0").is_err());
        assert!(codex_help_flags("0.145.0", CODEX_EXEC_HELP).is_err());
        let disabled = disabled_features(&codex_args("0.144.4").unwrap());
        assert_eq!(
            check_codex_features("0.145.0", &features_after_disables(&disabled), &disabled)
                .unwrap_err(),
            "Codex 0.145.0 has not been reviewed for Brauser"
        );
    }

    #[test]
    fn codex_reviewed_args_add_extra_disables_before_prompt() {
        let template = template_args(Adapter::Codex);
        let args = codex_args("0.144.4").unwrap();
        assert_eq!(args.len(), template.len() + 16);
        assert_eq!(&args[..template.len() - 1], &template[..template.len() - 1]);
        assert_eq!(args.last().map(String::as_str), Some(PROMPT_PLACEHOLDER));
        let extra: Vec<&str> = args[template.len() - 1..args.len() - 1]
            .chunks(2)
            .map(|pair| {
                assert_eq!(pair[0], "--disable");
                pair[1].as_str()
            })
            .collect();
        assert_eq!(extra, reviewed(REVIEWED_EXTRA_DISABLES, "0.144.4").unwrap());
    }

    #[test]
    fn proposed_env_names_report_presence_never_values() {
        let env = env_with(&[
            ("HOME", "/home/user"),
            ("ANTHROPIC_API_KEY", "sk-secret-value"),
            ("UNRELATED_TOKEN", "other-secret"),
        ]);
        let names = proposed_env_names(Adapter::ClaudeCode, &env);
        let present: Vec<&str> = names
            .iter()
            .filter(|name| name.present)
            .map(|name| name.name.as_str())
            .collect();
        assert_eq!(present, ["HOME", "ANTHROPIC_API_KEY"]);
        assert_eq!(
            names
                .iter()
                .map(|name| name.name.as_str())
                .collect::<Vec<_>>(),
            known_env_names(Adapter::ClaudeCode).collect::<Vec<_>>()
        );
        let shown = format!("{names:?}");
        assert!(!shown.contains("secret") && !shown.contains("UNRELATED_TOKEN"));
        assert_eq!(
            known_env_names(Adapter::Codex).collect::<Vec<_>>(),
            ["HOME", "PATH", "CODEX_HOME"]
        );
    }

    fn output(status: RunStatus, stdout: &str) -> RunOutput {
        RunOutput {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    const PASSING: &str = "{\"type\":\"system\",\"subtype\":\"init\",\"tools\":[]}\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"OK\"}\n";

    #[test]
    fn probe_judgement_needs_clean_exit_json_and_final_result() {
        assert_eq!(judge_probe(&output(RunStatus::Exited(0), PASSING)), Ok(()));
        for (status, stdout, expected) in [
            (RunStatus::Exited(2), PASSING, ProbeFailure::Failed(Some(2))),
            (RunStatus::Signaled, PASSING, ProbeFailure::Failed(None)),
            (RunStatus::TimedOut, PASSING, ProbeFailure::TimedOut),
            (
                RunStatus::OutputTooLarge,
                PASSING,
                ProbeFailure::UnexpectedOutput,
            ),
            (RunStatus::Exited(0), "", ProbeFailure::UnexpectedOutput),
            (
                RunStatus::Exited(0),
                "[1]\n",
                ProbeFailure::UnexpectedOutput,
            ),
            (
                RunStatus::Exited(0),
                "{\"type\":\"result\"}\n",
                ProbeFailure::UnexpectedOutput,
            ),
            (
                RunStatus::Exited(0),
                "{\"type\":\"result\",\"is_error\":false}\n{\"type\":\"assistant\"}\n",
                ProbeFailure::UnexpectedOutput,
            ),
            (
                RunStatus::Exited(0),
                "{\"type\":\"stream_event\",\"event\":{\"content_block\":{\"type\":\"server_tool_use\"}}}\n{\"type\":\"result\",\"is_error\":false}\n",
                ProbeFailure::ToolUse,
            ),
        ] {
            assert_eq!(
                judge_probe(&output(status, stdout)),
                Err(expected),
                "{stdout}"
            );
        }
        assert_eq!(
            ProbeFailure::Failed(Some(3)).to_string(),
            "test run failed (exit 3)"
        );
    }

    #[test]
    fn codex_probe_judgement_needs_text_items_and_a_completed_turn() {
        const START: &str =
            "{\"type\":\"thread.started\",\"thread_id\":\"t\"}\n{\"type\":\"turn.started\"}\n";
        const REASONING: &str = "{\"type\":\"item.completed\",\"item\":{\"id\":\"0\",\"type\":\"reasoning\",\"text\":\"x\"}}\n";
        const MESSAGE: &str = "{\"type\":\"item.completed\",\"item\":{\"id\":\"1\",\"type\":\"agent_message\",\"text\":\"OK\"}}\n";
        const DONE: &str =
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n";
        let passing = format!("{START}{REASONING}{MESSAGE}{DONE}");
        assert_eq!(
            judge_codex_probe(&output(RunStatus::Exited(0), &passing)),
            Ok(())
        );
        let command = "{\"type\":\"item.started\",\"item\":{\"id\":\"2\",\"type\":\"command_execution\",\"command\":\"ls\"}}\n";
        for (status, stdout, expected) in [
            (
                RunStatus::Exited(1),
                passing.clone(),
                ProbeFailure::Failed(Some(1)),
            ),
            (RunStatus::TimedOut, passing.clone(), ProbeFailure::TimedOut),
            (
                RunStatus::Exited(0),
                String::new(),
                ProbeFailure::UnexpectedOutput,
            ),
            (
                RunStatus::Exited(0),
                format!("{START}{command}{MESSAGE}{DONE}"),
                ProbeFailure::ToolUse,
            ),
            (
                RunStatus::Exited(0),
                format!(
                    "{START}{MESSAGE}{{\"type\":\"turn.failed\",\"error\":{{\"message\":\"x\"}}}}\n"
                ),
                ProbeFailure::ReportedError,
            ),
            (
                RunStatus::Exited(0),
                format!("{START}{{\"type\":\"error\",\"message\":\"x\"}}\n{MESSAGE}{DONE}"),
                ProbeFailure::ReportedError,
            ),
            // No answer, an answer after the turn, an unknown event, and a
            // line that is not an event are all unexpected.
            (
                RunStatus::Exited(0),
                format!("{START}{REASONING}{DONE}"),
                ProbeFailure::UnexpectedOutput,
            ),
            (
                RunStatus::Exited(0),
                format!("{START}{DONE}{MESSAGE}"),
                ProbeFailure::UnexpectedOutput,
            ),
            (
                RunStatus::Exited(0),
                format!("{START}{{\"type\":\"turn.paused\"}}\n{MESSAGE}{DONE}"),
                ProbeFailure::UnexpectedOutput,
            ),
            (
                RunStatus::Exited(0),
                format!("{START}[1]\n{MESSAGE}{DONE}"),
                ProbeFailure::UnexpectedOutput,
            ),
            (
                RunStatus::Exited(0),
                format!("{START}{{\"type\":\"item.completed\"}}\n{MESSAGE}{DONE}"),
                ProbeFailure::UnexpectedOutput,
            ),
        ] {
            assert_eq!(
                judge_codex_probe(&output(status, &stdout)),
                Err(expected),
                "{stdout}"
            );
        }
    }
}

/// Fake harnesses for process tests here and in `consent` and `native`.
/// Each is a `#!/bin/sh` script in a temp dir that logs its argv, `$0`, and
/// environment names to files there.
#[cfg(all(test, unix))]
pub(crate) mod fake {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::tests::{CLAUDE_HELP, CODEX_EXEC_HELP, features_after_disables};
    use super::{HarnessEnv, codex_args, disabled_features};

    pub(crate) const PASSING_BODY: &str = r#"cat > "$log/stdin.log"
printf '%s\n' '{"type":"system","subtype":"init","tools":[]}' '{"type":"result","subtype":"success","is_error":false,"result":"OK"}'"#;
    pub(crate) const CODEX_PASSING_BODY: &str = r#"cat > "$log/stdin.log"
printf '%s\n' '{"type":"thread.started","thread_id":"t"}' '{"type":"turn.started"}' '{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"OK"}}' '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}'"#;
    /// Names /bin/sh itself exports into `env`'s output.
    const SHELL_NAMES: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_"];

    pub(crate) struct Fake {
        root: tempfile::TempDir,
    }

    impl Fake {
        pub(crate) fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            for dir in ["bin", "log", "home"] {
                fs::create_dir(root.path().join(dir)).unwrap();
            }
            Self { root }
        }

        pub(crate) fn path(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        /// Write a fake harness at `bin/<name>`, answering `--version` with
        /// `version`, `--help` with `help`, and anything else with `body`.
        pub(crate) fn install(&self, name: &str, version: &str, help: &str, body: &str) -> PathBuf {
            let help_path = self.path(&format!("{name}-help.txt"));
            fs::write(&help_path, help).unwrap();
            let script = self.path("bin").join(name);
            self.write_script(&script, name, version, &help_path, body);
            script
        }

        pub(crate) fn write_script(
            &self,
            script: &Path,
            name: &str,
            version: &str,
            help: &Path,
            body: &str,
        ) {
            let log = self.path("log").join(name);
            fs::create_dir_all(&log).unwrap();
            let text = format!(
                r#"#!/bin/sh
log='{log}'
for arg in "$@"; do printf '[%s]' "$arg"; done >> "$log/argv.log"
echo >> "$log/argv.log"
printf '%s\n' "$0" >> "$log/argv0.log"
env | cut -d= -f1 | sort | tr '\n' ' ' >> "$log/env.log"
echo >> "$log/env.log"
case "$1" in
  --version) echo '{version}'; exit 0 ;;
  --help) cat '{help}'; exit 0 ;;
esac
{body}
"#,
                log = log.display(),
                help = help.display(),
            );
            fs::write(script, text).unwrap();
            fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        pub(crate) fn claude(&self, body: &str) -> PathBuf {
            self.install("claude", "2.1.284 (Claude Code)", CLAUDE_HELP, body)
        }

        /// A fake codex-cli 0.144.4 whose features list honors the reviewed
        /// disables.
        pub(crate) fn codex(&self, body: &str) -> PathBuf {
            let disabled = disabled_features(&codex_args("0.144.4").unwrap());
            self.codex_listing(
                "codex-cli 0.144.4",
                &features_after_disables(&disabled),
                body,
            )
        }

        /// A fake Codex answering `exec --help` with the reviewed fixture and
        /// `features list` with `features`, whatever it is asked to disable.
        pub(crate) fn codex_listing(&self, version: &str, features: &str, body: &str) -> PathBuf {
            self.codex_with(version, CODEX_EXEC_HELP, features, body)
        }

        /// A fake Codex answering `exec --help` with `exec_help` and
        /// `features list` with `features`.
        pub(crate) fn codex_with(
            &self,
            version: &str,
            exec_help: &str,
            features: &str,
            body: &str,
        ) -> PathBuf {
            let help = self.path("codex-exec-help.txt");
            fs::write(&help, exec_help).unwrap();
            let listed = self.path("codex-features.txt");
            fs::write(&listed, features).unwrap();
            let body = format!(
                r#"if [ "$1" = exec ] && [ "$2" = --help ]; then cat '{help}'; exit 0; fi
if [ "$1" = features ] && [ "$2" = list ]; then
  home="${{CODEX_HOME:-$HOME/.codex}}"
  printf '%s %s\n' "$home" "$(ls -A "$home" 2>/dev/null | tr '\n' ' ')" >> "$log/codex-home.log"
  cat '{listed}'
  # Like codex-cli, the list reflects [features] in the user's config.toml.
  if [ -f "$home/config.toml" ]; then echo 'user_dev_feature under development true'; fi
  exit 0
fi
{body}"#,
                help = help.display(),
                listed = listed.display(),
            );
            self.install("codex", version, "", &body)
        }

        pub(crate) fn env(&self, extra: &[(&str, &str)]) -> HarnessEnv {
            let home = self.path("home");
            let mut vars = BTreeMap::from([
                ("HOME".to_owned(), OsString::from(&home)),
                ("PATH".to_owned(), OsString::from("/usr/bin:/bin")),
            ]);
            for (name, value) in extra {
                vars.insert((*name).to_owned(), OsString::from(value));
            }
            HarnessEnv {
                search_path: self.path("bin").into_os_string(),
                home: Some(home),
                vars,
                work_root: self.path("config").join("agent-work"),
            }
        }

        pub(crate) fn log(&self, name: &str, file: &str) -> Option<String> {
            fs::read_to_string(self.path("log").join(name).join(file)).ok()
        }

        /// Each run's environment names, without the ones /bin/sh adds.
        pub(crate) fn env_names(&self, name: &str) -> Vec<Vec<String>> {
            self.log(name, "env.log")
                .unwrap_or_default()
                .lines()
                .map(|line| {
                    line.split_whitespace()
                        .filter(|name| !SHELL_NAMES.contains(name))
                        .map(str::to_owned)
                        .collect()
                })
                .collect()
        }
    }
}

/// Process tests, using the fake harnesses in [`fake`].
#[cfg(all(test, unix))]
mod process_tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::time::Instant;

    use super::fake::{CODEX_PASSING_BODY, Fake, PASSING_BODY};
    use super::tests::CLAUDE_HELP;
    use super::*;

    fn claude_candidate(env: &HarnessEnv) -> Candidate {
        let candidate = discover(env)
            .into_iter()
            .find(|candidate| candidate.adapter == Adapter::ClaudeCode)
            .unwrap();
        assert_eq!(candidate.refusal, None);
        candidate
    }

    fn allow(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn quick() -> Limits {
        Limits {
            timeout: Duration::from_secs(20),
            ..probe_limits(60)
        }
    }

    #[test]
    fn discover_uses_only_injected_search_path() {
        let fake = Fake::new();
        let script = fake.claude(PASSING_BODY);
        let env = fake.env(&[]);
        let found = discover(&env);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].harness_id, "claude-code");
        assert_eq!(found[0].found_at, script);

        // No search path means nothing is found, even with a real harness
        // installed for this user. Relative entries are never searched.
        for search_path in ["", "bin"] {
            let empty = HarnessEnv {
                search_path: OsString::from(search_path),
                ..fake.env(&[])
            };
            assert!(discover(&empty).is_empty());
        }
    }

    #[test]
    fn discover_follows_symlink_to_real_path_identity() {
        let fake = Fake::new();
        let real_dir = fake.path("versions");
        fs::create_dir(&real_dir).unwrap();
        let help = fake.path("help.txt");
        fs::write(&help, CLAUDE_HELP).unwrap();
        let real = real_dir.join("2.1.284");
        fake.write_script(
            &real,
            "claude",
            "2.1.284 (Claude Code)",
            &help,
            PASSING_BODY,
        );
        let link = fake.path("bin").join("claude");
        symlink(&real, &link).unwrap();

        let candidate = claude_candidate(&fake.env(&[]));
        let identity = candidate.identity.clone().unwrap();
        assert_eq!(candidate.found_at, link);
        assert_eq!(identity, super::identity(&real).unwrap());
        assert_eq!(
            PathBuf::from(&identity.real_path),
            fs::canonicalize(&real).unwrap()
        );
        assert_eq!(candidate.version.as_deref(), Some("2.1.284"));
        // The host runs the canonical path it checked, never the link.
        let argv0 = fake.log("claude", "argv0.log").unwrap();
        assert_eq!(
            argv0.lines().collect::<Vec<_>>(),
            [identity.real_path.as_str(); 2]
        );
    }

    #[test]
    fn refuses_world_writable_binary_or_dir() {
        for (binary_mode, dir_mode) in [
            (0o777, 0o755),
            (0o775, 0o755),
            (0o755, 0o777),
            (0o755, 0o775),
        ] {
            let fake = Fake::new();
            let script = fake.claude(PASSING_BODY);
            fs::set_permissions(&script, fs::Permissions::from_mode(binary_mode)).unwrap();
            fs::set_permissions(fake.path("bin"), fs::Permissions::from_mode(dir_mode)).unwrap();
            let found = discover(&fake.env(&[]));
            let refusal = found[0].refusal.clone().unwrap();
            assert!(refusal.ends_with("is writable by other users"), "{refusal}");
            assert_eq!(fake.log("claude", "argv.log"), None);
            fs::set_permissions(fake.path("bin"), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn discover_refuses_unparseable_version() {
        let fake = Fake::new();
        fake.install("claude", "2.1.284", CLAUDE_HELP, PASSING_BODY);
        let found = discover(&fake.env(&[]));
        assert_eq!(
            found[0].refusal.as_deref(),
            Some("claude --version output was not recognized")
        );
        assert_eq!(found[0].version, None);
        assert_eq!(
            fake.log("claude", "argv.log").as_deref(),
            Some("[--version]\n")
        );
    }

    fn codex_candidate(env: &HarnessEnv) -> Candidate {
        discover(env)
            .into_iter()
            .find(|candidate| candidate.adapter == Adapter::Codex)
            .unwrap()
    }

    #[test]
    fn codex_setup_checks_exec_help_and_features_list_with_the_disables() {
        let fake = Fake::new();
        let codex = fake.codex(CODEX_PASSING_BODY);
        let env = fake.env(&[("OPENAI_API_KEY", "sk-secret"), ("CODEX_HOME", "/x")]);
        let candidate = codex_candidate(&env);
        assert_eq!(candidate.refusal, None);
        assert_eq!(candidate.found_at, codex);
        assert_eq!(candidate.version.as_deref(), Some("0.144.4"));
        let args = codex_args("0.144.4").unwrap();
        assert_eq!(candidate.args, args);
        let disables: String = disabled_features(&args)
            .iter()
            .map(|feature| format!("[--disable][{feature}]"))
            .collect();
        assert_eq!(
            fake.log("codex", "argv.log").unwrap(),
            format!("[--version]\n[exec][--help]\n[features][list]{disables}\n")
        );
        // The user's CODEX_HOME reaches --version and --help, and the probe;
        // features list gets an empty one instead (see the next test).
        assert_eq!(fake.env_names("codex"), [["CODEX_HOME", "HOME", "PATH"]; 3]);
        assert!(candidate.env_names.contains(&EnvName {
            name: "CODEX_HOME".into(),
            present: true
        }));
        assert_eq!(
            required_env(&candidate),
            ["HOME", "PATH", "CODEX_HOME"].map(str::to_owned)
        );
        assert_eq!(
            candidate.help_sha256.as_deref(),
            Some(hex::encode(Sha256::digest(tests::CODEX_EXEC_HELP.as_bytes())).as_str())
        );
        assert!(candidate.confirmed_flags.contains(&"--disable".to_owned()));
        probe_with_limits(&candidate, &required_env(&candidate), &env, quick()).unwrap();
        assert_eq!(fake.env_names("codex")[3], ["CODEX_HOME", "HOME", "PATH"]);
        let probe_argv = fake.log("codex", "argv.log").unwrap();
        let last = probe_argv.lines().last().unwrap();
        assert!(last.starts_with("[exec][--sandbox][read-only]"), "{last}");
        assert!(last.ends_with(&format!("[{PROBE_PROMPT}]")), "{last}");
        assert_eq!(
            fake.log("codex", "stdin.log").unwrap().as_bytes(),
            PROBE_STDIN
        );
    }

    #[test]
    fn codex_features_list_ignores_the_user_config_as_runs_do() {
        // Runs pass --ignore-user-config, which features list lacks, so the
        // list runs against an empty host-owned CODEX_HOME.
        let fake = Fake::new();
        fake.codex(CODEX_PASSING_BODY);
        let codex_home = fake.path("home").join(".codex");
        fs::create_dir(&codex_home).unwrap();
        fs::write(
            codex_home.join("config.toml"),
            "[features]\nuser_dev_feature = true\n",
        )
        .unwrap();
        for extra in [vec![], vec![("CODEX_HOME", codex_home.to_str().unwrap())]] {
            let env = fake.env(&extra);
            let candidate = codex_candidate(&env);
            assert_eq!(candidate.refusal, None);
            let log = fake.log("codex", "codex-home.log").unwrap();
            let (listed_home, contents) = log.lines().last().unwrap().split_once(' ').unwrap();
            let work_root = fs::canonicalize(&env.work_root).unwrap();
            assert!(
                Path::new(listed_home).starts_with(&env.work_root)
                    || Path::new(listed_home).starts_with(&work_root),
                "{listed_home}"
            );
            assert_eq!(contents.trim(), "", "the listed CODEX_HOME was not empty");
            assert!(!Path::new(listed_home).exists(), "it is removed afterwards");
        }
        // Without CODEX_HOME in the host's environment, only HOME and PATH.
        assert_eq!(fake.env_names("codex")[0], ["HOME", "PATH"]);
        assert_eq!(fake.env_names("codex")[2], ["CODEX_HOME", "HOME", "PATH"]);
    }

    #[test]
    fn codex_unreviewed_version_is_refused_after_version_only() {
        let fake = Fake::new();
        fake.codex_listing("codex-cli 0.145.0", tests::CODEX_FEATURES, "exit 0");
        let candidate = codex_candidate(&fake.env(&[]));
        assert_eq!(
            candidate.refusal.as_deref(),
            Some("Codex 0.145.0 has not been reviewed for Brauser")
        );
        assert_eq!(
            fake.log("codex", "argv.log").as_deref(),
            Some("[--version]\n")
        );
        assert_eq!(
            probe(&candidate, &allow(REQUIRED_ENV), &fake.env(&[]), 5),
            Err(ProbeFailure::Unavailable(
                "Codex 0.145.0 has not been reviewed for Brauser".to_owned()
            ))
        );
        for version in ["0.144.4", "codex-cli 0.144", "codex-cli 0.144.4 beta"] {
            assert_eq!(parse_codex_version(version.as_bytes()), None, "{version}");
        }
        assert_eq!(
            parse_codex_version(b"codex-cli 0.144.4\n").as_deref(),
            Some("0.144.4")
        );
    }

    #[test]
    fn codex_features_still_enabled_after_disable_are_refused() {
        let fake = Fake::new();
        // The raw list: every feature it was asked to disable still reads true.
        fake.codex_listing("codex-cli 0.144.4", tests::CODEX_FEATURES, "exit 0");
        let candidate = codex_candidate(&fake.env(&[]));
        let refusal = candidate.refusal.unwrap();
        assert!(
            refusal.starts_with("Codex 0.144.4 still reports ") && refusal.contains("--disable"),
            "{refusal}"
        );
        assert!(candidate.help_sha256.is_none());
    }

    #[test]
    fn codex_probe_refuses_a_tool_item() {
        let fake = Fake::new();
        fake.codex(r#"printf '%s\n' '{"type":"item.started","item":{"id":"c","type":"command_execution"}}' '{"type":"turn.completed"}'"#);
        let env = fake.env(&[]);
        let candidate = codex_candidate(&env);
        assert_eq!(
            probe_with_limits(&candidate, &allow(REQUIRED_ENV), &env, quick()),
            Err(ProbeFailure::ToolUse)
        );
    }

    #[test]
    fn codex_unexpected_enabled_feature_is_refused_before_any_probe() {
        let fake = Fake::new();
        let disabled = disabled_features(&codex_args("0.144.4").unwrap());
        let listed = format!(
            "{}\nnew_tool                             under development  true\n",
            tests::features_after_disables(&disabled)
        );
        fake.codex_listing("codex-cli 0.144.4", &listed, CODEX_PASSING_BODY);
        let env = fake.env(&[]);
        let candidate = codex_candidate(&env);
        assert_eq!(
            candidate.refusal.as_deref(),
            Some("Codex 0.144.4 reports new_tool enabled, which is not on the reviewed list")
        );
        assert!(candidate.help_sha256.is_none());
        assert!(matches!(
            probe_with_limits(&candidate, &allow(REQUIRED_ENV), &env, quick()),
            Err(ProbeFailure::Unavailable(_))
        ));
        // Version, exec help, features list; the probe never ran.
        assert_eq!(fake.log("codex", "argv.log").unwrap().lines().count(), 3);
        assert_eq!(fake.log("codex", "stdin.log"), None);
    }

    #[test]
    fn codex_missing_exec_flag_is_refused_before_features_list() {
        let fake = Fake::new();
        let help: String = tests::CODEX_EXEC_HELP
            .lines()
            .filter(|line| !line.trim_start().starts_with("--ephemeral"))
            .map(|line| format!("{line}\n"))
            .collect();
        assert_ne!(help, tests::CODEX_EXEC_HELP);
        fake.codex_with(
            "codex-cli 0.144.4",
            &help,
            tests::CODEX_FEATURES,
            CODEX_PASSING_BODY,
        );
        let env = fake.env(&[]);
        let candidate = codex_candidate(&env);
        let refusal = candidate.refusal.clone().unwrap();
        assert!(
            refusal.contains("--ephemeral") && refusal.ends_with("in codex exec --help"),
            "{refusal}"
        );
        assert_eq!(
            fake.log("codex", "argv.log").as_deref(),
            Some("[--version]\n[exec][--help]\n")
        );
        assert!(matches!(
            probe_with_limits(&candidate, &allow(REQUIRED_ENV), &env, quick()),
            Err(ProbeFailure::Unavailable(_))
        ));
        assert_eq!(fake.log("codex", "stdin.log"), None);
    }

    #[test]
    fn codex_probe_refuses_an_event_off_the_allowlist() {
        // A known-but-unlisted event and a text item followed by an unknown
        // item type both fail the probe, even with a clean exit.
        for (event, expected) in [
            (
                r#"{"type":"session.configured","model":"x"}"#,
                ProbeFailure::UnexpectedOutput,
            ),
            (
                r#"{"type":"item.completed","item":{"id":"w","type":"web_search","query":"x"}}"#,
                ProbeFailure::ToolUse,
            ),
        ] {
            let fake = Fake::new();
            fake.codex(&format!(
                r#"cat > "$log/stdin.log"
printf '%s\n' '{{"type":"thread.started","thread_id":"t"}}' '{{"type":"turn.started"}}' '{event}' '{{"type":"item.completed","item":{{"id":"i","type":"agent_message","text":"OK"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":1,"output_tokens":1}}}}'"#
            ));
            let env = fake.env(&[]);
            let candidate = codex_candidate(&env);
            assert_eq!(candidate.refusal, None);
            assert_eq!(
                probe_with_limits(&candidate, &allow(REQUIRED_ENV), &env, quick()),
                Err(expected),
                "{event}"
            );
        }
    }

    #[test]
    fn version_and_help_run_with_only_home_and_path() {
        let fake = Fake::new();
        fake.claude(PASSING_BODY);
        let env = fake.env(&[
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("AWS_PROFILE", "work"),
            ("SECRET_TOKEN", "hunter2"),
        ]);
        let candidate = claude_candidate(&env);
        assert_eq!(
            fake.log("claude", "argv.log").as_deref(),
            Some("[--version]\n[--help]\n")
        );
        assert_eq!(
            fake.env_names("claude"),
            [["HOME", "PATH"], ["HOME", "PATH"]]
        );
        assert_eq!(candidate.version.as_deref(), Some("2.1.284"));
        assert_eq!(candidate.args, template_args(Adapter::ClaudeCode));
        assert_eq!(
            candidate.help_sha256.as_deref(),
            Some(hex::encode(Sha256::digest(CLAUDE_HELP.as_bytes())).as_str())
        );
        assert!(
            candidate
                .confirmed_flags
                .contains(&"--strict-mcp-config".to_owned())
        );
        let shown = format!("{candidate:?}");
        assert!(!shown.contains("sk-secret") && !shown.contains("hunter2"));
        assert!(candidate.env_names.contains(&EnvName {
            name: "AWS_PROFILE".into(),
            present: true
        }));
    }

    #[test]
    fn missing_flag_refuses_without_fallback() {
        let fake = Fake::new();
        let help: String = CLAUDE_HELP
            .lines()
            .filter(|line| !line.trim_start().starts_with("--setting-sources "))
            .map(|line| format!("{line}\n"))
            .collect();
        fake.install("claude", "2.1.284 (Claude Code)", &help, PASSING_BODY);
        let found = discover(&fake.env(&[]));
        assert_eq!(
            found[0].refusal.as_deref(),
            Some("Claude Code 2.1.284 does not list --setting-sources in claude --help")
        );
        assert!(found[0].confirmed_flags.is_empty());
        assert_eq!(
            fake.log("claude", "argv.log").as_deref(),
            Some("[--version]\n[--help]\n")
        );
    }

    #[test]
    fn probe_passes_full_template_argv_and_fixed_stdin() {
        let fake = Fake::new();
        fake.claude(PASSING_BODY);
        let env = fake.env(&[]);
        let candidate = claude_candidate(&env);
        probe(&candidate, &allow(REQUIRED_ENV), &env, 30).unwrap();
        let argv = fake.log("claude", "argv.log").unwrap();
        assert_eq!(
            argv.lines().last(),
            Some(
                "[-p][Reply with the single word OK.][--tools][][--disallowedTools][*][--strict-mcp-config][--setting-sources][][--permission-mode][dontAsk][--no-session-persistence][--output-format][stream-json][--verbose][--include-partial-messages]"
            )
        );
        assert_eq!(
            fake.log("claude", "stdin.log").as_deref(),
            Some("Brauser setup test.")
        );
    }

    #[test]
    fn probe_env_is_cleared_to_allowlist() {
        let fake = Fake::new();
        fake.claude(PASSING_BODY);
        let env = fake.env(&[("AWS_PROFILE", "work"), ("SECRET_TOKEN", "hunter2")]);
        let candidate = claude_candidate(&env);
        probe(
            &candidate,
            &allow(&["HOME", "PATH", "AWS_PROFILE", "AWS_REGION"]),
            &env,
            30,
        )
        .unwrap();
        let runs = fake.env_names("claude");
        assert_eq!(runs.last().unwrap(), &["AWS_PROFILE", "HOME", "PATH"]);
    }

    fn probe_body(body: &str) -> Result<(), ProbeFailure> {
        let fake = Fake::new();
        fake.claude(body);
        let env = fake.env(&[]);
        probe_with_limits(&claude_candidate(&env), &allow(REQUIRED_ENV), &env, quick())
    }

    #[test]
    fn probe_refuses_nonzero_exit() {
        let body =
            r#"printf '%s\n' '{"type":"result","subtype":"success","is_error":false}'; exit 3"#;
        assert_eq!(probe_body(body), Err(ProbeFailure::Failed(Some(3))));
    }

    #[test]
    fn probe_refuses_result_is_error_true_with_subtype_success() {
        let body = r#"printf '%s\n' '{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key"}'"#;
        let failure = probe_body(body).unwrap_err();
        assert_eq!(failure, ProbeFailure::ReportedError);
        assert!(!failure.to_string().contains("API key"));
    }

    #[test]
    fn probe_refuses_tool_use() {
        let body = r#"printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}' '{"type":"result","subtype":"success","is_error":false}'"#;
        assert_eq!(probe_body(body), Err(ProbeFailure::ToolUse));
    }

    #[test]
    fn probe_refuses_non_json_line() {
        let body = r#"echo 'Warning: something'; printf '%s\n' '{"type":"result","subtype":"success","is_error":false}'"#;
        assert_eq!(probe_body(body), Err(ProbeFailure::UnexpectedOutput));
    }

    #[test]
    fn probe_timeout_kills_process_group() {
        let fake = Fake::new();
        fake.claude(r#"sleep 30 & echo $! > "$log/child.pid"; sleep 30"#);
        let env = fake.env(&[]);
        let limits = Limits {
            timeout: Duration::from_secs(1),
            ..probe_limits(60)
        };
        let started = Instant::now();
        assert_eq!(
            probe_with_limits(&claude_candidate(&env), &allow(REQUIRED_ENV), &env, limits),
            Err(ProbeFailure::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: i32 = fake
            .log("claude", "child.pid")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rustix::process::test_kill_process(pid).is_ok() {
            assert!(Instant::now() < deadline, "background child survived");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn probe_output_cap_kills_and_fails() {
        let started = Instant::now();
        assert_eq!(
            probe_body(r#"yes '{"type":"stream_event"}'"#),
            Err(ProbeFailure::UnexpectedOutput)
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn probe_runs_in_empty_host_workdir_removed_afterwards() {
        let fake = Fake::new();
        let script =
            fake.claude("ls -A; touch leftover; stat -f %Lp . 2>/dev/null || stat -c %a .; pwd -P");
        let env = fake.env(&[]);
        let mut seen = Vec::new();
        for _ in 0..2 {
            let outcome = run_bounded(&script, &allow(&["run"]), &[], &env, &[], quick()).unwrap();
            assert_eq!(outcome.status, RunStatus::Exited(0));
            let stdout = String::from_utf8(outcome.stdout).unwrap();
            let (mode, dir) = stdout.trim_end().split_once('\n').unwrap();
            assert_eq!(mode, "700", "{stdout}");
            let dir = PathBuf::from(dir);
            assert_eq!(
                dir.parent(),
                Some(fs::canonicalize(&env.work_root).unwrap().as_path())
            );
            assert!(!dir.exists(), "{} was left behind", dir.display());
            seen.push(dir);
        }
        assert_ne!(seen[0], seen[1]);
        let mode = fs::metadata(&env.work_root).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn concurrent_runs_do_not_share_a_workdir() {
        // Each Chrome port starts its own host process on the same work root.
        let fake = Fake::new();
        let script = fake.claude("touch mine; sleep 1; [ -e mine ] && echo kept");
        let env = fake.env(&[]);
        let first = {
            let (script, env) = (script.clone(), env.clone());
            std::thread::spawn(move || {
                run_bounded(&script, &allow(&["run"]), &[], &env, &[], quick()).unwrap()
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        let second = run_bounded(&script, &allow(&["run"]), &[], &env, &[], quick()).unwrap();
        for outcome in [first.join().unwrap(), second] {
            assert_eq!(outcome.status, RunStatus::Exited(0));
            assert_eq!(String::from_utf8(outcome.stdout).unwrap(), "kept\n");
        }
    }

    #[test]
    fn work_dir_symlink_is_refused() {
        let fake = Fake::new();
        let script = fake.claude(PASSING_BODY);
        let elsewhere = fake.path("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let env = HarnessEnv {
            work_root: fake.path("link"),
            ..fake.env(&[])
        };
        symlink(&elsewhere, &env.work_root).unwrap();
        assert!(run_bounded(&script, &allow(&["run"]), &[], &env, &[], quick()).is_err());
        assert_eq!(fake.log("claude", "argv.log"), None);
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn startup_sweep_removes_only_stale_generated_work_dirs() {
        let fake = Fake::new();
        let root = fake.path("agent-work");
        fs::create_dir(&root).unwrap();
        let id = "0123456789abcdef0123456789abcdef";
        for name in [
            format!("run-{id}"),
            format!("codex-home-{id}"),
            "run-short".into(),
            "notes".into(),
        ] {
            fs::create_dir(root.join(name)).unwrap();
        }
        fs::write(root.join(format!("run-{id}")).join("left-behind"), "x").unwrap();
        let elsewhere = fake.path("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, root.join(format!("run-{}", "f".repeat(32)))).unwrap();

        // Nothing is old enough yet.
        assert_eq!(
            sweep_stale_work_dirs(&root, Duration::from_secs(3600)).unwrap(),
            0
        );
        assert_eq!(sweep_stale_work_dirs(&root, Duration::ZERO).unwrap(), 2);
        let mut left: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["notes", &format!("run-{}", "f".repeat(32)), "run-short"]
        );
        assert!(elsewhere.exists());
        // A missing or symlinked work root is left alone.
        assert_eq!(
            sweep_stale_work_dirs(&fake.path("absent"), Duration::ZERO).unwrap(),
            0
        );
        let link = fake.path("link");
        symlink(&root, &link).unwrap();
        assert_eq!(sweep_stale_work_dirs(&link, Duration::ZERO).unwrap(), 0);
    }
}
