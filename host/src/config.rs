//! The host owns configuration. An absent file is an unconfigured, inert state.
//! An update is validated in full before the old file is atomically replaced.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use brauser_protocol::{
    AgentConfig, AgentState, AgentStatus, ConfigSnapshot, ErrorCode, HarnessAdapter, SiteConfig,
    StorageConfig,
};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::brand::{APP_NAME, NAMESPACE};
use crate::harness::{self, BinaryIdentity};
use crate::privacy;
use crate::vault::{STALE_TEMPORARY_AGE, Vault, is_generated_temporary, selected_root_identity};

const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_ROOT_PATH_BYTES: usize = 4 * 1024;
const MAX_LOCATION_PATH_BYTES: usize = 512;
const MAX_PATH_COMPONENT_BYTES: usize = 255;
const MAX_SITES: usize = 128;
const MAX_STRIP_PARAMS: usize = 64;
const MAX_RULE_BYTES: usize = 64;
const MAX_AGENT_ARGS: usize = 64;
const MAX_AGENT_ARG_BYTES: usize = 256;
const MAX_ENV_ALLOW: usize = 32;
const MAX_ENV_NAME_BYTES: usize = 64;
const MAX_TIMEOUT_SECS: u32 = 600;
pub(crate) const DEFAULT_TIMEOUT_SECS: u32 = 120;
pub(crate) const PROMPT_PLACEHOLDER: &str = "{prompt}";
const MISSING_REVISION: &str = "missing";
/// Never a real revision, so no update can be based on an unreadable file.
const UNREADABLE_REVISION: &str = "unreadable";
/// Hash at most this much of an oversized config. Past it, the revision
/// also covers the length so a growing file still changes revision.
const MAX_HASHED_CONFIG_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn default_strip_params() -> Vec<String> {
    vec!["utm_*".into(), "fbclid".into(), "gclid".into()]
}

pub(crate) const fn default_near_repeat_secs() -> u32 {
    300
}

pub struct ConfigStore {
    path: PathBuf,
    config: ConfigSnapshot,
    revision: String,
    issue: Option<String>,
    root_identity: Option<String>,
    agent: AgentView,
}

struct DiskState {
    config: ConfigSnapshot,
    revision: String,
    issue: Option<String>,
    needs_backup: bool,
    root_identity: Option<String>,
    agent: AgentView,
}

/// The raw `[agent]` table and harness record as last read from disk, and
/// what they parse to. They are parsed apart from the rest of the file, so a
/// bad harness entry disables only AI commands and capture keeps working
/// (§7.1). An update without a harness grant carries the raw tables forward.
#[derive(Clone, Default)]
struct AgentView {
    table: Option<toml::Value>,
    record_table: Option<toml::Value>,
    agent: Option<AgentConfig>,
    record: Option<HarnessRecord>,
    issue: Option<String>,
}

const AGENT_ENTRY_INVALID: &str = concat!(
    "The AI harness entry in config.toml is invalid. Set up the harness again in ",
    crate::app_name!(),
    " settings."
);
const SET_UP_AGENT: &str = concat!(
    "Set up an AI harness and confirm AI privacy exclusions in ",
    crate::app_name!(),
    " settings."
);
const CONFIRM_DENYLIST: &str = concat!(
    "Confirm AI privacy exclusions in ",
    crate::app_name!(),
    " settings before using AI commands."
);
const SET_UP_HARNESS: &str = concat!("Set up an AI harness in ", crate::app_name!(), " settings.");
const CHOOSE_SUMMARIES: &str = concat!(
    "Choose a summaries folder in ",
    crate::app_name!(),
    " settings."
);
const AI_UNAVAILABLE_HERE: &str = "AI commands are not available on this platform yet.";
const HARNESS_CHANGED: &str = concat!(
    "The AI harness changed on disk; run setup again in ",
    crate::app_name!(),
    " settings."
);

impl AgentView {
    fn parse(table: Option<toml::Value>, record_table: Option<toml::Value>) -> Self {
        let (agent, mut issue) = match table.clone().map(parse_agent_table).transpose() {
            Ok(agent) => (agent.flatten(), None),
            Err(_) => (None, Some(AGENT_ENTRY_INVALID.to_owned())),
        };
        let record = match record_table
            .clone()
            .map(toml::Value::try_into::<HarnessRecord>)
        {
            Some(Ok(record)) => Some(record),
            Some(Err(_)) => {
                issue.get_or_insert_with(|| AGENT_ENTRY_INVALID.to_owned());
                None
            }
            None => None,
        };
        Self {
            table,
            record_table,
            agent,
            record,
            issue,
        }
    }

    fn committed(commit: &HarnessCommit) -> Result<Self> {
        let table = StoredAgent {
            default: Some(commit.agent.harness_id.clone()),
            harnesses: BTreeMap::from([(
                commit.agent.harness_id.clone(),
                StoredHarness {
                    binary: commit.agent.binary.clone(),
                    args: commit.agent.args.clone(),
                    env_allow: commit.agent.env_allow.clone(),
                    timeout_secs: commit.agent.timeout_secs,
                },
            )]),
        };
        let view = Self::parse(
            Some(toml::Value::try_from(table).context("serializing the AI harness entry")?),
            Some(toml::Value::try_from(&commit.record).context("serializing the harness record")?),
        );
        if view.agent.as_ref() != Some(&commit.agent)
            || view.record.as_ref() != Some(&commit.record)
        {
            bail!("the AI harness entry does not round-trip through config.toml");
        }
        Ok(view)
    }
}

/// Host-only facts from harness setup (§7.1). Like the notes folder identity,
/// this never crosses the messaging boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRecord {
    pub harness_id: String,
    pub real_path: String,
    pub size: u64,
    pub mtime_ns: String,
    #[serde(default)]
    pub file_id: Option<String>,
    pub version: String,
    pub help_sha256: String,
    pub confirmed_flags: Vec<String>,
    pub probe_passed_at: String,
}

impl HarnessRecord {
    pub(crate) fn identity(&self) -> BinaryIdentity {
        BinaryIdentity {
            real_path: self.real_path.clone(),
            size: self.size,
            mtime_ns: self.mtime_ns.clone(),
            file_id: self.file_id.clone(),
        }
    }
}

/// What native harness setup commits in one write (§7.3): the harness entry,
/// its checked record, and `agent_denylist_confirmed = true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessCommit {
    pub agent: AgentConfig,
    pub record: HarnessRecord,
}

pub struct AgentReadiness {
    pub harness_id: String,
    pub harness_version: String,
}

struct AgentRefusal {
    state: AgentState,
    code: ErrorCode,
    message: String,
}

