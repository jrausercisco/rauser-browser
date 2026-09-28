//! The host owns configuration. An absent file is an unconfigured, inert state.
//! An update is validated in full before the old file is atomically replaced.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use brauser_protocol::{ConfigSnapshot, SiteConfig, StorageConfig};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::brand::{APP_NAME, NAMESPACE};
use crate::vault::{STALE_TEMPORARY_AGE, Vault, is_generated_temporary, selected_root_identity};

const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_ROOT_PATH_BYTES: usize = 4 * 1024;
const MAX_LOCATION_PATH_BYTES: usize = 512;
const MAX_PATH_COMPONENT_BYTES: usize = 255;
const MAX_SITES: usize = 128;
const MAX_STRIP_PARAMS: usize = 64;
const MAX_RULE_BYTES: usize = 64;
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
}

struct DiskState {
    config: ConfigSnapshot,
    revision: String,
    issue: Option<String>,
    needs_backup: bool,
    root_identity: Option<String>,
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
            Ok(state) => Self {
                path,
                config: state.config,
                revision: state.revision,
                issue: state.issue,
                root_identity: state.root_identity,
            },
            // Exiting here would show the extension only a disconnect. Start
            // inert instead; each request refreshes, fails the same way, and
            // answers invalid_config until the file is readable again.
            Err(error) => {
                eprintln!("{NAMESPACE}: warning: configuration is unreadable: {error:#}");
                let mut store = Self {
                    path,
                    config: empty_config(),
                    revision: String::new(),
                    issue: None,
                    root_identity: None,
                };
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

    pub fn snapshot(&self) -> &ConfigSnapshot {
        &self.config
    }

    pub fn revision(&self) -> &str {
        &self.revision
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
        self.config = state.config;
        self.revision = state.revision;
        self.issue = state.issue;
        self.root_identity = state.root_identity;
        Ok(())
    }

    pub fn configured(&self) -> bool {
        self.issue.is_none()
            && self.config.storage.is_some()
            && validate_persisted(&self.config).is_ok()
    }

    /// Replace config only when the caller's revision still matches the file.
    /// `None` is a conflict; `Some(revision)` is a committed update.
    #[cfg(test)]
    pub(crate) fn update(
        &mut self,
        next: ConfigSnapshot,
        expected_revision: &str,
    ) -> Result<Option<String>> {
        self.update_with_root_identity(next, expected_revision, None)
    }

    pub(crate) fn update_with_root_identity(
        &mut self,
        next: ConfigSnapshot,
        expected_revision: &str,
        selected_identity: Option<&str>,
    ) -> Result<Option<String>> {
        let json = serde_json::to_vec(&next).context("serializing config JSON")?;
        if json.len() > MAX_CONFIG_BYTES {
            bail!("configuration exceeds the 64 KiB size limit");
        }
        validate(&next)?;
        let parent = self.path.parent().context("config path has no parent")?;
        let _lock_file = self.lock_current()?;
        let current = read_disk_state(&self.path)?;
        if expected_revision != current.revision {
            self.config = current.config;
            self.revision = current.revision;
            self.issue = current.issue;
            self.root_identity = current.root_identity;
            return Ok(None);
        }

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
        let serialized =
            toml::to_string_pretty(&StoredConfig::from_snapshot(&next, next_identity.clone()))
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
                self.config = after_backup.config;
                self.revision = after_backup.revision;
                self.issue = after_backup.issue;
                self.root_identity = after_backup.root_identity;
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
        if let Err(error) = sync_parent(parent) {
            eprintln!(
                "{NAMESPACE}: warning: config was saved but directory sync failed: {error:#}"
            );
        }
        Ok(Some(revision))
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        Self {
            path,
            config: empty_config(),
            revision: MISSING_REVISION.into(),
            issue: None,
            root_identity: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    #[serde(default)]
    storage: Option<StorageConfig>,
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
}

impl StoredConfig {
    fn from_snapshot(value: &ConfigSnapshot, root_identity: Option<String>) -> Self {
        Self {
            storage: value.storage.clone(),
            // Only native dispatch calls ConfigStore::update, after verifying
            // the picker grant for a new root. This marks the root's M1 origin.
            root_picker_confirmed: value.storage.is_some(),
            root_identity,
            capture_enabled: value.capture_enabled,
            sites: value.sites.clone(),
            strip_params: value.strip_params.clone(),
            near_repeat_secs: value.near_repeat_secs,
        }
    }
}

impl From<StoredConfig> for ConfigSnapshot {
    fn from(value: StoredConfig) -> Self {
        Self {
            storage: value.storage,
            capture_enabled: value.capture_enabled,
            sites: value.sites,
            strip_params: value.strip_params,
            near_repeat_secs: value.near_repeat_secs,
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
        });
    }
    let parsed = std::str::from_utf8(&contents)
        .map_err(anyhow::Error::from)
        .and_then(|text| toml::from_str::<StoredConfig>(text).map_err(anyhow::Error::from));
    match parsed {
        Ok(stored) => {
            let root_identity = stored.root_identity.clone();
            let root_unconfirmed = stored.storage.is_some()
                && (!stored.root_picker_confirmed || root_identity.is_none());
            let config: ConfigSnapshot = stored.into();
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
            })
        }
        Err(_) => Ok(DiskState {
            config: empty_config(),
            revision,
            issue: Some("Configuration is invalid or from a newer version. Choose a notes folder and save a replacement; the original will be backed up.".into()),
            needs_backup: true,
            root_identity: None,
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

fn empty_config() -> ConfigSnapshot {
    ConfigSnapshot {
        storage: None,
        capture_enabled: false,
        sites: Vec::new(),
        strip_params: default_strip_params(),
        near_repeat_secs: default_near_repeat_secs(),
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

    let paths = [
        Path::new(&storage.log_dir),
        Path::new(&storage.pages_dir),
        Path::new(&storage.later_dir),
    ];
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
                bail!("log, pages, and later locations must not overlap");
            }
        }
    }
    Ok(())
}

fn has_unsafe_display_chars(value: &str) -> bool {
    value.chars().any(|ch| {
        ch.is_control()
            || matches!(
                ch,
                '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
    })
}

fn lower_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy().to_lowercase())
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

#[cfg(test)]
mod tests {
    use super::*;

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
            }),
            ..empty_config()
        };
        // Old M1 files recorded picker confirmation but not folder identity.
        fs::write(
            &path,
            toml::to_string(&StoredConfig::from_snapshot(&next, None)).unwrap(),
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
