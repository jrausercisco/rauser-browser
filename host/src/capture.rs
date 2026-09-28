//! Host-side visit policy and durable daily Markdown logging.
//!
//! Authorization uses the original URL. Only then may URL normalization drop
//! fragments or configured tracking parameters. A stable lock outside the
//! chosen notes folder covers the event-ID check, append, and file sync.

use std::fs::{self, OpenOptions as StdOpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use directories::ProjectDirs;
use rauser_protocol::{SiteConfig, StorageConfig, VisitEvent};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;
use uuid::Uuid;

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
    log_dir: PathBuf,
    lock_path: PathBuf,
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
        let chosen = Path::new(&storage.root);
        if !chosen.is_absolute() {
            bail!("notes folder must be absolute");
        }
        let canonical = fs::canonicalize(chosen).context("resolving notes folder")?;
        if !canonical.is_dir() {
            bail!("notes folder is not a directory");
        }
        let log_dir = checked_relative_dir(&storage.log_dir)?;
        let root = Dir::open_ambient_dir(&canonical, ambient_authority())
            .context("opening chosen notes folder")?;

        let project = ProjectDirs::from("", "", "Rauser")
            .context("cannot locate this user's application config directory")?;
        // All capture processes for this user share one lock. Paths that name
        // the same vault entry can differ in case on macOS and Windows, so a
        // lock derived from configured path text would not serialize them.
        let lock_path = project.config_dir().join("locks").join("capture.lock");
        Ok(Self {
            root,
            log_dir,
            lock_path,
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
        let event_hash = sha256_hex(event_id.as_bytes());
        let url_hash = sha256_hex(normalized_url.as_bytes());
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

        let title = safe_title(event.title.as_deref());
        let entry = format!(
            "\n- {} — {} — <{}>\n{MARKER_PREFIX}{event_hash} url={url_hash} at={} -->\n",
            event.occurred_at, title, normalized_url, event.occurred_at
        );
        if metadata.len().saturating_add(entry.len() as u64) > MAX_LOG_BYTES {
            return Ok(CaptureOutcome::Rejected {
                reason: "daily_log_full".into(),
            });
        }
        file.write_all(entry.as_bytes())
            .context("appending daily log entry")?;
        file.sync_all().context("syncing daily log")?;
        sync_directory(&day_dir).context("syncing daily log directory")?;
        Ok(CaptureOutcome::Persisted {
            relative_path: relative_dir.join(filename),
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