impl ConfigStore {
    pub fn default_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("", "", APP_NAME)
            .context("cannot locate this user's application config directory")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        Ok(Self::load_from(Self::default_path()?))
    }

    fn load_from(path: PathBuf) -> Self {
        // The folder may have moved since setup. Keep the config available for
        // repair through update_config, but never use it for vault I/O until a
        // fresh validation succeeds.
        match read_disk_state(&path) {
            Ok(state) => {
                let mut store = Self::for_path(path);
                store.adopt(state);
                store
            }
            // Exiting here would show the extension only a disconnect. Start
            // inert instead; each request refreshes, fails the same way, and
            // answers invalid_config until the file is readable again.
            Err(error) => {
                eprintln!("{NAMESPACE}: warning: configuration is unreadable: {error:#}");
                let mut store = Self::for_path(path);
                store.become_inert();
                store
            }
        }
    }

    /// Forget the last readable config. `hello` and `get_config` report this
    /// state with its issue, so the extension can tell the user to repair the
    /// file; every other request refuses with invalid_config.
    fn become_inert(&mut self) {
        self.config = empty_config();
        self.revision = UNREADABLE_REVISION.into();
        self.issue = Some("The configuration file cannot be read or is not a regular file. Fix or remove it, then reload this page.".into());
        self.root_identity = None;
        self.agent = AgentView::default();
    }

    /// Best-effort startup cleanup of temporaries a killed host left behind,
    /// in the config directory and, once the folder is trusted, the page
    /// notes directory. Failures are logged; they never block serving.
    pub fn sweep_stale_temporaries(&self) {
        if let Some(parent) = self.path.parent()
            && let Err(error) = sweep_config_temporaries(parent)
        {
            eprintln!("{NAMESPACE}: warning: could not sweep config temporaries: {error:#}");
        }
        // A host killed during a harness run leaves its work directory.
        if let Some(parent) = self.path.parent()
            && let Err(error) = harness::sweep_stale_work_dirs(
                &parent.join(harness::WORK_ROOT_NAME),
                STALE_TEMPORARY_AGE,
            )
        {
            eprintln!("{NAMESPACE}: warning: could not sweep harness work directories: {error:#}");
        }
        if !self.configured() {
            return;
        }
        let Some(storage) = self.config.storage.as_ref() else {
            return;
        };
        if let Err(error) = Vault::open_checked(storage, self.root_identity())
            .and_then(|vault| vault.sweep_stale_temporaries())
        {
            eprintln!("{NAMESPACE}: warning: could not sweep page-note temporaries: {error:#}");
        }
    }

    fn for_path(path: PathBuf) -> Self {
        Self {
            path,
            config: empty_config(),
            revision: MISSING_REVISION.into(),
            issue: None,
            root_identity: None,
            agent: AgentView::default(),
        }
    }

    fn adopt(&mut self, state: DiskState) {
        self.config = state.config;
        self.revision = state.revision;
        self.issue = state.issue;
        self.root_identity = state.root_identity;
        self.agent = state.agent;
    }

    pub fn snapshot(&self) -> &ConfigSnapshot {
        &self.config
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// The directory holding config.toml, which also holds host-owned state
    /// such as the harness work directory.
    pub fn dir(&self) -> Result<&Path> {
        self.path.parent().context("config path has no parent")
    }

    pub fn root_identity(&self) -> Option<&str> {
        self.root_identity.as_deref()
    }

    /// Hold this lock from the policy refresh through a privileged write.
    /// ConfigStore::update takes the same lock before replacing config.toml.
    pub(crate) fn lock_current(&self) -> Result<File> {
        let parent = self.path.parent().context("config path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let path = parent.join(".config.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock().context("locking configuration")?;
        Ok(file)
    }

    /// A malformed or newer config is shown to setup, never used for capture.
    pub fn config_issue(&self) -> Option<&str> {
        self.issue.as_deref()
    }

    /// Re-read config.toml. On failure the store becomes inert rather than
    /// keeping a policy the file no longer supports.
    pub fn refresh(&mut self) -> Result<()> {
        let state = match read_disk_state(&self.path) {
            Ok(state) => state,
            Err(error) => {
                self.become_inert();
                return Err(error);
            }
        };
        self.adopt(state);
        Ok(())
    }

    pub fn configured(&self) -> bool {
        self.issue.is_none()
            && self.config.storage.is_some()
            && validate_persisted(&self.config).is_ok()
    }

    /// Whether AI commands may run. Call under `lock_current` right after
    /// `refresh`. The host refuses every agent request until the user has
    /// confirmed the denylist once through native harness setup (§7.3).
    pub fn agent_readiness(&self) -> Result<AgentReadiness, (ErrorCode, String)> {
        self.agent_gate()
            .map_err(|refusal| (refusal.code, refusal.message))
    }

    /// The settings page's view of the same gate.
    pub fn agent_status(&self) -> AgentStatus {
        match self.agent_gate() {
            Ok(ready) => AgentStatus {
                state: AgentState::Ready,
                harness_version: Some(ready.harness_version),
                message: None,
            },
            Err(refusal) => AgentStatus {
                state: refusal.state,
                harness_version: self
                    .agent
                    .record
                    .as_ref()
                    .map(|record| record.version.clone()),
                message: Some(refusal.message),
            },
        }
    }

    fn agent_gate(&self) -> Result<AgentReadiness, AgentRefusal> {
        let refuse = |state, code, message: &str| AgentRefusal {
            state,
            code,
            message: message.to_owned(),
        };
        if self.issue.is_some() {
            return Err(refuse(
                AgentState::NotSetUp,
                ErrorCode::InvalidConfig,
                "configuration is unavailable or needs repair",
            ));
        }
        if !self.config.agent_denylist_confirmed {
            return Err(if self.config.agent.is_some() {
                refuse(
                    AgentState::DenylistUnconfirmed,
                    ErrorCode::NotConfigured,
                    CONFIRM_DENYLIST,
                )
            } else {
                refuse(AgentState::NotSetUp, ErrorCode::NotConfigured, SET_UP_AGENT)
            });
        }
        let Some(agent) = &self.config.agent else {
            return Err(match &self.agent.issue {
                Some(issue) => refuse(AgentState::HarnessProblem, ErrorCode::NotConfigured, issue),
                None => refuse(
                    AgentState::NotSetUp,
                    ErrorCode::NotConfigured,
                    SET_UP_HARNESS,
                ),
            });
        };
        let Some(record) = self
            .agent
            .record
            .as_ref()
            .filter(|record| record.harness_id == agent.harness_id)
        else {
            return Err(refuse(
                AgentState::HarnessProblem,
                ErrorCode::NotConfigured,
                SET_UP_HARNESS,
            ));
        };
        if agent.adapter == HarnessAdapter::Codex {
            // Codex runs only with the extra disables reviewed for the
            // version setup recorded, never another reviewed version's.
            if !harness::template_matches(
                harness::Adapter::Codex,
                &agent.args,
                Some(&record.version),
            ) {
                return Err(refuse(
                    AgentState::HarnessProblem,
                    ErrorCode::NotConfigured,
                    AGENT_ENTRY_INVALID,
                ));
            }
        }
        if self
            .config
            .storage
            .as_ref()
            .and_then(|storage| storage.summaries_dir.as_ref())
            .is_none()
        {
            return Err(refuse(
                AgentState::NotSetUp,
                ErrorCode::NotConfigured,
                CHOOSE_SUMMARIES,
            ));
        }
        if !cfg!(unix) {
            return Err(refuse(
                AgentState::HarnessProblem,
                ErrorCode::NotConfigured,
                AI_UNAVAILABLE_HERE,
            ));
        }
        // Fail closed until step 5.2 adds the automatic re-check (§7.1).
        let unchanged = harness::identity(Path::new(&agent.binary))
            .is_ok_and(|identity| identity == record.identity());
        if !unchanged {
            return Err(refuse(
                AgentState::HarnessProblem,
                ErrorCode::NotConfigured,
                HARNESS_CHANGED,
            ));
        }
        Ok(AgentReadiness {
            harness_id: agent.harness_id.clone(),
            harness_version: record.version.clone(),
        })
    }

    /// Replace config only when the caller's revision still matches the file.
    /// `None` is a conflict; `Some(revision)` is a committed update.
    #[cfg(test)]
    pub(crate) fn update(
        &mut self,
        next: ConfigSnapshot,
        expected_revision: &str,
    ) -> Result<Option<String>> {
        self.update_with_grants(next, expected_revision, None, None)
    }

    #[cfg(test)]
    pub(crate) fn update_with_root_identity(
        &mut self,
        next: ConfigSnapshot,
        expected_revision: &str,
        selected_identity: Option<&str>,
    ) -> Result<Option<String>> {
        self.update_with_grants(next, expected_revision, selected_identity, None)
    }

    /// Native dispatch calls this only after `ConsentAuthority` has checked
    /// every grant. The agent checks below repeat that decision against the
    /// file itself, under the lock, as defense in depth.
    pub(crate) fn update_with_grants(
        &mut self,
        next: ConfigSnapshot,
        expected_revision: &str,
        selected_identity: Option<&str>,
        harness_commit: Option<HarnessCommit>,
    ) -> Result<Option<String>> {
        let json = serde_json::to_vec(&next).context("serializing config JSON")?;
        if json.len() > MAX_CONFIG_BYTES {
            bail!("configuration exceeds the 64 KiB size limit");
        }
        validate(&next)?;
        if let Some(commit) = &harness_commit
            && (next.agent.as_ref() != Some(&commit.agent)
                || !next.agent_denylist_confirmed
                || commit.record.harness_id != commit.agent.harness_id)
        {
            bail!("harness setup does not match this configuration");
        }
        let parent = self.path.parent().context("config path has no parent")?;
        let _lock_file = self.lock_current()?;
        let current = read_disk_state(&self.path)?;
        if expected_revision != current.revision {
            self.adopt(current);
            return Ok(None);
        }

        let agent = match &harness_commit {
            Some(commit) => {
                if harness::identity(Path::new(&commit.agent.binary))? != commit.record.identity() {
                    bail!("{HARNESS_CHANGED}");
                }
                AgentView::committed(commit)?
            }
            None => {
                // Only native harness setup turns the confirmation on or sets
                // a harness. Removing the harness keeps the confirmation.
                let confirmed = current.issue.is_none() && current.config.agent_denylist_confirmed;
                if next.agent_denylist_confirmed != confirmed {
                    bail!("agent_denylist_confirmed can only be set by native harness setup");
                }
                match (&next.agent, &current.config.agent) {
                    (None, Some(_)) => AgentView::default(),
                    // Keep an entry this host cannot parse for the user to fix.
                    (None, None) => current.agent.clone(),
                    (Some(next_agent), Some(old)) if next_agent == old => current.agent.clone(),
                    _ => bail!("the AI harness can only be set by native harness setup"),
                }
            }
        };

        let next_identity = match next.storage.as_ref() {
            None => None,
            Some(storage) => {
                let unchanged = current
                    .config
                    .storage
                    .as_ref()
                    .is_some_and(|old| old.root == storage.root);
                let expected = selected_identity
                    .or_else(|| {
                        unchanged
                            .then_some(current.root_identity.as_deref())
                            .flatten()
                    })
                    .context("a fresh native folder selection is required")?;
                let actual = selected_root_identity(Path::new(&storage.root))?;
                if actual != expected {
                    bail!("selected notes folder changed; choose it again");
                }
                Some(actual)
            }
        };
        let serialized = toml::to_string_pretty(&StoredConfig::from_snapshot(
            &next,
            next_identity.clone(),
            &agent,
        ))
        .context("serializing config TOML")?;
        if serialized.len() > MAX_CONFIG_BYTES {
            bail!("configuration exceeds the 64 KiB size limit");
        }

        // Repair must preserve the exact unreadable file. A no-clobber,
        // durable backup is created before the replacement can be published.
        if current.needs_backup {
            backup_invalid(parent, &self.path, &current.revision)?;
            // A local editor might ignore our lock. Do not replace bytes that
            // changed while the backup was being made.
            let after_backup = read_disk_state(&self.path)?;
            if after_backup.revision != current.revision {
                self.adopt(after_backup);
                return Ok(None);
            }
        }

        let temporary = parent.join(format!(".config-{}.tmp", Uuid::new_v4()));
        let (mut file, _cleanup) = create_temporary(&temporary)?;
        let write_result = (|| -> Result<()> {
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        // Close the handle before the cleanup guard runs on error. Windows may
        // refuse to remove a still-open temporary file.
        drop(file);
        write_result?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        let revision = revision_for(serialized.as_bytes());
        self.config = next;
        self.revision = revision.clone();
        self.issue = None;
        self.root_identity = next_identity;
        self.agent = agent;
        if let Err(error) = sync_parent(parent) {
            eprintln!(
                "{NAMESPACE}: warning: config was saved but directory sync failed: {error:#}"
            );
        }
        Ok(Some(revision))
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        Self::for_path(path)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    #[serde(default)]
    storage: Option<StoredStorage>,
    #[serde(default)]
    root_picker_confirmed: bool,
    #[serde(default)]
    root_identity: Option<String>,
    #[serde(default)]
    capture_enabled: bool,
    #[serde(default)]
    sites: Vec<SiteConfig>,
    #[serde(default = "default_strip_params")]
    strip_params: Vec<String>,
    #[serde(default = "default_near_repeat_secs")]
    near_repeat_secs: u32,
    // A missing key (any config written before M2) counts as unconfirmed.
    #[serde(default)]
    agent_denylist: Vec<String>,
    #[serde(default)]
    agent_denylist_confirmed: bool,
    #[serde(default)]
    log_incognito: bool,
    #[serde(default)]
    agent: Option<toml::Value>,
    #[serde(default)]
    harness_record: Option<toml::Value>,
}

/// The on-disk storage table. Unlike the wire type, `summaries_dir` may be
/// omitted, so every M1 config.toml still loads.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredStorage {
    root: String,
    profile: String,
    log_dir: String,
    pages_dir: String,
    later_dir: String,
    #[serde(default)]
    summaries_dir: Option<String>,
}

impl From<StorageConfig> for StoredStorage {
    fn from(value: StorageConfig) -> Self {
        Self {
            root: value.root,
            profile: value.profile,
            log_dir: value.log_dir,
            pages_dir: value.pages_dir,
            later_dir: value.later_dir,
            summaries_dir: value.summaries_dir,
        }
    }
}

impl From<StoredStorage> for StorageConfig {
    fn from(value: StoredStorage) -> Self {
        Self {
            root: value.root,
            profile: value.profile,
            log_dir: value.log_dir,
            pages_dir: value.pages_dir,
            later_dir: value.later_dir,
            summaries_dir: value.summaries_dir,
        }
    }
}

/// `[agent]` as §7.1 shows it: the chosen harness and its entry.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAgent {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    harnesses: BTreeMap<String, StoredHarness>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredHarness {
    binary: String,
    args: Vec<String>,
    // A missing allowlist means an empty environment (§7.1).
    #[serde(default)]
    env_allow: Vec<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u32,
}

const fn default_timeout_secs() -> u32 {
    DEFAULT_TIMEOUT_SECS
}

fn parse_agent_table(value: toml::Value) -> Result<Option<AgentConfig>> {
    let stored: StoredAgent = value.try_into().context("parsing [agent]")?;
    let mut harnesses = stored.harnesses;
    let Some(harness_id) = stored.default else {
        return Ok(None);
    };
    if harnesses.len() > 1 {
        bail!("only one AI harness can be configured");
    }
    let adapter = adapter_for(&harness_id).context("unknown AI harness")?;
    let harness = harnesses
        .remove(&harness_id)
        .context("the default AI harness has no entry")?;
    let agent = AgentConfig {
        harness_id,
        adapter,
        binary: harness.binary,
        args: harness.args,
        env_allow: harness.env_allow,
        timeout_secs: harness.timeout_secs,
    };
    validate_agent(&agent)?;
    Ok(Some(agent))
}

impl StoredConfig {
    fn from_snapshot(
        value: &ConfigSnapshot,
        root_identity: Option<String>,
        agent: &AgentView,
    ) -> Self {
        Self {
            storage: value.storage.clone().map(StoredStorage::from),
            // Only native dispatch calls ConfigStore::update, after verifying
            // the picker grant for a new root. This marks the root's M1 origin.
            root_picker_confirmed: value.storage.is_some(),
            root_identity,
            capture_enabled: value.capture_enabled,
            sites: value.sites.clone(),
            strip_params: value.strip_params.clone(),
            near_repeat_secs: value.near_repeat_secs,
            agent_denylist: value.agent_denylist.clone(),
            agent_denylist_confirmed: value.agent_denylist_confirmed,
            log_incognito: value.log_incognito,
            // The harness entry and record come from the host's own view,
            // never from the extension's snapshot.
            agent: agent.table.clone(),
            harness_record: agent.record_table.clone(),
        }
    }
}

impl From<StoredConfig> for ConfigSnapshot {
    fn from(value: StoredConfig) -> Self {
        Self {
            storage: value.storage.map(StorageConfig::from),
            capture_enabled: value.capture_enabled,
            sites: value.sites,
            strip_params: value.strip_params,
            near_repeat_secs: value.near_repeat_secs,
            agent_denylist: value.agent_denylist,
            agent_denylist_confirmed: value.agent_denylist_confirmed,
            log_incognito: value.log_incognito,
            agent: None,
        }
    }
}

fn read_disk_state(path: &Path) -> Result<DiskState> {
    // Check before opening: opening a FIFO for reading would block while the
    // config lock is held.
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            bail!("{} is not a regular file", path.display())
        }
        _ => {}
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DiskState {
                config: empty_config(),
                revision: MISSING_REVISION.into(),
                issue: None,
                needs_backup: false,
                root_identity: None,
                agent: AgentView::default(),
            });
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("reading {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mut file = file.take(MAX_HASHED_CONFIG_BYTES);
    let mut contents = Vec::new();
    let mut hasher = RevisionHasher::new();
    let mut bytes_read = 0u64;
    let mut chunk = [0u8; 8192];
    loop {
        let count = file
            .read(&mut chunk)
            .with_context(|| format!("reading {}", path.display()))?;
        if count == 0 {
            break;
        }
        bytes_read += count as u64;
        hasher.update(&chunk[..count]);
        let keep = (MAX_CONFIG_BYTES + 1)
            .saturating_sub(contents.len())
            .min(count);
        contents.extend_from_slice(&chunk[..keep]);
    }
    let revision = hasher.finish(metadata.len());
    if bytes_read > MAX_CONFIG_BYTES as u64 {
        return Ok(DiskState {
            config: empty_config(),
            revision,
            issue: Some("Configuration exceeds 64 KiB. Choose a notes folder and save a replacement; the original will be backed up.".into()),
            needs_backup: true,
            root_identity: None,
            agent: AgentView::default(),
        });
    }
    let parsed = std::str::from_utf8(&contents)
        .map_err(anyhow::Error::from)
        .and_then(|text| toml::from_str::<StoredConfig>(text).map_err(anyhow::Error::from));
    match parsed {
        Ok(mut stored) => {
            let root_identity = stored.root_identity.clone();
            let root_unconfirmed = stored.storage.is_some()
                && (!stored.root_picker_confirmed || root_identity.is_none());
            let agent = AgentView::parse(stored.agent.take(), stored.harness_record.take());
            let mut config: ConfigSnapshot = stored.into();
            config.agent = agent.agent.clone();
            if root_unconfirmed || validate(&config).is_err() {
                let issue = if root_unconfirmed {
                    "This notes folder predates native picker confirmation. Choose it again to activate capture; the original config will be backed up."
                } else {
                    "Configuration settings are invalid. Choose a notes folder and save a repair; the original config will be backed up."
                };
                return Ok(DiskState {
                    config: empty_config(),
                    revision,
                    issue: Some(issue.to_owned()),
                    needs_backup: true,
                    root_identity: None,
                    agent,
                });
            }
            // The settings themselves are valid. A folder that is missing or
            // no longer matches its stored identity (an unmounted drive, or a
            // remount with a new device number) blocks vault I/O and needs
            // reselection, but it must not discard the allowlist: keep every
            // setting for the repair, which rewrites the file with a fresh
            // identity, so there is nothing to back up.
            let folder_changed = config.storage.as_ref().is_some_and(|storage| {
                match selected_root_identity(Path::new(&storage.root)) {
                    Ok(actual) => Some(actual.as_str()) != root_identity.as_deref(),
                    Err(_) => true,
                }
            });
            Ok(DiskState {
                config,
                revision,
                issue: folder_changed.then(|| "The selected notes folder is unavailable or changed. Reconnect it, or choose it again to repair this configuration; your other settings are kept.".to_owned()),
                needs_backup: false,
                root_identity,
                agent,
            })
        }
        Err(_) => Ok(DiskState {
            config: empty_config(),
            revision,
            issue: Some("Configuration is invalid or from a newer version. Choose a notes folder and save a replacement; the original will be backed up.".into()),
            needs_backup: true,
            root_identity: None,
            agent: AgentView::default(),
        }),
    }
}

fn backup_invalid(parent: &Path, source_path: &Path, expected_revision: &str) -> Result<()> {
    let path = parent.join(format!("config-invalid-{}.toml", Uuid::new_v4()));
    let (mut file, mut cleanup) = create_temporary(&path)?;
    let result = (|| -> Result<()> {
        let mut source = File::open(source_path)
            .with_context(|| format!("opening invalid config at {}", source_path.display()))?;
        // Copy every byte, but compute the revision exactly as
        // read_disk_state does, or a file past the hash cap never matches.
        let mut hasher = RevisionHasher::new();
        let mut copied = 0u64;
        let mut chunk = [0u8; 8192];
        loop {
            let count = source.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            hasher.update(&chunk[..count]);
            copied += count as u64;
            file.write_all(&chunk[..count])?;
        }
        if hasher.finish(copied) != expected_revision {
            bail!("configuration changed while it was being backed up");
        }
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    result.with_context(|| format!("backing up invalid config to {}", path.display()))?;
    sync_parent(parent).context("syncing config backup directory")?;
    cleanup.disarm();
    Ok(())
}

/// Remove `.config-<uuid>.tmp` files old enough that no live update can still
/// own them. Symlinks and anything not exactly that name are left alone.
fn sweep_config_temporaries(parent: &Path) -> Result<usize> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("listing {}", parent.display()));
        }
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", parent.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_generated_temporary(name, ".config-") {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let stale = metadata.is_file()
            && metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|elapsed| elapsed >= STALE_TEMPORARY_AGE);
        if stale && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn revision_for(contents: &[u8]) -> String {
    let mut hasher = RevisionHasher::new();
    hasher.update(contents);
    hasher.finish(contents.len() as u64)
}

