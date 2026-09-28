//! Host-side visit policy and durable daily Markdown logging.
//!
//! Authorization uses the original URL. Only then may URL normalization drop
//! fragments or configured tracking parameters. A stable lock outside the
//! chosen notes folder covers the event-ID check, append, and file sync.

use std::fs::{self, OpenOptions as StdOpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use cap_std::fs::{Dir, OpenOptions};
use directories::ProjectDirs;
use rauser_protocol::{SiteConfig, StorageConfig, VisitEvent};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;
use uuid::Uuid;

use crate::vault::open_selected_root;

const LOG_HEADER: &str = "<!-- rauser:daily-log v1 -->";
const MARKER_PREFIX: &str = "<!-- rauser:visit id=";
const MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;
const MAX_URL_BYTES: usize = 8192;
const MAX_TITLE_BYTES: usize = 2048;
const MAX_STRIP_RULES: usize = 64;
const MAX_NEAR_REPEAT_SECS: u64 = 24 * 60 * 60;
const MAX_FUTURE_SECS: i64 = 5 * 60;
const MAX_PAST_SECS: i64 = 30 * 24 * 60 * 60;

pub struct CaptureStore {
    root: Dir,
    root_path: PathBuf,
    root_identity: String,
    log_dir: PathBuf,
    lock_path: PathBuf,
    journal_dir: PathBuf,
}

/// A durable intent is published before touching Markdown. On replay, only a
/// complete marker in the original log is terminal; an intent alone is never
/// treated as evidence that the visit was saved.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VisitIntent {
    version: u8,
    root_path: String,
    root_identity: String,
    relative_log: String,
    entry_hash: String,
    offset: u64,
}

pub struct CapturePolicy<'a> {
    pub enabled: bool,
    pub sites: &'a [SiteConfig],
    pub strip_params: &'a [String],
    pub near_repeat_secs: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CaptureOutcome {
    Persisted { relative_path: PathBuf },
    Suppressed { reason: String },
    Rejected { reason: String },
    Retryable { message: String },
}

impl CaptureStore {
    pub fn open(storage: &StorageConfig) -> Result<Self> {
        Self::open_checked(storage, None)
    }

    pub fn open_checked(storage: &StorageConfig, expected_identity: Option<&str>) -> Result<Self> {
        let project = ProjectDirs::from("", "", "Rauser")
            .context("cannot locate this user's application config directory")?;
        Self::open_at(storage, expected_identity, project.config_dir())
    }

    fn open_at(
        storage: &StorageConfig,
        expected_identity: Option<&str>,
        state_dir: &Path,
    ) -> Result<Self> {
        let chosen = Path::new(&storage.root);
        let log_dir = checked_relative_dir(&storage.log_dir)?;
        let (root, root_identity) = open_selected_root(chosen, expected_identity)?;
        // All capture processes for this user share one lock. Paths that name
        // the same vault entry can differ in case on macOS and Windows, so a
        // lock derived from configured path text would not serialize them.
        let lock_path = state_dir.join("locks").join("capture.lock");
        let journal_dir = state_dir.join("visit-ids");
        Ok(Self {
            root,
            root_path: chosen.to_path_buf(),
            root_identity,
            log_dir,
            lock_path,
            journal_dir,
        })
    }

