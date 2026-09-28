//! Capability-scoped vault access. The extension never supplies a filesystem
//! path. Page notes are replaced whole under a caller-checked version and
//! ownership (§4.4); callers own that check, this module trusts it.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::brand::NAMESPACE;
use anyhow::{Context, Result, bail};
use brauser_protocol::StorageConfig;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

const MAX_NOTE_BYTES: usize = 4 * 1024 * 1024;
/// A live save renames its temporary within one request, so one this old was
/// left by a host that was killed mid-save. The margin keeps a startup sweep
/// from racing another host process's save in progress.
pub(crate) const STALE_TEMPORARY_AGE: Duration = Duration::from_secs(60 * 60);

pub struct Vault {
    root: Dir,
    pages_dir: PathBuf,
}

/// A page that was published under its final name. Warnings describe work that
/// failed after publication; callers must not retry the create as if it failed.
#[derive(Debug)]
pub struct CreatedPage {
    pub relative_path: PathBuf,
    pub warnings: Vec<PostPublishWarning>,
}

#[derive(Debug)]
pub enum PostPublishWarning {
    DirectorySync(io::Error),
}

impl Vault {
    pub fn open(storage: &StorageConfig) -> Result<Self> {
        Self::open_checked(storage, None)
    }

    /// The identity comes from the native picker and is persisted with config.
    /// Check the opened directory handle, not just a canonicalized pathname:
    /// a folder can be replaced between those two operations.
    pub fn open_checked(storage: &StorageConfig, expected_identity: Option<&str>) -> Result<Self> {
        let chosen = Path::new(&storage.root);
        let pages_dir = checked_relative_dir(&storage.pages_dir)?;
        checked_relative_dir(&storage.log_dir)?;
        checked_relative_dir(&storage.later_dir)?;
        let (root, _) = open_selected_root(chosen, expected_identity)?;
        Ok(Self { root, pages_dir })
    }

    /// Atomically replace (or create) the whole note. The filename is derived
    /// from the URL, never from an extension path. Callers must already have
    /// checked the caller's expected version and Brauser ownership of any
    /// existing file (§4.4); this call trusts the markdown it is given and
    /// replaces whatever currently occupies the generated name.
    pub fn replace_page(&self, page_url: &str, markdown: &str) -> Result<CreatedPage> {
        self.replace_page_with_post_publish_ops(page_url, markdown, sync_directory)
    }

    pub fn page_relative_path(&self, page_url: &str) -> Result<PathBuf> {
        Ok(self.pages_dir.join(page_filename(page_url)?))
    }

    /// Read a page generated from the same canonical URL. Callers must check
    /// ownership and identity themselves before treating an existing file as
    /// an owned note (§6.4).
    pub fn read_page(&self, page_url: &str) -> Result<Option<String>> {
        let filename = page_filename(page_url)?;
        self.read_named(&filename)
    }

    fn read_named(&self, filename: &str) -> Result<Option<String>> {
        let pages = match self.root.open_dir(&self.pages_dir) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("opening page notes directory"),
        };
        let metadata = match pages.symlink_metadata(filename) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("checking page note"),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("page-note name is occupied by a non-regular file");
        }
        if metadata.len() > MAX_NOTE_BYTES as u64 {
            bail!("existing page note exceeds the 4 MiB read limit");
        }
        let mut file = pages.open(filename).context("opening page note")?;
        let opened = file.metadata().context("checking opened page note")?;
        if !opened.is_file() {
            bail!("opened page note is not a regular file");
        }
        if opened.len() > MAX_NOTE_BYTES as u64 {
            bail!("opened page note exceeds the 4 MiB read limit");
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_NOTE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .context("reading page note")?;
        if bytes.len() > MAX_NOTE_BYTES {
            bail!("existing page note exceeds the 4 MiB read limit");
        }
        Ok(Some(
            String::from_utf8(bytes).context("existing page note is not UTF-8")?,
        ))
    }

    /// Remove page-note temporaries left by a host killed between creating
    /// and renaming them. Only regular files whose whole name is one this
    /// module generates, and that are older than `STALE_TEMPORARY_AGE`, are
    /// removed. Returns how many were removed.
    pub fn sweep_stale_temporaries(&self) -> Result<usize> {
        let pages = match self.root.open_dir(&self.pages_dir) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error).context("opening page notes directory"),
        };
        let prefix = format!(".{NAMESPACE}-");
        let now = SystemTime::now();
        let mut removed = 0;
        for entry in pages.entries().context("listing page notes directory")? {
            let entry = entry.context("listing page notes directory")?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !is_generated_temporary(name, &prefix) {
                continue;
            }
            // Not following symlinks: a link is not ours, whatever its name.
            let Ok(metadata) = pages.symlink_metadata(name) else {
                continue;
            };
            let stale = metadata.is_file()
                && metadata
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified.into_std()).ok())
                    .is_some_and(|elapsed| elapsed >= STALE_TEMPORARY_AGE);
            if stale && pages.remove_file(name).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Write a temp file, fsync it, then atomically rename it over the note's
    /// generated name, replacing whatever is currently there (§4.4). Unlike
    /// the old no-clobber hard-link publish, a rename is itself the atomic
    /// cleanup step: on success no temporary name is left to remove.
    fn replace_page_with_post_publish_ops<G>(
        &self,
        page_url: &str,
        markdown: &str,
        sync_parent: G,
    ) -> Result<CreatedPage>
    where
        G: FnOnce(&Dir) -> io::Result<()>,
    {
        if markdown.len() > MAX_NOTE_BYTES {
            bail!("page note exceeds the 4 MiB write limit");
        }
        let filename = page_filename(page_url)?;
        self.root
            .create_dir_all(&self.pages_dir)
            .context("creating page notes directory")?;
        sync_directory_chain(&self.root, &self.pages_dir)
            .context("syncing page notes parent directories")?;
        let pages = self
            .root
            .open_dir(&self.pages_dir)
            .context("opening page notes directory")?;

        let temporary = format!(".{NAMESPACE}-{}.tmp", Uuid::new_v4());
        let mut file = pages
            .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
            .context("creating page note temporary file")?;
        let mut cleanup = TempFileCleanup::new(&pages, &temporary);
        let write_result = file
            .write_all(markdown.as_bytes())
            .and_then(|_| file.sync_all());
        drop(file);
        write_result?;

        pages
            .rename(&temporary, &pages, &filename)
            .with_context(|| format!("replacing page note: {filename}"))?;
        cleanup.disarm();
        drop(cleanup);

        let directory_sync = sync_parent(&pages);
        let mut warnings = Vec::new();
        if let Err(error) = directory_sync {
            warnings.push(PostPublishWarning::DirectorySync(error));
        }
        Ok(CreatedPage {
            relative_path: self.pages_dir.join(filename),
            warnings,
        })
    }
}

