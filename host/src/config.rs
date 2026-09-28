//! The host owns configuration. An absent file is an unconfigured, inert state.
//! An update is validated in full before the old file is atomically replaced.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use rauser_protocol::{ConfigSnapshot, StorageConfig};
use uuid::Uuid;

use crate::vault::Vault;

pub struct ConfigStore {
    path: PathBuf,
    config: ConfigSnapshot,
}

impl ConfigStore {
    pub fn default_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("", "", "Rauser")
            .context("cannot locate this user's application config directory")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::default_path()?;
        let config = match fs::read_to_string(&path) {
            Ok(contents) => toml::from_str::<ConfigSnapshot>(&contents)
                .with_context(|| format!("invalid config at {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => empty_config(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        // The folder may have moved since setup. Keep the config available for
        // repair through update_config, but never use it for vault I/O until a
        // fresh validation succeeds.
        Ok(Self { path, config })
    }

    pub fn snapshot(&self) -> &ConfigSnapshot {
        &self.config
    }

    pub fn configured(&self) -> bool {
        self.config.storage.is_some() && validate(&self.config).is_ok()
    }

    pub fn update(&mut self, next: ConfigSnapshot) -> Result<()> {
        validate(&next)?;
        let serialized = toml::to_string_pretty(&next).context("serializing config")?;
        let parent = self.path.parent().context("config path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

        let temporary = parent.join(format!(".config-{}.tmp", Uuid::new_v4()));
        let _cleanup = TempFileCleanup(&temporary);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        sync_parent(parent)?;
        self.config = next;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        Self {
            path,
            config: empty_config(),
        }
    }
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
        if path.as_os_str().is_empty()
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            bail!("content locations must be nonempty relative paths without . or .. components");
        }
    }
    for left in 0..paths.len() {
        for right in (left + 1)..paths.len() {
            if paths[left].starts_with(paths[right]) || paths[right].starts_with(paths[left]) {
                bail!("log, pages, and later locations must not overlap");
            }
            // Typical macOS and Windows volumes are case-insensitive. Reject
            // simple case-only aliases even when this machine's volume is not.
            let l = paths[left].to_string_lossy().to_lowercase();
            let r = paths[right].to_string_lossy().to_lowercase();
            if l == r || l.starts_with(&format!("{r}/")) || r.starts_with(&format!("{l}/")) {
                bail!("log, pages, and later locations must not overlap");
            }
        }
    }
    Ok(())
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
        store.update(empty_config()).unwrap();
        let contents = fs::read_to_string(path).unwrap();
        let restored: ConfigSnapshot = toml::from_str(&contents).unwrap();
        assert_eq!(restored, empty_config());
    }
}