    pub fn record(&self, policy: CapturePolicy<'_>, event: &VisitEvent) -> CaptureOutcome {
        if !policy.enabled {
            return CaptureOutcome::Suppressed {
                reason: "capture_disabled".into(),
            };
        }
        if event.incognito {
            return CaptureOutcome::Suppressed {
                reason: "incognito".into(),
            };
        }
        let event_id = match parse_event_id(&event.event_id) {
            Ok(value) => value,
            Err(reason) => return CaptureOutcome::Rejected { reason },
        };
        let timestamp = match parse_event_time(&event.occurred_at) {
            Ok(value) => value,
            Err(reason) => return CaptureOutcome::Rejected { reason },
        };
        if let Err(reason) = check_event_window(timestamp) {
            return CaptureOutcome::Rejected { reason };
        }
        if event.url.len() > MAX_URL_BYTES {
            return CaptureOutcome::Rejected {
                reason: "url_too_long".into(),
            };
        }
        if event
            .title
            .as_ref()
            .is_some_and(|value| value.len() > MAX_TITLE_BYTES)
        {
            return CaptureOutcome::Rejected {
                reason: "title_too_long".into(),
            };
        }
        let original = match parse_page_url(&event.url) {
            Ok(value) => value,
            Err(reason) => return CaptureOutcome::Rejected { reason },
        };
        let allowed = match is_authorized(&original, policy.sites) {
            Ok(value) => value,
            Err(message) => return CaptureOutcome::Retryable { message },
        };
        if !allowed {
            return CaptureOutcome::Rejected {
                reason: "site_not_allowed".into(),
            };
        }
        let normalized = match normalize_url(original, policy.strip_params) {
            Ok(value) => value,
            Err(message) => return CaptureOutcome::Retryable { message },
        };
        if policy.near_repeat_secs > MAX_NEAR_REPEAT_SECS {
            return CaptureOutcome::Retryable {
                message: "near-repeat window exceeds 24 hours".into(),
            };
        }
        match self.record_authorized(
            &event_id,
            timestamp,
            &normalized,
            event,
            policy.near_repeat_secs,
        ) {
            Ok(outcome) => outcome,
            Err(error) => CaptureOutcome::Retryable {
                message: error.to_string(),
            },
        }
    }

    fn record_authorized(
        &self,
        event_id: &str,
        timestamp: OffsetDateTime,
        normalized_url: &str,
        event: &VisitEvent,
        near_repeat_secs: u64,
    ) -> Result<CaptureOutcome> {
        let lock_parent = self
            .lock_path
            .parent()
            .context("capture lock path has no parent")?;
        fs::create_dir_all(lock_parent).context("creating capture lock directory")?;
        let lock_file = StdOpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)
            .context("opening capture lock")?;
        lock_file.lock().context("locking capture log")?;

        let event_hash = sha256_hex(event_id.as_bytes());
        let url_hash = sha256_hex(normalized_url.as_bytes());
        let title = safe_title(event.title.as_deref());
        let entry = format!(
            "\n- {} — {} — <{}>\n{MARKER_PREFIX}{event_hash} url={url_hash} at={} -->\n",
            event.occurred_at, title, normalized_url, event.occurred_at
        );
        let journal_path = self.journal_path(&event_hash);
        if let Some(intent) = read_intent(&journal_path)? {
            return self.replay_intent(&intent, &event_hash, entry.as_bytes());
        }

        let (relative_dir, filename) = self.log_parts(timestamp);
        self.root
            .create_dir_all(&relative_dir)
            .context("creating daily log directory")?;
        sync_directory_chain(&self.root, &relative_dir)
            .context("syncing daily log parent directories")?;
        let day_dir = self
            .root
            .open_dir(&relative_dir)
            .context("opening daily log directory")?;
        let ownership = ensure_daily_log(&day_dir, &filename)?;
        if !ownership {
            return Ok(CaptureOutcome::Rejected {
                reason: "daily_log_name_conflict".into(),
            });
        }