/// Open the selected directory as a capability, then identify that handle.
/// Reopening a pathname must never silently authorize a replacement folder.
pub(crate) fn open_selected_root(
    chosen: &Path,
    expected_identity: Option<&str>,
) -> Result<(Dir, String)> {
    if !chosen.is_absolute() {
        bail!("notes folder must be an absolute path");
    }
    let canonical = fs::canonicalize(chosen)
        .with_context(|| format!("resolving notes folder {}", chosen.display()))?;
    if !canonical.is_dir() {
        bail!("notes folder is not a directory: {}", canonical.display());
    }
    // This is the sole ambient filesystem entry point for vault contents.
    // cap-std confines subsequent paths to the opened directory.
    let root = Dir::open_ambient_dir(&canonical, ambient_authority())
        .with_context(|| format!("opening notes folder {}", canonical.display()))?;
    let identity = directory_identity(&root)?;
    if expected_identity.is_some_and(|expected| expected != identity) {
        bail!("selected notes folder changed; choose it again");
    }
    Ok((root, identity))
}

pub(crate) fn selected_root_identity(chosen: &Path) -> Result<String> {
    let (_, identity) = open_selected_root(chosen, None)?;
    Ok(identity)
}

fn directory_identity(dir: &Dir) -> Result<String> {
    let metadata = dir
        .try_clone()
        .context("cloning notes folder handle")?
        .into_std_file()
        .metadata()
        .context("identifying notes folder")?;
    if !metadata.is_dir() {
        bail!("selected notes folder is not a directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!(
            "unix:{:016x}:{:016x}",
            metadata.dev(),
            metadata.ino()
        ))
    }
    #[cfg(windows)]
    {
        // std's by-handle metadata accessors are unstable; winapi-util wraps
        // GetFileInformationByHandle for the same volume and file IDs.
        let handle = dir
            .try_clone()
            .context("cloning notes folder handle")?
            .into_std_file();
        let info = winapi_util::file::information(&handle)
            .context("identifying notes folder on Windows")?;
        Ok(format!(
            "windows:{:08x}:{:016x}",
            info.volume_serial_number(),
            info.file_index()
        ))
    }
}

/// True only for `<prefix><hyphenated UUID>.tmp`, the exact shape this host
/// generates, so a user's own file is never mistaken for a leftover.
pub(crate) fn is_generated_temporary(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(".tmp"))
        .is_some_and(|id| Uuid::try_parse(id).is_ok_and(|uuid| uuid.hyphenated().to_string() == id))
}

pub(crate) fn checked_relative_dir(value: &str) -> Result<PathBuf> {
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

/// Removes a temporary file on early return. Disarm it once the final name is
/// published: another writer could reuse the temporary name after that.
pub(crate) struct TempFileCleanup<'a> {
    dir: &'a Dir,
    name: &'a str,
    armed: bool,
}