/// The one definition of a config revision: the SHA-256 of at most
/// `MAX_HASHED_CONFIG_BYTES`, plus the file length when that cap is reached.
struct RevisionHasher {
    hasher: Sha256,
    hashed: u64,
}

impl RevisionHasher {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            hashed: 0,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        let room = MAX_HASHED_CONFIG_BYTES - self.hashed;
        let take = (bytes.len() as u64).min(room) as usize;
        self.hasher.update(&bytes[..take]);
        self.hashed += take as u64;
    }

    fn finish(mut self, total_len: u64) -> String {
        if self.hashed == MAX_HASHED_CONFIG_BYTES {
            self.hasher.update(total_len.to_le_bytes());
        }
        format!("sha256:{}", hex::encode(self.hasher.finalize()))
    }
}

fn create_temporary(path: &Path) -> Result<(File, TempFileCleanup<'_>)> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    // The guard is installed only after create_new succeeds. An existing file
    // with this name must never be removed on a collision or open failure.
    Ok((file, TempFileCleanup { path, armed: true }))
}

pub(crate) fn empty_config() -> ConfigSnapshot {
    ConfigSnapshot {
        storage: None,
        capture_enabled: false,
        sites: Vec::new(),
        strip_params: default_strip_params(),
        near_repeat_secs: default_near_repeat_secs(),
        agent_denylist: Vec::new(),
        agent_denylist_confirmed: false,
        log_incognito: false,
        agent: None,
    }
}