        let metadata = day_dir
            .symlink_metadata(&filename)
            .context("reading daily log metadata")?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Ok(CaptureOutcome::Rejected {
                reason: "daily_log_name_conflict".into(),
            });
        }
        if metadata.len() > MAX_LOG_BYTES {
            bail!("daily log exceeds 32 MiB; refusing an unbounded scan");
        }
        // A previous attempt may have synced the entry but failed to sync a
        // newly published directory entry. Do this before terminal replay or
        // near-repeat acknowledgements.
        sync_directory(&day_dir).context("syncing daily log directory before acknowledgement")?;
        let mut file = day_dir
            .open_with(&filename, OpenOptions::new().read(true).append(true))
            .context("opening owned daily log")?;
        let scan = scan_log(
            &mut file,
            &event_hash,
            &url_hash,
            timestamp.unix_timestamp(),
            near_repeat_secs,
        )?;
        if scan.duplicate {
            // A previous attempt may have written this marker but failed its
            // file sync. Do not acknowledge replay until it is durable.
            file.sync_all()
                .context("syncing replayed daily log entry")?;
            let intent = self.intent(&relative_dir.join(&filename), &entry, metadata.len())?;
            publish_intent(&journal_path, &intent)?;
            return Ok(CaptureOutcome::Suppressed {
                reason: "duplicate_event".into(),
            });
        }
        if scan.near_repeat {
            file.sync_all()
                .context("syncing near-repeat source entry")?;
            return Ok(CaptureOutcome::Suppressed {
                reason: "near_repeat".into(),
            });
        }
        if near_repeat_secs > 0
            && self.adjacent_near_repeat(timestamp, &url_hash, near_repeat_secs)?
        {
            return Ok(CaptureOutcome::Suppressed {
                reason: "near_repeat".into(),
            });
        }

        if metadata.len().saturating_add(entry.len() as u64) > MAX_LOG_BYTES {
            return Ok(CaptureOutcome::Rejected {
                reason: "daily_log_full".into(),
            });
        }
        let intent = self.intent(&relative_dir.join(&filename), &entry, metadata.len())?;
        publish_intent(&journal_path, &intent)
            .context("recording visit intent before Markdown append")?;
        file.write_all(entry.as_bytes())
            .context("appending daily log entry")?;
        file.sync_all().context("syncing daily log")?;
        sync_directory(&day_dir).context("syncing daily log directory")?;
        Ok(CaptureOutcome::Persisted {
            relative_path: relative_dir.join(filename),
        })
    }

    fn journal_path(&self, event_hash: &str) -> PathBuf {
        self.journal_dir
            .join(&event_hash[..2])
            .join(format!("{event_hash}.json"))
    }

    fn intent(&self, relative_log: &Path, entry: &str, offset: u64) -> Result<VisitIntent> {
        Ok(VisitIntent {
            version: 1,
            root_path: self
                .root_path
                .to_str()
                .context("notes path is not UTF-8")?
                .to_owned(),
            root_identity: self.root_identity.clone(),
            relative_log: relative_log
                .to_str()
                .context("log path is not UTF-8")?
                .to_owned(),
            entry_hash: sha256_hex(entry.as_bytes()),
            offset,
        })
    }

    fn replay_intent(
        &self,
        intent: &VisitIntent,
        event_hash: &str,
        entry: &[u8],
    ) -> Result<CaptureOutcome> {
        if intent.version != 1 || intent.entry_hash.len() != 64 || intent.offset > MAX_LOG_BYTES {
            bail!("visit intent is invalid; manual repair is required");
        }
        let relative_log = checked_relative_dir(&intent.relative_log)?;
        let parent = relative_log
            .parent()
            .context("visit intent has no log directory")?;
        let filename = relative_log
            .file_name()
            .context("visit intent has no log filename")?;
        let (root, _) =
            open_selected_root(Path::new(&intent.root_path), Some(&intent.root_identity)).context(
                "original notes folder changed; choose it again or discard the queued visit",
            )?;
        let day_dir = root
            .open_dir(parent)
            .context("original daily log is unavailable; manual repair is required")?;
        let metadata = day_dir
            .symlink_metadata(filename)
            .context("original daily log is unavailable; manual repair is required")?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_LOG_BYTES
        {
            bail!("original daily log changed; manual repair is required");
        }
        if !existing_log_is_owned(
            &day_dir,
            filename.to_str().context("log filename is not UTF-8")?,
            metadata.len(),
        )? {
            bail!("original daily log is not owned by Rauser");
        }
        let mut file = day_dir
            .open_with(filename, OpenOptions::new().read(true).append(true))
            .context("opening original daily log")?;
        let scan = scan_log(&mut file, event_hash, "", 0, 0)?;
        if scan.duplicate {
            file.sync_all().context("syncing replayed daily log")?;
            sync_directory(&day_dir).context("syncing original daily log directory")?;
            return Ok(CaptureOutcome::Suppressed {
                reason: "duplicate_event".into(),
            });
        }
        if self.root_identity != intent.root_identity {
            bail!("visit belongs to a previously selected notes folder; do not append there");
        }
        if intent.entry_hash != sha256_hex(entry) {
            bail!("queued visit changed after a partial write; manual repair is required");
        }
        if metadata.len() < intent.offset || metadata.len() > intent.offset + entry.len() as u64 {
            bail!("original daily log changed after visit intent; manual repair is required");
        }
        let tail_len = (metadata.len() - intent.offset) as usize;
        file.seek(SeekFrom::Start(intent.offset))?;
        let mut tail = vec![0; tail_len];
        file.read_exact(&mut tail)?;
        if tail != entry[..tail_len] {
            bail!("original daily log was edited after visit intent; manual repair is required");
        }
        file.write_all(&entry[tail_len..])
            .context("completing interrupted daily log entry")?;
        file.sync_all()
            .context("syncing completed daily log entry")?;
        sync_directory(&day_dir).context("syncing original daily log directory")?;
        Ok(CaptureOutcome::Persisted {
            relative_path: relative_log,
        })
    }

    fn log_parts(&self, timestamp: OffsetDateTime) -> (PathBuf, String) {
        let year = timestamp.year();
        let month = timestamp.month() as u8;
        let day = timestamp.day();
        let relative_dir = self
            .log_dir
            .join(format!("{year:04}"))
            .join(format!("{month:02}"));
        (relative_dir, format!("{year:04}-{month:02}-{day:02}.md"))
    }

    fn adjacent_near_repeat(
        &self,
        timestamp: OffsetDateTime,
        url_hash: &str,
        near_repeat_secs: u64,
    ) -> Result<bool> {
        let seconds_today = u64::from(timestamp.hour()) * 3600
            + u64::from(timestamp.minute()) * 60
            + u64::from(timestamp.second());
        let mut adjacent = Vec::new();
        if seconds_today <= near_repeat_secs {
            adjacent.push(timestamp - time::Duration::days(1));
        }
        if 86_400 - seconds_today <= near_repeat_secs {
            adjacent.push(timestamp + time::Duration::days(1));
        }
        for day in adjacent {
            let (relative_dir, filename) = self.log_parts(day);
            let dir = match self.root.open_dir(&relative_dir) {
                Ok(dir) => dir,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("opening adjacent daily log"),
            };
            let metadata = match dir.symlink_metadata(&filename) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("checking adjacent daily log"),
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                continue;
            }
            if !existing_log_is_owned(&dir, &filename, metadata.len())? {
                continue;
            }
            sync_directory(&dir).context("syncing adjacent daily log before acknowledgement")?;
            let mut file = dir
                .open_with(&filename, OpenOptions::new().read(true).append(true))
                .context("reading adjacent daily log")?;
            let scan = scan_log(
                &mut file,
                "",
                url_hash,
                timestamp.unix_timestamp(),
                near_repeat_secs,
            )?;
            if scan.near_repeat {
                file.sync_all()
                    .context("syncing adjacent near-repeat source entry")?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn checked_relative_dir(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if value.is_empty()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        bail!("log directory must be a nonempty relative path");
    }
    Ok(path.to_path_buf())
}

fn parse_event_id(value: &str) -> std::result::Result<String, String> {
    let parsed = Uuid::parse_str(value).map_err(|_| "invalid_event_id".to_owned())?;
    Ok(parsed.hyphenated().to_string())
}

fn parse_event_time(value: &str) -> std::result::Result<OffsetDateTime, String> {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return Err("invalid_timestamp".into());
    }
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| "invalid_timestamp".into())
}

