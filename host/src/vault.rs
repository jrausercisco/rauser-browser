//! Capability-scoped vault access. The extension never supplies a filesystem
//! path. M0 only creates new page files; later note edits must add managed
//! block ownership and conflict detection before replacing an existing file.

use std::fs;
use std::io::{self, Read, Write};
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

/// A page that was published under its final name. Warnings describe work that
/// failed after publication; callers must not retry the create as if it failed.
#[derive(Debug)]
pub struct CreatedPage {
    pub relative_path: PathBuf,
    pub warnings: Vec<PostPublishWarning>,
}

#[derive(Debug)]
pub enum PostPublishWarning {
    TemporaryFileRemoval {
        temporary_name: String,
        error: io::Error,
    },
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

    /// Atomically publish a new note, refusing to replace any existing file.
    /// The filename is derived from the URL, never from an extension path.
    pub fn create_page(&self, page_url: &str, markdown: &str) -> Result<CreatedPage> {
        self.create_page_with_post_publish_ops(
            page_url,
            markdown,
            |pages, name| pages.remove_file(name),
            sync_directory,
        )
    }

    /// Preserve a proposed change beside an existing note for manual review.
    /// The sibling name is stable for the normalized proposal, so retrying a
    /// lost acknowledgement cannot create an unbounded series of drafts.
    pub fn create_review_artifact(
        &self,
        page_url: &str,
        proposal_id: &str,
        markdown: &str,
    ) -> Result<CreatedPage> {
        let review_name = review_filename(page_url, proposal_id)?;
        self.create_named_with_post_publish_ops(
            &review_name,
            markdown,
            |pages, name| pages.remove_file(name),
            sync_directory,
        )
    }

    pub fn page_relative_path(&self, page_url: &str) -> Result<PathBuf> {
        Ok(self.pages_dir.join(page_filename(page_url)?))
    }

    pub fn review_relative_path(&self, page_url: &str, proposal_id: &str) -> Result<PathBuf> {
        Ok(self.pages_dir.join(review_filename(page_url, proposal_id)?))
    }

    /// Read a page generated from the same canonical URL. This is used only
    /// to distinguish an owned existing note from a filename conflict; M1
    /// never replaces or adopts the file.
    pub fn read_page(&self, page_url: &str) -> Result<Option<String>> {
        let filename = page_filename(page_url)?;
        self.read_named(&filename)
    }

    pub fn read_review_artifact(
        &self,
        page_url: &str,
        proposal_id: &str,
    ) -> Result<Option<String>> {
        let filename = review_filename(page_url, proposal_id)?;
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

    fn create_page_with_post_publish_ops<F, G>(
        &self,
        page_url: &str,
        markdown: &str,
        remove_temporary: F,
        sync_parent: G,
    ) -> Result<CreatedPage>
    where
        F: FnOnce(&Dir, &str) -> io::Result<()>,
        G: FnOnce(&Dir) -> io::Result<()>,
    {
        let filename = page_filename(page_url)?;
        self.create_named_with_post_publish_ops(&filename, markdown, remove_temporary, sync_parent)
    }

    fn create_named_with_post_publish_ops<F, G>(
        &self,
        filename: &str,
        markdown: &str,
        remove_temporary: F,
        sync_parent: G,
    ) -> Result<CreatedPage>
    where
        F: FnOnce(&Dir, &str) -> io::Result<()>,
        G: FnOnce(&Dir) -> io::Result<()>,
    {
        if markdown.len() > MAX_NOTE_BYTES {
            bail!("page note exceeds the 4 MiB write limit");
        }
        self.root
            .create_dir_all(&self.pages_dir)
            .context("creating page notes directory")?;
        sync_directory_chain(&self.root, &self.pages_dir)
            .context("syncing page notes parent directories")?;
        let pages = self
            .root
            .open_dir(&self.pages_dir)
            .context("opening page notes directory")?;

        let temporary = format!(".rauser-{}.tmp", Uuid::new_v4());
        let mut file = pages
            .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
            .context("creating page note temporary file")?;
        let mut cleanup = TempFileCleanup {
            dir: &pages,
            name: &temporary,
            armed: true,
        };
        let write_result = file
            .write_all(markdown.as_bytes())
            .and_then(|_| file.sync_all());
        drop(file);
        write_result?;

        // A hard link is an atomic no-clobber publication on the same volume.
        // rename() would silently replace a user file with this name.
        pages
            .hard_link(&temporary, &pages, filename)
            .with_context(|| {
                format!("page note already exists or cannot be created: {filename}")
            })?;

        let removal = remove_temporary(&pages, &temporary);
        cleanup.disarm();
        drop(cleanup);
        let directory_sync = sync_parent(&pages);
        let mut warnings = Vec::new();
        if let Err(error) = removal {
            warnings.push(PostPublishWarning::TemporaryFileRemoval {
                temporary_name: temporary,
                error,
            });
        }
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
        use std::os::windows::fs::MetadataExt;
        let volume = metadata
            .volume_serial_number()
            .context("Windows did not return a notes-folder volume ID")?;
        let file = metadata
            .file_index()
            .context("Windows did not return a notes-folder file ID")?;
        Ok(format!("windows:{volume:08x}:{file:016x}"))
    }
}

fn review_filename(page_url: &str, proposal_id: &str) -> Result<String> {
    if proposal_id.len() != 64
        || !proposal_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("review proposal ID must be a lowercase SHA-256 digest");
    }
    let filename = page_filename(page_url)?;
    let stem = filename
        .strip_suffix(".md")
        .context("page filename has no Markdown suffix")?;
    Ok(format!("{stem}.rauser-review-{proposal_id}.md"))
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
            let _ = self.dir.remove_file(self.name);
        }
    }
}

#[cfg(not(windows))]
fn sync_directory(dir: &Dir) -> io::Result<()> {
    dir.try_clone()?.into_std_file().sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_dir: &Dir) -> io::Result<()> {
    // std does not expose a portable way to fsync a Windows directory handle.
    Ok(())
}

fn sync_directory_chain(root: &Dir, relative: &Path) -> io::Result<()> {
    let mut current = root.try_clone()?;
    sync_directory(&current)?;
    for part in relative.components() {
        current = current.open_dir(part.as_os_str())?;
        sync_directory(&current)?;
    }
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
        assert!(relative.warnings.is_empty());
        assert!(
            vault
                .create_page("https://example.com/article#part-2", "second")
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(root.path().join(relative.relative_path)).unwrap(),
            "first"
        );
    }

    #[test]
    fn post_publish_failures_report_created_page_with_warnings() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::open(&storage(root.path(), "pages")).unwrap();
        let created = vault
            .create_page_with_post_publish_ops(
                "https://example.com/article",
                "saved content",
                |_, _| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                |_| Err(io::Error::other("simulated directory sync failure")),
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(root.path().join(&created.relative_path)).unwrap(),
            "saved content"
        );
        assert!(matches!(
            created.warnings.as_slice(),
            [
                PostPublishWarning::TemporaryFileRemoval { .. },
                PostPublishWarning::DirectorySync(_)
            ]
        ));
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