/// Check a proposed config's shape and limits. This never touches the
/// filesystem: an extension-supplied root is untrusted until a picker grant or
/// the persisted identity authorizes it, so probing it here would answer
/// whether arbitrary paths exist (and, on Windows, reach out to UNC shares).
/// Callers open the root only after authorization; see `validate_persisted`.
pub fn validate(config: &ConfigSnapshot) -> Result<()> {
    if config.sites.len() > MAX_SITES {
        bail!("site allowlist exceeds the 128-entry limit");
    }
    for site in &config.sites {
        validate_site(site)?;
    }
    if config.strip_params.len() > MAX_STRIP_PARAMS {
        bail!("tracking-parameter list exceeds the 64-entry limit");
    }
    for value in &config.strip_params {
        if value.is_empty()
            || value == "*"
            || value.len() > MAX_RULE_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'*'))
            || (value.contains('*') && !value.ends_with('*'))
            || value.matches('*').count() > 1
        {
            bail!("tracking parameters must be simple names or prefix patterns ending in *");
        }
    }
    if config.near_repeat_secs > 86_400 {
        bail!("near-repeat window exceeds 24 hours");
    }
    if config.capture_enabled && (config.storage.is_none() || config.sites.is_empty()) {
        bail!("capture requires a selected notes folder and at least one site");
    }
    if config.log_incognito {
        bail!("log_incognito cannot be turned on");
    }
    privacy::validate_denylist(&config.agent_denylist)?;
    if let Some(agent) = &config.agent {
        validate_agent(agent)?;
    }
    if let Some(storage) = &config.storage {
        validate_storage(storage)?;
    }
    Ok(())
}

/// Validate a config the host itself persisted, including that its notes
/// folder can be opened. Only call this for a root that is already trusted.
fn validate_persisted(config: &ConfigSnapshot) -> Result<()> {
    validate(config)?;
    if let Some(storage) = &config.storage {
        Vault::open(storage).context("notes folder is unavailable")?;
    }
    Ok(())
}

fn validate_site(site: &SiteConfig) -> Result<()> {
    if site.origin.len() > 2048 || site.path_prefix.len() > 2048 {
        bail!("site allowlist entry is too long");
    }
    let parsed = Url::parse(&site.origin).context("site origin is invalid")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.origin().ascii_serialization() != site.origin
    {
        bail!("site origin must be an exact HTTP(S) scheme, host, and optional port");
    }
    if !site.path_prefix.starts_with('/')
        || !site.path_prefix.bytes().all(|byte| byte.is_ascii_graphic())
        || !valid_percent_encoding(&site.path_prefix)
        || site.path_prefix.contains("//")
        || (site.path_prefix != "/" && site.path_prefix.ends_with('/'))
        || site.path_prefix.contains('?')
        || site.path_prefix.contains('#')
        || site.path_prefix.contains('\\')
        || site
            .path_prefix
            .split('/')
            .any(|part| part == "." || part == "..")
    {
        bail!("site path prefix must be an absolute URL path without query or traversal");
    }
    let full = Url::parse(&format!("{}{}", site.origin, site.path_prefix))
        .context("site path prefix is invalid")?;
    if full.path() != site.path_prefix {
        bail!("site path prefix must use canonical URL encoding");
    }
    Ok(())
}

fn valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'%' {
            if bytes
                .get(offset + 1)
                .is_none_or(|value| !value.is_ascii_hexdigit())
                || bytes
                    .get(offset + 2)
                    .is_none_or(|value| !value.is_ascii_hexdigit())
            {
                return false;
            }
            offset += 3;
        } else {
            offset += 1;
        }
    }
    true
}

fn validate_storage(storage: &StorageConfig) -> Result<()> {
    if storage.root.len() > MAX_ROOT_PATH_BYTES {
        bail!("storage.root exceeds the 4096-byte path limit");
    }
    if has_unsafe_display_chars(&storage.root) {
        bail!("notes folder path contains control or bidirectional formatting characters");
    }
    let root = Path::new(&storage.root);
    if !root.is_absolute() {
        bail!("storage.root must be an existing absolute folder selected by the user");
    }
    if storage.profile != "neutral" {
        bail!("storage.profile must be neutral until the Obsidian preset is implemented");
    }

    let mut paths = vec![
        Path::new(&storage.log_dir),
        Path::new(&storage.pages_dir),
        Path::new(&storage.later_dir),
    ];
    if let Some(summaries_dir) = &storage.summaries_dir {
        paths.push(Path::new(summaries_dir));
    }
    for path in &paths {
        let value = path.as_os_str().to_string_lossy();
        if value.len() > MAX_LOCATION_PATH_BYTES {
            bail!("content location exceeds the 512-byte path limit");
        }
        if has_unsafe_display_chars(&value) {
            bail!("content location contains control or bidirectional formatting characters");
        }
        if path.as_os_str().is_empty() || !path.components().all(|part| {
            matches!(part, Component::Normal(component) if component.len() <= MAX_PATH_COMPONENT_BYTES)
        }) {
            bail!("content locations must be nonempty relative paths without . or .. components");
        }
    }
    for left in 0..paths.len() {
        for right in (left + 1)..paths.len() {
            let l = lower_components(paths[left]);
            let r = lower_components(paths[right]);
            if l.starts_with(&r) || r.starts_with(&l) {
                bail!("log, pages, later, and summaries locations must not overlap");
            }
        }
    }
    Ok(())
}

pub(crate) fn has_unsafe_display_chars(value: &str) -> bool {
    value.chars().any(|ch| {
        ch.is_control()
            || matches!(
                ch,
                '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
    })
}

pub(crate) fn adapter_for(harness_id: &str) -> Option<HarnessAdapter> {
    match harness_id {
        "claude-code" => Some(HarnessAdapter::ClaudeCode),
        "codex" => Some(HarnessAdapter::Codex),
        _ => None,
    }
}

/// The shape every harness entry needs, including the adapter's exact
/// argument template. Harness setup also checks the binary's `--help`.
fn validate_agent(agent: &AgentConfig) -> Result<()> {
    if adapter_for(&agent.harness_id) != Some(agent.adapter) {
        bail!("the AI harness must be Claude Code or Codex");
    }
    if agent.binary.len() > MAX_ROOT_PATH_BYTES
        || has_unsafe_display_chars(&agent.binary)
        || !Path::new(&agent.binary).is_absolute()
    {
        bail!("the AI harness binary must be an absolute path");
    }
    if agent.args.len() > MAX_AGENT_ARGS
        || agent.args.iter().any(|arg| {
            arg.len() > MAX_AGENT_ARG_BYTES
                || has_unsafe_display_chars(arg)
                || (arg != PROMPT_PLACEHOLDER && arg.contains(PROMPT_PLACEHOLDER))
        })
        || agent
            .args
            .iter()
            .filter(|arg| *arg == PROMPT_PLACEHOLDER)
            .count()
            != 1
    {
        bail!(
            "AI harness arguments must contain {PROMPT_PLACEHOLDER} exactly once, as its own argument"
        );
    }
    // Codex's recorded version is checked at the agent gate.
    if !harness::template_matches(agent.adapter.into(), &agent.args, None) {
        bail!("AI harness arguments must be exactly the adapter's template");
    }
    if agent.env_allow.len() > MAX_ENV_ALLOW {
        bail!("the AI harness environment allowlist exceeds the 32-name limit");
    }
    let mut seen = HashSet::new();
    for name in &agent.env_allow {
        if !valid_env_name(name) || !seen.insert(name.as_str()) {
            bail!(
                "AI harness environment names must be distinct uppercase names, not DYLD_ or LD_ variables"
            );
        }
    }
    if agent.timeout_secs == 0 || agent.timeout_secs > MAX_TIMEOUT_SECS {
        bail!("the AI harness timeout must be 1 to 600 seconds");
    }
    Ok(())
}

fn valid_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_ENV_NAME_BYTES
        && (bytes[0].is_ascii_uppercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
        && !name.starts_with("DYLD_")
        && !name.starts_with("LD_")
}

/// Path components in canonical caseless form, NFD(fold(NFD(name))), so two
/// names a case-insensitive APFS volume treats as one folder compare equal.
/// `to_lowercase` is not enough: it keeps final sigma distinct from sigma.
fn lower_components(path: &Path) -> Vec<String> {
    let fold = icu_casemap::CaseMapper::new();
    let nfd = icu_normalizer::DecomposingNormalizer::new_nfd();
    path.components()
        .map(|part| {
            let name = part.as_os_str().to_string_lossy();
            nfd.normalize(&fold.fold_string(&nfd.normalize(&name)))
                .into_owned()
        })
        .collect()
}

struct TempFileCleanup<'a> {
    path: &'a Path,
    armed: bool,
}