fn check_event_window(value: OffsetDateTime) -> std::result::Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "host_clock_unavailable".to_owned())?
        .as_secs() as i64;
    let at = value.unix_timestamp();
    if at > now + MAX_FUTURE_SECS {
        return Err("timestamp_in_future".into());
    }
    if at < now - MAX_PAST_SECS {
        return Err("timestamp_too_old".into());
    }
    Ok(())
}

fn parse_page_url(value: &str) -> std::result::Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "invalid_url".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("invalid_url_scheme".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("url_contains_credentials".into());
    }
    Ok(url)
}

/// Check the original URL against confirmed host rules. Call this before
/// normalization; dropping query parameters must never widen authorization.
pub fn url_allowed(raw: &str, sites: &[SiteConfig]) -> Result<bool> {
    let url = parse_page_url(raw).map_err(anyhow::Error::msg)?;
    is_authorized(&url, sites).map_err(anyhow::Error::msg)
}

/// Return a canonical page URL. Callers must authorize `raw` first.
pub fn canonical_url(raw: &str, strip_params: &[String]) -> Result<String> {
    let url = parse_page_url(raw).map_err(anyhow::Error::msg)?;
    normalize_url(url, strip_params).map_err(anyhow::Error::msg)
}

fn is_authorized(url: &Url, sites: &[SiteConfig]) -> std::result::Result<bool, String> {
    if sites.len() > 256 {
        return Err("site policy has too many rules".into());
    }
    for site in sites {
        let rule = Url::parse(&site.origin).map_err(|_| "invalid site origin".to_owned())?;
        if !matches!(rule.scheme(), "http" | "https")
            || rule.host_str().is_none()
            || !rule.username().is_empty()
            || rule.password().is_some()
            || rule.path() != "/"
            || rule.query().is_some()
            || rule.fragment().is_some()
            || site.origin.ends_with('/')
        {
            return Err("site policy must use an exact HTTP(S) origin".into());
        }
        let prefix = site.path_prefix.as_str();
        if !prefix.starts_with('/')
            || prefix.contains('?')
            || prefix.contains('#')
            || prefix.split('/').any(|part| matches!(part, "." | ".."))
        {
            return Err("invalid site path prefix".into());
        }
        let matches_path = if prefix == "/" {
            true
        } else if prefix.ends_with('/') {
            // A slash-terminated rule matches descendants only. In
            // particular, "//" must never collapse into a rule for all paths.
            url.path().starts_with(prefix)
        } else {
            url.path() == prefix
                || url
                    .path()
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with('/'))
        };
        if rule.origin() == url.origin() && matches_path {
            return Ok(true);
        }
    }
    Ok(false)
}

