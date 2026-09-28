//! The host owns configuration. An absent file is an unconfigured, inert state.
//! An update is validated in full before the old file is atomically replaced.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use rauser_protocol::{ConfigSnapshot, StorageConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::vault::Vault;

const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_ROOT_PATH_BYTES: usize = 4 * 1024;
const MAX_LOCATION_PATH_BYTES: usize = 512;
const MAX_PATH_COMPONENT_BYTES: usize = 255;
const MISSING_REVISION: &str = "missing";

pub struct ConfigStore {
    path: PathBuf,
    config: ConfigSnapshot,
    revision: String,
}

impl ConfigStore {
    pub fn default_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("", "", "Rauser")
            .context("cannot locate this user's application config directory")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::default_path()?;
        let (config, revision) = read_disk_state(&path)?;
        // The folder may have moved since setup. Keep the config available for
        // repair through update_config, but never use it for vault I/O until a
        // fresh validation succeeds.
        Ok(Self {
            path,
            config,
            revision,
        })
    }

    pub fn snapshot(&self) -> &ConfigSnapshot {
        &self.config
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn refresh(&mut self) -> Result<()> {
        let (config, revision) = read_disk_state(&self.path)?;
        self.config = config;
        self.revision = revision;
        Ok(())
    }

    pub fn configured(&self) -> bool {
        self.config.storage.is_some() && validate(&self.config).is_ok()
    }

    /// Replace config only when the caller's revision still matches the file.
    /// `None` is a conflict; `Some(revision)` is a committed update.
    pub fn update(
        &mut self,
        next: ConfigSnapshot,
        expected_revision: &str,
    ) -> Result<Option<String>> {
        let json = serde_json::to_vec(&next).context("serializing config JSON")?;
        if json.len() > MAX_CONFIG_BYTES {
            bail!("configuration exceeds the 64 KiB size limit");
        }
        validate(&next)?;
        let serialized = toml::to_string_pretty(&StoredConfig::from(&next))
            .context("serializing config TOML")?;
        if serialized.len() > MAX_CONFIG_BYTES {
            bail!("configuration exceeds the 64 KiB size limit");
        }
        let parent = self.path.parent().context("config path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

        // The lock file has a stable name. Locking config.toml itself would
        // leave a different inode locked after atomic replacement.
        let lock_path = parent.join(".config.lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        lock_file.lock().context("locking configuration")?;
        let (current, actual_revision) = read_disk_state(&self.path)?;
        if expected_revision != actual_revision {
            self.config = current;
            self.revision = actual_revision;
            return Ok(None);
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
        if let Err(error) = sync_parent(parent) {
            eprintln!("rauser: warning: config was saved but directory sync failed: {error:#}");
        }
        Ok(Some(revision))
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        Self {
            path,
            config: empty_config(),
            revision: MISSING_REVISION.into(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    #[serde(default)]
    storage: Option<StorageConfig>,
    #[serde(default)]
    capture_enabled: bool,
}

impl From<&ConfigSnapshot> for StoredConfig {
    fn from(value: &ConfigSnapshot) -> Self {
        Self {
            storage: value.storage.clone(),
            capture_enabled: value.capture_enabled,
        }
    }
}

impl From<StoredConfig> for ConfigSnapshot {
    fn from(value: StoredConfig) -> Self {
        Self {
            storage: value.storage,
            capture_enabled: value.capture_enabled,
        }
    }
}

fn read_disk_state(path: &Path) -> Result<(ConfigSnapshot, String)> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((empty_config(), MISSING_REVISION.into()));
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut contents = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut contents)
        .with_context(|| format!("reading {}", path.display()))?;
    if contents.len() > MAX_CONFIG_BYTES {
        bail!("configuration at {} exceeds 64 KiB", path.display());
    }
    let revision = revision_for(&contents);
    let text = std::str::from_utf8(&contents).context("configuration is not UTF-8")?;
    let stored: StoredConfig =
        toml::from_str(text).with_context(|| format!("invalid config at {}", path.display()))?;
    Ok((stored.into(), revision))
}

fn revision_for(contents: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(contents)))
}

fn create_temporary(path: &Path) -> Result<(File, TempFileCleanup<'_>)> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    // The guard is installed only after create_new succeeds. An existing file
    // with this name must never be removed on a collision or open failure.
    Ok((file, TempFileCleanup(path)))
}

fn empty_config() -> ConfigSnapshot {
    ConfigSnapshot {
        storage: None,
        capture_enabled: false,
    }
}

pub fn validate(config: &ConfigSnapshot) -> Result<()> {
    if config.capture_enabled {
        // M0 has no site allowlist or host permission contract. Capture cannot
        // be authorized safely until those fields and checks arrive in M1.
        bail!("capture cannot be enabled until site allowlists are implemented");
    }
    if let Some(storage) = &config.storage {
        validate_storage(storage)?;
        Vault::open(storage).context("notes folder is unavailable")?;
    }
    Ok(())
}

fn validate_storage(storage: &StorageConfig) -> Result<()> {
    if storage.root.len() > MAX_ROOT_PATH_BYTES {
        bail!("storage.root exceeds the 4096-byte path limit");
    }
    let root = Path::new(&storage.root);
    if !root.is_absolute() {
        bail!("storage.root must be an existing absolute folder selected by the user");
    }
    if storage.profile != "neutral" && storage.profile != "obsidian" {
        bail!("storage.profile must be neutral or obsidian in M0");
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

fn lower_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

struct TempFileCleanup<'a>(&'a Path);

impl Drop for TempFileCleanup<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
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
        };
        let second_revision = second
            .update(next.clone(), &first_revision)
            .unwrap()
            .unwrap();
        assert_ne!(first_revision, second_revision);
        first.refresh().unwrap();
        assert_eq!(first.snapshot(), &next);
        assert_eq!(first.revision(), second_revision);
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
    fn temporary_name_collision_preserves_existing_file() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join(".config-existing.tmp");
        fs::write(&path, "existing").unwrap();
        assert!(create_temporary(&path).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "existing");
    }
}
