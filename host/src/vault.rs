//! Capability-scoped vault access. The extension never supplies a filesystem
//! path. M0 only creates new page files; later note edits must add managed
//! block ownership and conflict detection before replacing an existing file.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use rauser_protocol::StorageConfig;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

const MAX_NOTE_BYTES: usize = 4 * 1024 * 1024;

pub struct Vault {
    root: Dir,
    pages_dir: PathBuf,
}

impl Vault {
    pub fn open(storage: &StorageConfig) -> Result<Self> {
        let chosen = Path::new(&storage.root);
        if !chosen.is_absolute() {
            bail!("notes folder must be an absolute path");
        }
        let canonical = fs::canonicalize(chosen)
            .with_context(|| format!("resolving notes folder {}", chosen.display()))?;
        if !canonical.is_dir() {
            bail!("notes folder is not a directory: {}", canonical.display());
        }
        let pages_dir = checked_relative_dir(&storage.pages_dir)?;
        checked_relative_dir(&storage.log_dir)?;
        checked_relative_dir(&storage.later_dir)?;

        // This is the sole ambient filesystem entry point for vault contents.
        // cap-std resolves subsequent paths relative to this open directory and
        // prevents symlinks from escaping it.
        let root = Dir::open_ambient_dir(&canonical, ambient_authority())
            .with_context(|| format!("opening notes folder {}", canonical.display()))?;
        Ok(Self { root, pages_dir })
    }

    /// Atomically publish a new note, refusing to replace any existing file.
    /// The filename is derived from the URL, never from an extension path.
    pub fn create_page(&self, page_url: &str, markdown: &str) -> Result<PathBuf> {
        if markdown.len() > MAX_NOTE_BYTES {
            bail!("page note exceeds the 4 MiB write limit");
        }
        let filename = page_filename(page_url)?;
        self.root
            .create_dir_all(&self.pages_dir)
            .context("creating page notes directory")?;
        let pages = self
            .root
            .open_dir(&self.pages_dir)
            .context("opening page notes directory")?;

        let temporary = format!(".rauser-{}.tmp", Uuid::new_v4());
        let _cleanup = TempFileCleanup {
            dir: &pages,
            name: &temporary,
        };
        let mut file = pages
            .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
            .context("creating page note temporary file")?;
        file.write_all(markdown.as_bytes())?;
        file.sync_all()?;
        drop(file);

        // A hard link is an atomic no-clobber publication on the same volume.
        // rename() would silently replace a user file with this name.
        pages
            .hard_link(&temporary, &pages, &filename)
            .with_context(|| {
                format!("page note already exists or cannot be created: {filename}")
            })?;
        pages.remove_file(&temporary)?;
        sync_directory(&pages)?;
        Ok(self.pages_dir.join(filename))
    }
}

fn checked_relative_dir(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        bail!("content location must be a nonempty relative directory");
    }
    Ok(path.to_path_buf())
}

pub fn page_filename(page_url: &str) -> Result<String> {
    let mut parsed = Url::parse(page_url).context("page URL is invalid")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("page URL must be HTTP or HTTPS with a host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("page URL must not contain credentials");
    }
    parsed.set_fragment(None);
    let canonical = parsed.as_str();
    let digest = Sha256::digest(canonical.as_bytes());
    let stem = format!("{}{}", parsed.host_str().unwrap_or_default(), parsed.path());
    let slug = slug(&stem);
    Ok(format!("{slug}-{}.md", hex::encode(digest)))
}

fn slug(value: &str) -> String {
    let mut result = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        if result.len() >= 64 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            result.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !result.is_empty() {
            result.push('-');
            last_dash = true;
        }
    }
    while result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() {
        "page".to_owned()
    } else {
        result
    }
}

struct TempFileCleanup<'a> {
    dir: &'a Dir,
    name: &'a str,
}

impl Drop for TempFileCleanup<'_> {
    fn drop(&mut self) {
        let _ = self.dir.remove_file(self.name);
    }
}

#[cfg(not(windows))]
fn sync_directory(dir: &Dir) -> Result<()> {
    dir.try_clone()?.into_std_file().sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_dir: &Dir) -> Result<()> {
    // std does not expose a portable way to fsync a Windows directory handle.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage(root: &Path, pages_dir: &str) -> StorageConfig {
        StorageConfig {
            root: root.to_string_lossy().into_owned(),
            profile: "neutral".into(),
            log_dir: "log".into(),
            pages_dir: pages_dir.into(),
            later_dir: "later".into(),
        }
    }

    #[test]
    fn rejects_traversal_in_configured_directory() {
        let root = tempfile::tempdir().unwrap();
        assert!(Vault::open(&storage(root.path(), "../escape")).is_err());
        assert!(Vault::open(&storage(root.path(), "/tmp/escape")).is_err());
    }

    #[test]
    fn creating_a_page_never_replaces_an_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        let relative = vault
            .create_page("https://example.com/article#part-1", "first")
            .unwrap();
        assert!(
            vault
                .create_page("https://example.com/article#part-2", "second")
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(root.path().join(relative)).unwrap(),
            "first"
        );
    }

    #[test]
    fn an_existing_user_file_with_the_generated_name_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let pages = root.path().join("pages");
        fs::create_dir(&pages).unwrap();
        let name = page_filename("https://example.com/user-note").unwrap();
        let user_file = pages.join(name);
        fs::write(&user_file, "user content").unwrap();

        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        assert!(
            vault
                .create_page("https://example.com/user-note", "generated")
                .is_err()
        );
        assert_eq!(fs::read_to_string(user_file).unwrap(), "user content");
    }

    #[cfg(unix)]
    #[test]
    fn configured_symlink_cannot_escape_the_vault() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("pages")).unwrap();
        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        assert!(vault.create_page("https://example.com", "content").is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