fn normalize_url(mut url: Url, strip_params: &[String]) -> std::result::Result<String, String> {
    if strip_params.len() > MAX_STRIP_RULES {
        return Err("too many URL parameter rules".into());
    }
    for pattern in strip_params {
        let core = pattern.strip_suffix('*').unwrap_or(pattern);
        if core.is_empty()
            || pattern.len() > 64
            || !core
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            || core.contains('*')
        {
            return Err("invalid URL parameter rule".into());
        }
    }
    url.set_fragment(None);
    if let Some(query) = url.query().map(str::to_owned) {
        // Inspect decoded keys, but retain every untouched raw query segment.
        // Rebuilding all pairs would change the encoding of meaningful values.
        let segments: Vec<_> = query.split('&').collect();
        let kept: Vec<_> = segments
            .iter()
            .copied()
            .filter(|segment| {
                let key = url::form_urlencoded::parse(segment.as_bytes())
                    .next()
                    .map(|(key, _)| key.into_owned())
                    .unwrap_or_default();
                !strip_params
                    .iter()
                    .any(|pattern| matches_param(&key, pattern))
            })
            .collect();
        if kept.len() != segments.len() {
            if kept.is_empty() {
                url.set_query(None);
            } else {
                url.set_query(Some(&kept.join("&")));
            }
        }
    }
    Ok(url.to_string())
}

fn matches_param(key: &str, pattern: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    match pattern.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None => key == pattern,
    }
}