impl<'a> TempFileCleanup<'a> {
    pub(crate) fn new(dir: &'a Dir, name: &'a str) -> Self {
        Self {
            dir,
            name,
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileCleanup<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.dir.remove_file(self.name);
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn sync_directory(dir: &Dir) -> io::Result<()> {
    dir.try_clone()?.into_std_file().sync_all()?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn sync_directory(_dir: &Dir) -> io::Result<()> {
    // std does not expose a portable way to fsync a Windows directory handle.
    Ok(())
}

pub(crate) fn sync_directory_chain(root: &Dir, relative: &Path) -> io::Result<()> {
    let mut current = root.try_clone()?;
    sync_directory(&current)?;
    for part in relative.components() {
        current = current.open_dir(part.as_os_str())?;
        sync_directory(&current)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
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
    fn replacing_a_page_creates_it_when_absent() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        let created = vault
            .replace_page("https://example.com/article#part-1", "first")
            .unwrap();
        assert!(created.warnings.is_empty());
        assert_eq!(
            fs::read_to_string(root.path().join(&created.relative_path)).unwrap(),
            "first"
        );
        // The fragment is not part of the generated identity.
        assert_eq!(
            created.relative_path,
            vault
                .page_relative_path("https://example.com/article#part-2")
                .unwrap()
        );
    }

    #[test]
    fn replacing_a_page_overwrites_existing_content() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        let first = vault
            .replace_page("https://example.com/article", "first")
            .unwrap();
        let second = vault
            .replace_page("https://example.com/article", "second")
            .unwrap();
        assert_eq!(first.relative_path, second.relative_path);
        assert_eq!(
            fs::read_to_string(root.path().join(second.relative_path)).unwrap(),
            "second"
        );
    }

    #[test]
    fn post_publish_directory_sync_failure_reports_a_warning_but_still_publishes() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        let created = vault
            .replace_page_with_post_publish_ops(
                "https://example.com/article",
                "saved content",
                |_| Err(io::Error::other("simulated directory sync failure")),
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(root.path().join(&created.relative_path)).unwrap(),
            "saved content"
        );
        assert!(matches!(
            created.warnings.as_slice(),
            [PostPublishWarning::DirectorySync(_)]
        ));
    }

    #[test]
    fn replace_page_overwrites_whatever_occupies_the_generated_name() {
        // vault::replace_page trusts its caller. The note module (§4.4) is
        // responsible for checking the caller's version and ownership of an
        // existing file before calling this; this low-level test documents
        // that this layer itself does not refuse an unrelated occupant.
        let root = tempfile::tempdir().unwrap();
        let pages = root.path().join("pages");
        fs::create_dir(&pages).unwrap();
        let name = page_filename("https://example.com/user-note").unwrap();
        let user_file = pages.join(name);
        fs::write(&user_file, "user content").unwrap();

        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        vault
            .replace_page("https://example.com/user-note", "generated")
            .unwrap();
        assert_eq!(fs::read_to_string(user_file).unwrap(), "generated");
    }

    pub(crate) fn age_file(path: &Path, age: Duration) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    #[test]
    fn startup_sweep_removes_only_stale_page_note_temporaries() {
        let root = tempfile::tempdir().unwrap();
        let pages = root.path().join("pages");
        fs::create_dir(&pages).unwrap();
        let old = STALE_TEMPORARY_AGE + Duration::from_secs(60);
        let stale = pages.join(format!(".{NAMESPACE}-{}.tmp", Uuid::new_v4()));
        let fresh = pages.join(format!(".{NAMESPACE}-{}.tmp", Uuid::new_v4()));
        // Other writers' temporaries and user files are never touched.
        let log_temp = pages.join(format!(".{NAMESPACE}-log-{}.tmp", Uuid::new_v4()));
        let user_file = pages.join(format!(".{NAMESPACE}-notes.tmp"));
        let note = pages.join("note.md");
        for path in [&stale, &fresh, &log_temp, &user_file, &note] {
            fs::write(path, "content").unwrap();
        }
        for path in [&stale, &log_temp, &user_file, &note] {
            age_file(path, old);
        }

        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        assert_eq!(vault.sweep_stale_temporaries().unwrap(), 1);
        assert!(!stale.exists());
        for path in [&fresh, &log_temp, &user_file, &note] {
            assert!(path.exists(), "{} was removed", path.display());
        }
    }

    #[test]
    fn startup_sweep_tolerates_a_missing_pages_directory() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        assert_eq!(vault.sweep_stale_temporaries().unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn startup_sweep_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        fs::write(&target, "outside").unwrap();
        let pages = root.path().join("pages");
        fs::create_dir(&pages).unwrap();
        let link = pages.join(format!(".{NAMESPACE}-{}.tmp", Uuid::new_v4()));
        symlink(&target, &link).unwrap();
        age_file(&target, STALE_TEMPORARY_AGE + Duration::from_secs(60));

        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        assert_eq!(vault.sweep_stale_temporaries().unwrap(), 0);
        assert!(link.symlink_metadata().is_ok());
        assert_eq!(fs::read_to_string(target).unwrap(), "outside");
    }

    #[cfg(unix)]
    #[test]
    fn configured_symlink_cannot_escape_the_vault() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("pages")).unwrap();
        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        assert!(
            vault
                .replace_page("https://example.com", "content")
                .is_err()
        );
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