impl TempFileCleanup<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileCleanup<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(self.path);
        }
    }
}

#[cfg(not(windows))]
fn sync_parent(parent: &Path) -> Result<()> {
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_parent(_parent: &Path) -> Result<()> {
    // Windows directory handles are not opened for fsync by std::fs::File.
    // The temporary file itself was synced before the atomic rename.
    Ok(())
}

/// config.toml bodies as a user or an older build would have written them.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::harness;
    use crate::vault::selected_root_identity;

    fn quoted(value: &str) -> String {
        toml::Value::String(value.to_owned()).to_string()
    }

    /// A config with storage under `root`. `keys` go at the top level and
    /// `tables` after `[storage]`.
    pub(crate) fn write(path: &Path, root: &Path, storage_extra: &str, keys: &str, tables: &str) {
        let identity = selected_root_identity(root).unwrap();
        let body = format!(
            "root_picker_confirmed = true\nroot_identity = {}\n{keys}\n[storage]\nroot = {}\nprofile = \"neutral\"\nlog_dir = \"log\"\npages_dir = \"pages\"\nlater_dir = \"later\"\n{storage_extra}\n{tables}",
            quoted(&identity),
            quoted(&root.to_string_lossy()),
        );
        fs::write(path, body).unwrap();
    }

    /// A notes folder, fake harness, and config.toml that pass the agent gate.
    pub(crate) fn ready(folder: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let root = folder.join("notes");
        fs::create_dir(&root).unwrap();
        let binary = fake_harness(folder);
        let path = folder.join("config.toml");
        write(
            &path,
            &root,
            "summaries_dir = \"summaries\"",
            "agent_denylist = [\"bank.example\"]\nagent_denylist_confirmed = true",
            &format!("{}{}", agent_table(&binary), record_table(&binary)),
        );
        (path, root, binary)
    }

    /// An absolute harness path on this platform for tests that never read
    /// or run it. Unix keeps the path a real install would have.
    pub(crate) fn absent_binary(name: &str) -> String {
        if cfg!(windows) {
            format!("C:\\Program Files\\{name}\\{name}.exe")
        } else {
            format!("/usr/local/bin/{name}")
        }
    }

    /// A fake harness file. It is never executed; only its identity matters.
    pub(crate) fn fake_harness(folder: &Path) -> std::path::PathBuf {
        let path = folder.join("claude");
        fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        path
    }

    pub(crate) fn agent_table(binary: &Path) -> String {
        harness_table(
            "claude-code",
            binary,
            &harness::template_args(harness::Adapter::ClaudeCode),
        )
    }

    pub(crate) fn harness_table(harness_id: &str, binary: &Path, args: &[String]) -> String {
        format!(
            "[agent]\ndefault = \"{harness_id}\"\n[agent.harnesses.{harness_id}]\nbinary = {}\nargs = {}\nenv_allow = [\"HOME\"]\n",
            quoted(&binary.to_string_lossy()),
            toml::Value::try_from(args).unwrap(),
        )
    }

    pub(crate) fn record_table(binary: &Path) -> String {
        record_table_for("claude-code", "2.1.284", binary)
    }

    pub(crate) fn record_table_for(harness_id: &str, version: &str, binary: &Path) -> String {
        let identity = harness::identity(binary).unwrap();
        format!(
            "[harness_record]\nharness_id = \"{harness_id}\"\nreal_path = {}\nsize = {}\nmtime_ns = {}\n{}version = \"{version}\"\nhelp_sha256 = \"00\"\nconfirmed_flags = [\"-p\"]\nprobe_passed_at = \"2026-09-28T00:00:00Z\"\n",
            quoted(&identity.real_path),
            identity.size,
            quoted(&identity.mtime_ns),
            // Only unix identities carry a file id; an empty string would
            // not match the `None` a Windows identity has.
            identity
                .file_id
                .as_deref()
                .map(|file_id| format!("file_id = {}\n", quoted(file_id)))
                .unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(path: &Path) -> ConfigStore {
        let mut store = ConfigStore::for_test(path.to_path_buf());
        store.refresh().unwrap();
        store
    }

    fn refusal(store: &ConfigStore) -> (ErrorCode, String) {
        store
            .agent_readiness()
            .err()
            .expect("agent should be refused")
    }

    /// The gate passes a config setup would write. Other platforms refuse
    /// AI commands for now (DESIGN.md), before any identity check, so the
    /// same config gets that refusal there.
    fn assert_ready_here(store: &ConfigStore, harness_id: &str, version: &str) {
        if cfg!(unix) {
            let ready = store.agent_readiness().expect("agent should be ready");
            assert_eq!(ready.harness_id, harness_id);
            assert_eq!(ready.harness_version, version);
            assert_eq!(store.agent_status().state, AgentState::Ready);
        } else {
            assert_eq!(
                refusal(store),
                (ErrorCode::NotConfigured, AI_UNAVAILABLE_HERE.into())
            );
            assert_eq!(store.agent_status().state, AgentState::HarnessProblem);
        }
    }

    #[test]
    fn m1_config_without_new_keys_loads_unconfirmed() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        // An M1 file: storage without summaries_dir and no privacy keys.
        fixtures::write(&path, folder.path(), "", "capture_enabled = false", "");
        let store = loaded(&path);
        assert_eq!(store.config_issue(), None);
        assert!(store.configured());
        let snapshot = store.snapshot();
        assert_eq!(snapshot.storage.as_ref().unwrap().summaries_dir, None);
        assert!(snapshot.agent_denylist.is_empty());
        assert!(!snapshot.agent_denylist_confirmed);
        assert!(!snapshot.log_incognito);
        assert_eq!(snapshot.agent, None);
        assert_eq!(
            refusal(&store),
            (ErrorCode::NotConfigured, SET_UP_AGENT.into())
        );
        assert_eq!(store.agent_status().state, AgentState::NotSetUp);
    }

    #[test]
    fn log_incognito_true_is_rejected_and_on_disk_true_is_recoverable() {
        let config = ConfigSnapshot {
            log_incognito: true,
            ..empty_config()
        };
        assert!(validate(&config).is_err());

        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        fixtures::write(&path, folder.path(), "", "log_incognito = true", "");
        let mut store = loaded(&path);
        assert!(store.config_issue().is_some());
        assert!(!store.configured());
        let revision = store.revision().to_owned();
        let repaired = ConfigSnapshot {
            storage: Some(StorageConfig {
                root: folder.path().to_string_lossy().into_owned(),
                profile: "neutral".into(),
                log_dir: "log".into(),
                pages_dir: "pages".into(),
                later_dir: "later".into(),
                summaries_dir: None,
            }),
            ..empty_config()
        };
        let identity = selected_root_identity(folder.path()).unwrap();
        store
            .update_with_root_identity(repaired, &revision, Some(&identity))
            .unwrap()
            .unwrap();
        let store = loaded(&path);
        assert_eq!(store.config_issue(), None);
        assert!(!store.snapshot().log_incognito);
    }

    #[test]
    fn invalid_agent_table_disables_ai_but_keeps_capture() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        fixtures::write(
            &path,
            folder.path(),
            "summaries_dir = \"summaries\"",
            "agent_denylist_confirmed = true",
            "[agent]\ndefault = \"claude-code\"\n[agent.harnesses.claude-code]\nbinary = \"claude\"\nargs = [\"{prompt}\"]\n",
        );
        let mut store = loaded(&path);
        assert_eq!(store.config_issue(), None);
        assert!(store.configured());
        assert_eq!(store.snapshot().agent, None);
        let status = store.agent_status();
        assert_eq!(status.state, AgentState::HarnessProblem);
        assert_eq!(status.message.as_deref(), Some(AGENT_ENTRY_INVALID));

        // Saving other settings keeps the bad entry on disk for the user.
        let next = ConfigSnapshot {
            near_repeat_secs: 60,
            ..store.snapshot().clone()
        };
        let revision = store.revision().to_owned();
        store.update(next, &revision).unwrap().unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("binary = \"claude\""));
        assert_eq!(
            loaded(&path).agent_status().message.as_deref(),
            Some(AGENT_ENTRY_INVALID)
        );
    }

    #[test]
    fn rejects_summaries_dir_overlapping_pages() {
        let folder = tempfile::tempdir().unwrap();
        let mut storage = StorageConfig {
            root: folder.path().to_string_lossy().into_owned(),
            profile: "neutral".into(),
            log_dir: "log".into(),
            pages_dir: "pages".into(),
            later_dir: "later".into(),
            summaries_dir: Some("pages/summaries".into()),
        };
        assert!(validate_storage(&storage).is_err());
        storage.summaries_dir = Some("log".into());
        assert!(validate_storage(&storage).is_err());
        storage.summaries_dir = Some("../summaries".into());
        assert!(validate_storage(&storage).is_err());
        storage.summaries_dir = Some("summaries".into());
        validate_storage(&storage).unwrap();
    }

    #[test]
    fn summaries_overlap_is_case_insensitive() {
        let folder = tempfile::tempdir().unwrap();
        let storage = StorageConfig {
            root: folder.path().to_string_lossy().into_owned(),
            profile: "neutral".into(),
            log_dir: "log".into(),
            pages_dir: "pages".into(),
            later_dir: "later".into(),
            summaries_dir: Some("Pages".into()),
        };
        assert!(validate_storage(&storage).is_err());
    }

    #[test]
    fn overlap_uses_unicode_case_folding_and_normalization() {
        // A case-insensitive APFS volume treats final and medial sigma as one
        // letter, and composed and decomposed accents as one name.
        let folder = tempfile::tempdir().unwrap();
        let mut storage = StorageConfig {
            root: folder.path().to_string_lossy().into_owned(),
            profile: "neutral".into(),
            log_dir: "log".into(),
            pages_dir: "ας".into(),
            later_dir: "later".into(),
            summaries_dir: Some("ασ".into()),
        };
        assert!(validate_storage(&storage).is_err());
        storage.pages_dir = "pages".into();
        storage.log_dir = "Stra\u{df}e".into();
        storage.summaries_dir = Some("STRASSE/summaries".into());
        assert!(validate_storage(&storage).is_err());
        storage.log_dir = "caf\u{e9}".into();
        storage.summaries_dir = Some("CAFE\u{301}".into());
        assert!(validate_storage(&storage).is_err());
        storage.summaries_dir = Some("cafe".into());
        validate_storage(&storage).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn agent_readiness_is_ready_with_confirmed_denylist_and_unchanged_harness() {
        let folder = tempfile::tempdir().unwrap();
        let (path, _, binary) = fixtures::ready(folder.path());
        let store = loaded(&path);
        let ready = store.agent_readiness().expect("agent should be ready");
        assert_eq!(ready.harness_id, "claude-code");
        assert_eq!(ready.harness_version, "2.1.284");
        assert_eq!(store.agent_status().state, AgentState::Ready);

        // An in-place update changes the identity and fails closed.
        fs::write(&binary, "#!/bin/sh\nexit 2 # updated\n").unwrap();
        assert_eq!(
            refusal(&loaded(&path)),
            (ErrorCode::NotConfigured, HARNESS_CHANGED.into())
        );
    }

    #[test]
    fn update_without_commit_carries_agent_state_forward_from_disk() {
        let folder = tempfile::tempdir().unwrap();
        let (path, _, _) = fixtures::ready(folder.path());
        let mut store = loaded(&path);
        let next = ConfigSnapshot {
            near_repeat_secs: 60,
            ..store.snapshot().clone()
        };
        let revision = store.revision().to_owned();
        store.update(next, &revision).unwrap().unwrap();
        assert_ready_here(&store, "claude-code", "2.1.284");
        let reloaded = loaded(&path);
        assert_eq!(reloaded.snapshot().near_repeat_secs, 60);
        assert!(reloaded.snapshot().agent.is_some());
        assert_ready_here(&reloaded, "claude-code", "2.1.284");
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("[harness_record]")
        );
    }

    #[test]
    fn update_without_commit_cannot_confirm_or_set_a_harness() {
        let folder = tempfile::tempdir().unwrap();
        let (path, root, _) = fixtures::ready(folder.path());
        let agent = loaded(&path).snapshot().agent.clone().unwrap();
        fixtures::write(&path, &root, "summaries_dir = \"summaries\"", "", "");
        let mut store = loaded(&path);
        let revision = store.revision().to_owned();
        let before = fs::read(&path).unwrap();
        for next in [
            ConfigSnapshot {
                agent_denylist_confirmed: true,
                ..store.snapshot().clone()
            },
            ConfigSnapshot {
                agent: Some(agent.clone()),
                ..store.snapshot().clone()
            },
        ] {
            assert!(store.update(next, &revision).is_err());
        }
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn update_without_commit_cannot_unconfirm_the_denylist() {
        let folder = tempfile::tempdir().unwrap();
        let (path, _, _) = fixtures::ready(folder.path());
        let mut store = loaded(&path);
        let revision = store.revision().to_owned();
        let next = ConfigSnapshot {
            agent_denylist_confirmed: false,
            ..store.snapshot().clone()
        };
        assert!(store.update(next, &revision).is_err());
    }

    #[test]
    fn removing_harness_keeps_denylist_confirmed() {
        let folder = tempfile::tempdir().unwrap();
        let (path, _, _) = fixtures::ready(folder.path());
        let mut store = loaded(&path);
        let next = ConfigSnapshot {
            agent: None,
            ..store.snapshot().clone()
        };
        let revision = store.revision().to_owned();
        store.update(next, &revision).unwrap().unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("[agent"));
        assert!(!contents.contains("[harness_record]"));
        let store = loaded(&path);
        assert!(store.snapshot().agent_denylist_confirmed);
        assert_eq!(
            store.snapshot().agent_denylist,
            vec!["bank.example".to_owned()]
        );
        assert_eq!(
            refusal(&store),
            (ErrorCode::NotConfigured, SET_UP_HARNESS.into())
        );
        assert_eq!(store.agent_status().state, AgentState::NotSetUp);
    }

    fn claude(binary: &Path) -> AgentConfig {
        AgentConfig {
            harness_id: "claude-code".into(),
            adapter: HarnessAdapter::ClaudeCode,
            binary: binary.to_string_lossy().into_owned(),
            args: harness::template_args(harness::Adapter::ClaudeCode),
            env_allow: vec!["HOME".into()],
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    #[test]
    fn agent_entry_rejects_dyld_env_name() {
        let agent = claude(Path::new(&fixtures::absent_binary("claude")));
        validate_agent(&agent).unwrap();
        for name in [
            "DYLD_INSERT_LIBRARIES",
            "LD_PRELOAD",
            "home",
            "1HOME",
            "HOME=x",
            "",
        ] {
            let bad = AgentConfig {
                env_allow: vec![name.into()],
                ..agent.clone()
            };
            assert!(validate_agent(&bad).is_err(), "{name:?} was accepted");
        }
        let duplicate = AgentConfig {
            env_allow: vec!["HOME".into(), "HOME".into()],
            ..agent.clone()
        };
        assert!(validate_agent(&duplicate).is_err());
    }

    #[test]
    fn agent_entry_args_must_be_the_adapter_template() {
        let agent = claude(Path::new(&fixtures::absent_binary("claude")));
        let mut loosened = agent.args.clone();
        loosened.retain(|arg| arg != "--strict-mcp-config");
        let mut extra = agent.args.clone();
        extra.push("--safe-mode".into());
        for args in [
            loosened,
            extra,
            vec!["-p".into(), PROMPT_PLACEHOLDER.into()],
        ] {
            let bad = AgentConfig {
                args: args.clone(),
                ..agent.clone()
            };
            assert!(validate_agent(&bad).is_err(), "{args:?} was accepted");
        }
        let codex = AgentConfig {
            harness_id: "codex".into(),
            adapter: HarnessAdapter::Codex,
            args: harness::codex_args("0.144.4").unwrap(),
            ..agent.clone()
        };
        validate_agent(&codex).unwrap();
        let unreviewed = AgentConfig {
            args: harness::template_args(harness::Adapter::Codex),
            ..codex
        };
        assert!(validate_agent(&unreviewed).is_err());
    }

    #[test]
    fn agent_gate_needs_codex_args_reviewed_for_the_recorded_version() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path().join("notes");
        fs::create_dir(&root).unwrap();
        let binary = folder.path().join("codex");
        fs::write(&binary, "#!/bin/sh\nexit 1\n").unwrap();
        let path = folder.path().join("config.toml");
        fixtures::write(
            &path,
            &root,
            "summaries_dir = \"summaries\"",
            "agent_denylist_confirmed = true",
            &format!(
                "{}{}",
                fixtures::harness_table("codex", &binary, &harness::codex_args("0.144.4").unwrap()),
                fixtures::record_table_for("codex", "0.144.4", &binary)
            ),
        );
        let store = loaded(&path);
        assert!(store.snapshot().agent.is_some());
        assert_ready_here(&store, "codex", "0.144.4");

        // Setup recorded a version whose reviewed args differ (none here).
        fixtures::write(
            &path,
            &root,
            "summaries_dir = \"summaries\"",
            "agent_denylist_confirmed = true",
            &format!(
                "{}{}",
                fixtures::harness_table("codex", &binary, &harness::codex_args("0.144.4").unwrap()),
                fixtures::record_table_for("codex", "0.145.0", &binary)
            ),
        );
        let store = loaded(&path);
        assert_eq!(
            refusal(&store),
            (ErrorCode::NotConfigured, AGENT_ENTRY_INVALID.into())
        );
        assert_eq!(store.agent_status().state, AgentState::HarnessProblem);
    }

    #[test]
    fn agent_entry_needs_one_standalone_prompt_and_absolute_binary() {
        let agent = claude(Path::new(&fixtures::absent_binary("claude")));
        for args in [
            vec!["-p".to_owned()],
            vec!["{prompt}".to_owned(), "{prompt}".to_owned()],
            vec!["--prompt={prompt}".to_owned()],
        ] {
            let bad = AgentConfig {
                args: args.clone(),
                ..agent.clone()
            };
            assert!(validate_agent(&bad).is_err(), "{args:?} was accepted");
        }
        for bad in [
            AgentConfig {
                binary: "claude".into(),
                ..agent.clone()
            },
            AgentConfig {
                adapter: HarnessAdapter::Codex,
                ..agent.clone()
            },
            AgentConfig {
                timeout_secs: 0,
                ..agent.clone()
            },
            AgentConfig {
                timeout_secs: MAX_TIMEOUT_SECS + 1,
                ..agent.clone()
            },
        ] {
            assert!(validate_agent(&bad).is_err());
        }
    }

    #[test]
    fn agent_readiness_refuses_unconfirmed() {
        let folder = tempfile::tempdir().unwrap();
        let (path, root, binary) = fixtures::ready(folder.path());
        fixtures::write(
            &path,
            &root,
            "summaries_dir = \"summaries\"",
            "agent_denylist_confirmed = false",
            &format!(
                "{}{}",
                fixtures::agent_table(&binary),
                fixtures::record_table(&binary)
            ),
        );
        let store = loaded(&path);
        assert!(store.snapshot().agent.is_some());
        assert_eq!(
            refusal(&store),
            (ErrorCode::NotConfigured, CONFIRM_DENYLIST.into())
        );
        let status = store.agent_status();
        assert_eq!(status.state, AgentState::DenylistUnconfirmed);
        assert_eq!(status.harness_version.as_deref(), Some("2.1.284"));
    }

    #[test]
    fn agent_readiness_refuses_without_harness() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        fixtures::write(
            &path,
            folder.path(),
            "summaries_dir = \"summaries\"",
            "agent_denylist_confirmed = true",
            "",
        );
        let store = loaded(&path);
        assert_eq!(
            refusal(&store),
            (ErrorCode::NotConfigured, SET_UP_HARNESS.into())
        );
        assert_eq!(store.agent_status().state, AgentState::NotSetUp);
    }

    #[test]
    fn agent_readiness_refuses_without_record_or_summaries_folder() {
        let folder = tempfile::tempdir().unwrap();
        let (path, root, binary) = fixtures::ready(folder.path());
        fixtures::write(
            &path,
            &root,
            "summaries_dir = \"summaries\"",
            "agent_denylist_confirmed = true",
            &fixtures::agent_table(&binary),
        );
        let store = loaded(&path);
        assert_eq!(
            refusal(&store),
            (ErrorCode::NotConfigured, SET_UP_HARNESS.into())
        );
        assert_eq!(store.agent_status().state, AgentState::HarnessProblem);

        fixtures::write(
            &path,
            &root,
            "",
            "agent_denylist_confirmed = true",
            &format!(
                "{}{}",
                fixtures::agent_table(&binary),
                fixtures::record_table(&binary)
            ),
        );
        assert_eq!(
            refusal(&loaded(&path)),
            (ErrorCode::NotConfigured, CHOOSE_SUMMARIES.into())
        );
    }

    #[test]
    fn harness_commit_writes_entry_record_and_confirmation_in_one_write() {
        let folder = tempfile::tempdir().unwrap();
        let (path, root, binary) = fixtures::ready(folder.path());
        let record: HarnessRecord = loaded(&path).agent.record.clone().unwrap();
        fixtures::write(&path, &root, "summaries_dir = \"summaries\"", "", "");
        let mut store = loaded(&path);
        let revision = store.revision().to_owned();
        let next = ConfigSnapshot {
            agent_denylist_confirmed: true,
            agent: Some(claude(&binary)),
            ..store.snapshot().clone()
        };

        // A record for a different binary never commits.
        let stale = HarnessCommit {
            agent: claude(&binary),
            record: HarnessRecord {
                size: record.size + 1,
                ..record.clone()
            },
        };
        let before = fs::read(&path).unwrap();
        assert!(
            store
                .update_with_grants(next.clone(), &revision, None, Some(stale))
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), before);

        let commit = HarnessCommit {
            agent: claude(&binary),
            record,
        };
        store
            .update_with_grants(next, &revision, None, Some(commit))
            .unwrap()
            .unwrap();
        let store = loaded(&path);
        assert!(store.snapshot().agent_denylist_confirmed);
        assert_eq!(store.snapshot().agent, Some(claude(&binary)));
        assert_ready_here(&store, "claude-code", "2.1.284");
    }

    /// A config with a notes folder and no harness, then one harness commit.
    /// Returns the config path, the fake binary, and the committed revision.
    fn committed(folder: &Path) -> (PathBuf, PathBuf, String) {
        let (path, root, binary) = fixtures::ready(folder);
        let record: HarnessRecord = loaded(&path).agent.record.clone().unwrap();
        fixtures::write(
            &path,
            &root,
            "summaries_dir = \"summaries\"",
            "agent_denylist = [\"bank.example\"]",
            "",
        );
        let mut store = loaded(&path);
        assert_eq!(store.agent_status().state, AgentState::NotSetUp);
        let next = ConfigSnapshot {
            agent_denylist_confirmed: true,
            agent: Some(claude(&binary)),
            ..store.snapshot().clone()
        };
        let commit = HarnessCommit {
            agent: claude(&binary),
            record,
        };
        let revision = store.revision().to_owned();
        let revision = store
            .update_with_grants(next, &revision, None, Some(commit))
            .unwrap()
            .unwrap();
        (path, binary, revision)
    }

    #[test]
    fn harness_commit_writes_record_and_flag_in_one_revision() {
        let folder = tempfile::tempdir().unwrap();
        let (path, binary, revision) = committed(folder.path());
        let text = fs::read_to_string(&path).unwrap();
        // One serialization carries all three; the revision is of that file.
        assert_eq!(revision_for(text.as_bytes()), revision);
        assert!(text.contains("agent_denylist_confirmed = true"), "{text}");
        assert!(text.contains("[agent"), "{text}");
        assert!(text.contains("[harness_record]"), "{text}");
        assert!(text.contains("probe_passed_at"), "{text}");
        let store = loaded(&path);
        assert_eq!(store.revision(), revision);
        assert_eq!(store.snapshot().agent, Some(claude(&binary)));
        assert_eq!(
            store
                .agent
                .record
                .as_ref()
                .map(|record| record.version.as_str()),
            Some("2.1.284")
        );
    }

    #[test]
    fn harness_record_survives_unrelated_updates() {
        let folder = tempfile::tempdir().unwrap();
        let (path, _, revision) = committed(folder.path());
        let mut store = loaded(&path);
        let record = store.agent.record.clone();
        let next = ConfigSnapshot {
            near_repeat_secs: 600,
            ..store.snapshot().clone()
        };
        let updated = store.update(next, &revision).unwrap().unwrap();
        assert_ne!(updated, revision);
        let store = loaded(&path);
        assert_eq!(store.snapshot().near_repeat_secs, 600);
        assert!(store.snapshot().agent_denylist_confirmed);
        assert_eq!(store.agent.record, record);
        assert_ready_here(&store, "claude-code", "2.1.284");
    }

    #[test]
    fn agent_readiness_ready_after_commit() {
        let folder = tempfile::tempdir().unwrap();
        let (path, _, _) = committed(folder.path());
        assert_ready_here(&loaded(&path), "claude-code", "2.1.284");
    }

    /// Step 1 refuses AI commands off unix even for a harness whose identity
    /// is unchanged since setup.
    #[cfg(not(unix))]
    #[test]
    fn agent_readiness_refuses_on_this_platform() {
        let folder = tempfile::tempdir().unwrap();
        let (path, _, binary) = fixtures::ready(folder.path());
        let store = loaded(&path);
        assert_eq!(
            store.agent.record.as_ref().map(HarnessRecord::identity),
            Some(harness::identity(&binary).unwrap())
        );
        assert_eq!(
            refusal(&store),
            (ErrorCode::NotConfigured, AI_UNAVAILABLE_HERE.into())
        );
        assert_eq!(store.agent_status().state, AgentState::HarnessProblem);
    }

    #[cfg(unix)]
    #[test]
    fn agent_readiness_refuses_changed_identity() {
        let folder = tempfile::tempdir().unwrap();
        let (path, binary, _) = committed(folder.path());
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(&binary)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let store = loaded(&path);
        assert_eq!(
            refusal(&store),
            (ErrorCode::NotConfigured, HARNESS_CHANGED.into())
        );
        assert_eq!(store.agent_status().state, AgentState::HarnessProblem);
    }

    #[test]
    fn capture_remains_off_until_site_authorization_exists() {
        let config = ConfigSnapshot {
            storage: None,
            capture_enabled: true,
            ..empty_config()
        };
        assert!(validate(&config).is_err());
    }

    #[test]
    fn rejects_overlapping_content_locations() {
        let folder = tempfile::tempdir().unwrap();
        let config = ConfigSnapshot {
            storage: Some(StorageConfig {
                root: folder.path().to_string_lossy().into_owned(),
                profile: "neutral".into(),
                log_dir: "notes".into(),
                pages_dir: "notes/pages".into(),
                later_dir: "later".into(),
                summaries_dir: None,
            }),
            capture_enabled: false,
            ..empty_config()
        };
        assert!(validate(&config).is_err());
    }

    #[test]
    fn empty_config_round_trips_through_an_atomic_update() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        let mut store = ConfigStore::for_test(path.clone());
        let revision = store
            .update(empty_config(), MISSING_REVISION)
            .unwrap()
            .unwrap();
        let contents = fs::read_to_string(path).unwrap();
        let restored: StoredConfig = toml::from_str(&contents).unwrap();
        assert_eq!(ConfigSnapshot::from(restored), empty_config());
        assert_eq!(revision, revision_for(contents.as_bytes()));
    }

    #[test]
    fn stale_revision_does_not_replace_config() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        let mut first = ConfigStore::for_test(path.clone());
        let mut second = ConfigStore::for_test(path.clone());
        let committed = first
            .update(empty_config(), MISSING_REVISION)
            .unwrap()
            .unwrap();
        let before = fs::read(&path).unwrap();

        assert_eq!(
            second.update(empty_config(), MISSING_REVISION).unwrap(),
            None
        );
        assert_eq!(second.revision(), committed);
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn two_writers_can_commit_in_revision_order() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        let mut first = ConfigStore::for_test(path.clone());
        let mut second = ConfigStore::for_test(path);
        let first_revision = first
            .update(empty_config(), MISSING_REVISION)
            .unwrap()
            .unwrap();
        assert_eq!(
            second.update(empty_config(), MISSING_REVISION).unwrap(),
            None
        );

        let next = ConfigSnapshot {
            storage: Some(StorageConfig {
                root: folder.path().to_string_lossy().into_owned(),
                profile: "neutral".into(),
                log_dir: "log".into(),
                pages_dir: "pages".into(),
                later_dir: "later".into(),
                summaries_dir: None,
            }),
            capture_enabled: false,
            ..empty_config()
        };
        let root_identity = selected_root_identity(folder.path()).unwrap();
        let second_revision = second
            .update_with_root_identity(next.clone(), &first_revision, Some(&root_identity))
            .unwrap()
            .unwrap();
        assert_ne!(first_revision, second_revision);
        first.refresh().unwrap();
        assert_eq!(first.snapshot(), &next);
        assert_eq!(first.revision(), second_revision);
    }

    #[test]
    fn legacy_same_path_requires_and_accepts_fresh_selection() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        let next = ConfigSnapshot {
            storage: Some(StorageConfig {
                root: folder.path().to_string_lossy().into_owned(),
                profile: "neutral".into(),
                log_dir: "log".into(),
                pages_dir: "pages".into(),
                later_dir: "later".into(),
                summaries_dir: None,
            }),
            ..empty_config()
        };
        // Old M1 files recorded picker confirmation but not folder identity.
        fs::write(
            &path,
            toml::to_string(&StoredConfig::from_snapshot(
                &next,
                None,
                &AgentView::default(),
            ))
            .unwrap(),
        )
        .unwrap();
        let mut store = ConfigStore::for_test(path);
        store.refresh().unwrap();
        assert!(store.config_issue().is_some());
        assert!(store.snapshot().storage.is_none());
        let revision = store.revision().to_owned();
        assert!(store.update(next.clone(), &revision).is_err());
        let identity = selected_root_identity(folder.path()).unwrap();
        store
            .update_with_root_identity(next.clone(), &revision, Some(&identity))
            .unwrap()
            .unwrap();
        store.refresh().unwrap();
        assert!(store.config_issue().is_none());
        assert_eq!(store.snapshot(), &next);
    }

    #[test]
    fn replacing_selected_directory_requires_reselection() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("selected");
        let moved = base.path().join("moved");
        fs::create_dir(&root).unwrap();
        let path = base.path().join("config.toml");
        let mut store = ConfigStore::for_test(path);
        let config = ConfigSnapshot {
            storage: Some(StorageConfig {
                root: root.to_string_lossy().into_owned(),
                profile: "neutral".into(),
                log_dir: "log".into(),
                pages_dir: "pages".into(),
                later_dir: "later".into(),
                summaries_dir: None,
            }),
            ..empty_config()
        };
        let identity = selected_root_identity(&root).unwrap();
        store
            .update_with_root_identity(config.clone(), MISSING_REVISION, Some(&identity))
            .unwrap()
            .unwrap();
        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        store.refresh().unwrap();
        assert!(store.config_issue().is_some());
        assert!(!store.configured());
        // Only the folder needs reselection; the settings are kept for repair.
        assert_eq!(store.snapshot(), &config);
        let revision = store.revision().to_owned();
        // The stored identity no longer matches, so the same path alone is
        // not enough to authorize the replacement folder.
        assert!(store.update(config.clone(), &revision).is_err());
        let new_identity = selected_root_identity(&root).unwrap();
        assert_ne!(identity, new_identity);
        store
            .update_with_root_identity(config, &revision, Some(&new_identity))
            .unwrap()
            .unwrap();
        store.refresh().unwrap();
        assert!(store.config_issue().is_none());
        // The file was valid; only its identity was stale, so nothing to back up.
        assert_eq!(backup_count(base.path()), 0);
    }

    #[test]
    fn remounted_or_missing_folder_keeps_settings_without_a_backup() {
        // An external or network volume can disappear, or come back with a
        // new device number. Neither may wipe the site allowlist.
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("selected");
        let away = base.path().join("away");
        fs::create_dir(&root).unwrap();
        let path = base.path().join("config.toml");
        let mut store = ConfigStore::for_test(path.clone());
        let config = ConfigSnapshot {
            storage: Some(StorageConfig {
                root: root.to_string_lossy().into_owned(),
                profile: "neutral".into(),
                log_dir: "log".into(),
                pages_dir: "pages".into(),
                later_dir: "later".into(),
                summaries_dir: None,
            }),
            capture_enabled: true,
            sites: vec![SiteConfig {
                origin: "https://example.com".into(),
                path_prefix: "/".into(),
            }],
            ..empty_config()
        };
        let identity = selected_root_identity(&root).unwrap();
        store
            .update_with_root_identity(config.clone(), MISSING_REVISION, Some(&identity))
            .unwrap()
            .unwrap();
        let saved = fs::read(&path).unwrap();

        // Unmounted: the folder is gone. Capture stops, settings stay.
        fs::rename(&root, &away).unwrap();
        store.refresh().unwrap();
        assert!(store.config_issue().is_some());
        assert!(!store.configured());
        assert_eq!(store.snapshot(), &config);
        assert_eq!(store.root_identity(), Some(identity.as_str()));

        // Remounted with the same identity: no repair is needed at all.
        fs::rename(&away, &root).unwrap();
        store.refresh().unwrap();
        assert!(store.config_issue().is_none());
        assert!(store.configured());
        assert_eq!(fs::read(&path).unwrap(), saved);
        assert_eq!(backup_count(base.path()), 0);
    }

    #[test]
    fn unreadable_config_degrades_to_an_issue_at_startup() {
        // A directory (or FIFO, or unreadable file) at the config path must
        // not stop the host: it serves invalid_config errors instead.
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        fs::create_dir(&path).unwrap();
        let mut store = ConfigStore::load_from(path);
        assert!(store.config_issue().is_some());
        assert!(!store.configured());
        assert_eq!(store.snapshot(), &empty_config());
        assert!(store.refresh().is_err());
        assert!(store.config_issue().is_some());
    }

    #[test]
    fn a_config_that_becomes_unreadable_turns_inert_on_refresh() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        let mut store = ConfigStore::for_test(path.clone());
        store.refresh().unwrap();
        assert!(store.config_issue().is_none());
        fs::create_dir(&path).unwrap();
        assert!(store.refresh().is_err());
        assert!(store.config_issue().is_some());
        assert!(!store.configured());
        assert_eq!(store.snapshot(), &empty_config());
        assert_eq!(store.revision(), UNREADABLE_REVISION);
        fs::remove_dir(&path).unwrap();
        store.refresh().unwrap();
        assert!(store.config_issue().is_none());
    }

    #[test]
    fn oversized_config_past_the_hash_cap_can_be_repaired() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        let oversized = vec![b'#'; MAX_HASHED_CONFIG_BYTES as usize + 10];
        fs::write(&path, &oversized).unwrap();
        let mut store = ConfigStore::for_test(path.clone());
        store.refresh().unwrap();
        assert!(store.config_issue().is_some());
        let revision = store.revision().to_owned();
        store
            .update(empty_config(), &revision)
            .unwrap()
            .expect("repair should commit");
        store.refresh().unwrap();
        assert!(store.config_issue().is_none());
        assert_eq!(backup_count(folder.path()), 1);
        let backup = fs::read_dir(folder.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| {
                entry
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("config-invalid-")
            })
            .unwrap();
        assert_eq!(fs::read(backup).unwrap(), oversized);
    }

    fn backup_count(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config-invalid-")
            })
            .count()
    }

    #[test]
    fn oversized_json_is_rejected_before_persistence() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        let mut store = ConfigStore::for_test(path.clone());
        let large = ConfigSnapshot {
            storage: Some(StorageConfig {
                root: "x".repeat(MAX_CONFIG_BYTES),
                profile: "neutral".into(),
                log_dir: "log".into(),
                pages_dir: "pages".into(),
                later_dir: "later".into(),
                summaries_dir: None,
            }),
            capture_enabled: false,
            ..empty_config()
        };
        assert!(store.update(large, MISSING_REVISION).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn case_insensitive_overlap_uses_path_components() {
        let folder = tempfile::tempdir().unwrap();
        let storage = StorageConfig {
            root: folder.path().to_string_lossy().into_owned(),
            profile: "neutral".into(),
            log_dir: "Notes/Pages".into(),
            pages_dir: "notes".into(),
            later_dir: "later".into(),
            summaries_dir: None,
        };
        assert!(validate_storage(&storage).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn backslash_paths_are_checked_as_components_on_windows() {
        let folder = tempfile::tempdir().unwrap();
        let storage = StorageConfig {
            root: folder.path().to_string_lossy().into_owned(),
            profile: "neutral".into(),
            log_dir: "Notes\\Pages".into(),
            pages_dir: "notes".into(),
            later_dir: "later".into(),
            summaries_dir: None,
        };
        assert!(validate_storage(&storage).is_err());
    }

    #[test]
    fn startup_sweep_removes_only_stale_config_temporaries() {
        use crate::vault::tests::age_file;
        use std::time::Duration;

        let folder = tempfile::tempdir().unwrap();
        let old = STALE_TEMPORARY_AGE + Duration::from_secs(60);
        let stale = folder
            .path()
            .join(format!(".config-{}.tmp", Uuid::new_v4()));
        let fresh = folder
            .path()
            .join(format!(".config-{}.tmp", Uuid::new_v4()));
        let other = folder.path().join(".config-existing.tmp");
        let config = folder.path().join("config.toml");
        for path in [&stale, &fresh, &other, &config] {
            fs::write(path, "content").unwrap();
        }
        for path in [&stale, &other, &config] {
            age_file(path, old);
        }
        let store = ConfigStore::for_test(config.clone());
        store.sweep_stale_temporaries();
        assert!(!stale.exists());
        for path in [&fresh, &other, &config] {
            assert!(path.exists(), "{} was removed", path.display());
        }
    }

    #[test]
    fn temporary_name_collision_preserves_existing_file() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join(".config-existing.tmp");
        fs::write(&path, "existing").unwrap();
        assert!(create_temporary(&path).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "existing");
    }
}