fn sha256_hex(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

fn read_intent(path: &Path) -> Result<Option<VisitIntent>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("checking visit intent"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 8192 {
        bail!("visit intent is invalid; manual repair is required");
    }
    let bytes = fs::read(path).context("reading visit intent")?;
    if bytes.len() > 8192 {
        bail!("visit intent exceeds its size limit");
    }
    let intent: VisitIntent = serde_json::from_slice(&bytes)
        .context("visit intent is malformed; manual repair is required")?;
    Ok(Some(intent))
}

fn publish_intent(path: &Path, intent: &VisitIntent) -> Result<()> {
    let bytes = serde_json::to_vec(intent).context("serializing visit intent")?;
    if bytes.len() > 8192 {
        bail!("visit intent exceeds its size limit");
    }
    let parent = path.parent().context("visit intent has no parent")?;
    fs::create_dir_all(parent).context("creating visit intent directory")?;
    // Flush each newly created directory entry before the Markdown append.
    let journal_dir = parent
        .parent()
        .context("visit intent has no shard parent")?;
    sync_state_directory(
        journal_dir
            .parent()
            .context("visit intent has no state parent")?,
    )?;
    sync_state_directory(journal_dir)?;
    sync_state_directory(parent)?;
    let temporary = parent.join(format!(".rauser-intent-{}.tmp", Uuid::new_v4()));
    let mut options = StdOpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .context("creating visit intent temporary file")?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes).context("writing visit intent")?;
        file.sync_all().context("syncing visit intent")?;
        drop(file);
        fs::hard_link(&temporary, path).context("publishing visit intent")?;
        sync_state_directory(parent).context("syncing visit intent directory")?;
        Ok(())
    })();
    if let Err(error) = fs::remove_file(&temporary) {
        eprintln!("rauser: warning: could not remove visit intent temporary file: {error}");
    }
    result
}

#[cfg(not(windows))]
fn sync_state_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("opening state directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing state directory {}", path.display()))
}

#[cfg(windows)]
fn sync_state_directory(_path: &Path) -> Result<()> {
    // Windows directory-entry power-loss durability needs separate validation.
    Ok(())
}

fn ensure_daily_log(dir: &Dir, name: &str) -> Result<bool> {
    match dir.symlink_metadata(name) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Ok(false);
            }
            return existing_log_is_owned(dir, name, metadata.len());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("checking daily log"),
    }
    let temporary = format!(".rauser-log-{}.tmp", Uuid::new_v4());
    let mut file = dir
        .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
        .context("creating daily log temporary file")?;
    let mut cleanup = TempFileCleanup {
        dir,
        name: &temporary,
        armed: true,
    };
    file.write_all(format!("{LOG_HEADER}\n").as_bytes())?;
    file.sync_all()?;
    drop(file);
    match dir.hard_link(&temporary, dir, name) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = dir.symlink_metadata(name)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Ok(false);
            }
            return existing_log_is_owned(dir, name, metadata.len());
        }
        Err(error) => return Err(error).context("publishing daily log"),
    }
    // Once the final name exists, never let Drop remove the temp name again:
    // a different writer could reuse that name after the explicit removal.
    cleanup.armed = false;
    if let Err(error) = dir.remove_file(&temporary) {
        eprintln!("rauser: warning: could not remove daily log temporary file: {error}");
    }
    sync_directory(dir).context("syncing new daily log directory")?;
    Ok(true)
}

fn existing_log_is_owned(dir: &Dir, name: &str, length: u64) -> Result<bool> {
    if length > MAX_LOG_BYTES {
        bail!("daily log exceeds 32 MiB; refusing an unbounded scan");
    }
    let file = dir.open(name).context("opening existing daily log")?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first)?;
    Ok(first.trim_end_matches(['\r', '\n']) == LOG_HEADER)
}

struct TempFileCleanup<'a> {
    dir: &'a Dir,
    name: &'a str,
    armed: bool,
}

impl Drop for TempFileCleanup<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.dir.remove_file(self.name);
        }
    }
}

struct ScanResult {
    duplicate: bool,
    near_repeat: bool,
}

fn scan_log(
    file: &mut cap_std::fs::File,
    event_hash: &str,
    url_hash: &str,
    at: i64,
    near_repeat_secs: u64,
) -> Result<ScanResult> {
    let mut lines = BufReader::new(file).lines();
    if lines.next().transpose()?.as_deref() != Some(LOG_HEADER) {
        bail!("daily log exists without Rauser ownership marker");
    }
    let mut result = ScanResult {
        duplicate: false,
        near_repeat: false,
    };
    for line in lines {
        if let Some((id, url, previous_at)) = parse_marker(&line?) {
            if id == event_hash {
                result.duplicate = true;
            }
            if near_repeat_secs > 0
                && url == url_hash
                && at.abs_diff(previous_at) <= near_repeat_secs
            {
                result.near_repeat = true;
            }
        }
    }
    Ok(result)
}

fn parse_marker(line: &str) -> Option<(&str, &str, i64)> {
    let rest = line.strip_prefix(MARKER_PREFIX)?;
    let (id, rest) = rest.split_once(" url=")?;
    let (url, rest) = rest.split_once(" at=")?;
    let timestamp = rest.strip_suffix(" -->")?;
    if id.len() != 64 || url.len() != 64 {
        return None;
    }
    let at = parse_event_time(timestamp).ok()?.unix_timestamp();
    Some((id, url, at))
}

fn safe_title(value: Option<&str>) -> String {
    let title = value.unwrap_or("Untitled").trim();
    let mut out = String::new();
    for ch in title.chars().take(300) {
        if ch.is_control() {
            out.push(' ');
        } else {
            if matches!(ch, '\\' | '[' | ']' | '<' | '>' | '*' | '_' | '`' | '|') {
                out.push('\\');
            }
            out.push(ch);
        }
    }
    if out.trim().is_empty() {
        "Untitled".into()
    } else {
        out
    }
}

#[cfg(not(windows))]
fn sync_directory(dir: &Dir) -> io::Result<()> {
    dir.try_clone()?.into_std_file().sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_dir: &Dir) -> io::Result<()> {
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

    fn storage(root: &Path, log_dir: &str) -> StorageConfig {
        StorageConfig {
            root: root.to_string_lossy().into_owned(),
            profile: "neutral".into(),
            log_dir: log_dir.into(),
            pages_dir: "pages".into(),
            later_dir: "later".into(),
        }
    }

    fn event() -> VisitEvent {
        VisitEvent {
            event_id: Uuid::new_v4().to_string(),
            url: "https://example.com/article?keep=1".into(),
            title: Some("Article".into()),
            occurred_at: OffsetDateTime::now_utc()
                .replace_nanosecond(0)
                .unwrap()
                .format(&Rfc3339)
                .unwrap(),
            incognito: false,
        }
    }

    fn record(store: &CaptureStore, event: &VisitEvent) -> CaptureOutcome {
        let sites = [SiteConfig {
            origin: "https://example.com".into(),
            path_prefix: "/".into(),
        }];
        store.record(
            CapturePolicy {
                enabled: true,
                sites: &sites,
                strip_params: &[],
                near_repeat_secs: 0,
            },
            event,
        )
    }

    #[test]
    fn event_id_is_deduplicated_across_day_and_log_directory() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let first =
            CaptureStore::open_at(&storage(root.path(), "log"), None, state.path()).unwrap();
        let visit = event();
        assert!(matches!(
            record(&first, &visit),
            CaptureOutcome::Persisted { .. }
        ));
        let mut shifted = visit.clone();
        shifted.occurred_at = (OffsetDateTime::now_utc() - time::Duration::days(1))
            .replace_nanosecond(0)
            .unwrap()
            .format(&Rfc3339)
            .unwrap();
        let other =
            CaptureStore::open_at(&storage(root.path(), "other-log"), None, state.path()).unwrap();
        assert_eq!(
            record(&other, &shifted),
            CaptureOutcome::Suppressed {
                reason: "duplicate_event".into()
            }
        );
        assert!(!root.path().join("other-log").exists());
    }

    #[test]
    fn event_id_does_not_move_to_a_new_notes_root() {
        let old_root = tempfile::tempdir().unwrap();
        let new_root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let first =
            CaptureStore::open_at(&storage(old_root.path(), "log"), None, state.path()).unwrap();
        let visit = event();
        assert!(matches!(
            record(&first, &visit),
            CaptureOutcome::Persisted { .. }
        ));
        let second =
            CaptureStore::open_at(&storage(new_root.path(), "log"), None, state.path()).unwrap();
        assert_eq!(
            record(&second, &visit),
            CaptureOutcome::Suppressed {
                reason: "duplicate_event".into()
            }
        );
        assert!(!new_root.path().join("log").exists());
    }

    #[test]
    fn partial_visit_is_not_completed_in_a_deselected_root() {
        let old_root = tempfile::tempdir().unwrap();
        let new_root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let first =
            CaptureStore::open_at(&storage(old_root.path(), "log"), None, state.path()).unwrap();
        let visit = event();
        let CaptureOutcome::Persisted { relative_path } = record(&first, &visit) else {
            panic!("expected persisted visit");
        };
        let hash = sha256_hex(visit.event_id.as_bytes());
        let intent = read_intent(&first.journal_path(&hash)).unwrap().unwrap();
        let log = old_root.path().join(relative_path);
        fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .unwrap()
            .set_len(intent.offset + 8)
            .unwrap();
        let before = fs::read(&log).unwrap();
        let second =
            CaptureStore::open_at(&storage(new_root.path(), "log"), None, state.path()).unwrap();
        assert!(matches!(
            record(&second, &visit),
            CaptureOutcome::Retryable { .. }
        ));
        assert_eq!(fs::read(&log).unwrap(), before);
        assert!(!new_root.path().join("log").exists());
    }

    #[test]
    fn exact_partial_entry_is_completed_but_an_edited_tail_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let store =
            CaptureStore::open_at(&storage(root.path(), "log"), None, state.path()).unwrap();
        let visit = event();
        let CaptureOutcome::Persisted { relative_path } = record(&store, &visit) else {
            panic!("expected persisted visit");
        };
        let hash = sha256_hex(visit.event_id.as_bytes());
        let intent = read_intent(&store.journal_path(&hash)).unwrap().unwrap();
        let log = root.path().join(relative_path);
        let original = fs::read(&log).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .unwrap()
            .set_len(intent.offset + 8)
            .unwrap();
        assert!(matches!(
            record(&store, &visit),
            CaptureOutcome::Persisted { .. }
        ));
        assert_eq!(fs::read(&log).unwrap(), original);

        fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .unwrap()
            .set_len(intent.offset + 8)
            .unwrap();
        let mut file = fs::OpenOptions::new().write(true).open(&log).unwrap();
        file.seek(SeekFrom::Start(intent.offset)).unwrap();
        file.write_all(b"X").unwrap();
        drop(file);
        let before = fs::read(&log).unwrap();
        assert!(matches!(
            record(&store, &visit),
            CaptureOutcome::Retryable { .. }
        ));
        assert_eq!(fs::read(&log).unwrap(), before);
    }

    #[test]
    fn missing_original_log_is_retryable_and_not_recreated_elsewhere() {
        let old_root = tempfile::tempdir().unwrap();
        let new_root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let first =
            CaptureStore::open_at(&storage(old_root.path(), "log"), None, state.path()).unwrap();
        let visit = event();
        let CaptureOutcome::Persisted { relative_path } = record(&first, &visit) else {
            panic!("expected persisted visit");
        };
        fs::remove_file(old_root.path().join(relative_path)).unwrap();
        let second =
            CaptureStore::open_at(&storage(new_root.path(), "log"), None, state.path()).unwrap();
        assert!(matches!(
            record(&second, &visit),
            CaptureOutcome::Retryable { .. }
        ));
        assert!(!new_root.path().join("log").exists());
    }
}
