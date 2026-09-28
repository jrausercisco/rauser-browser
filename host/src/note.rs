//! Versioned whole-file page notes (§4.4, §5.3). The side panel is the only
//! editor: a note is created on the first save and replaced whole on every
//! later save. Each save names the version it replaces; a stale version is
//! refused and the note's current content is returned so nothing the user
//! typed is silently lost.

use brauser_protocol::NoteSaveOutcome;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::brand::FRONTMATTER_KEY;
use crate::vault::Vault;

const MAX_TITLE_BYTES: usize = 2048;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MISSING_REVISION: &str = "missing";

pub struct LoadedNote {
    pub exists: bool,
    pub revision: String,
    pub title: String,
    pub body: String,
}

pub enum SaveOutcome {
    Saved {
        outcome: NoteSaveOutcome,
        revision: String,
        relative_path: String,
    },
    /// `expected_revision` did not match the note's current version. Nothing
    /// was written; this carries the note's current content so the caller
    /// can show it and let the user copy back what it could not save.
    Stale {
        revision: String,
        exists: bool,
        title: String,
        body: String,
    },
}

/// An existing file occupies the note's generated name but is not a Brauser
/// page note for this exact URL (wrong identity, or `brauser.kind` is not
/// absent/`page`, e.g. a summary file). Never adopted or overwritten (§6.4).
#[derive(Debug)]
pub struct OwnershipConflict(pub String);

#[derive(Debug)]
pub enum NoteRequestError {
    Invalid(String),
    Conflict(OwnershipConflict),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for NoteRequestError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

struct ParsedNote {
    title: String,
    created: String,
    body: String,
}

pub fn load_note(vault: &Vault, canonical_url: &str) -> Result<LoadedNote, NoteRequestError> {
    match vault.read_page(canonical_url)? {
        None => Ok(LoadedNote {
            exists: false,
            revision: MISSING_REVISION.to_owned(),
            title: String::new(),
            body: String::new(),
        }),
        Some(contents) => {
            let parsed = parse_owned_note(canonical_url, &contents).ok_or_else(not_owned)?;
            Ok(LoadedNote {
                exists: true,
                revision: revision_for(&contents),
                title: parsed.title,
                body: parsed.body,
            })
        }
    }
}

pub fn save_note(
    vault: &Vault,
    canonical_url: &str,
    title: &str,
    body: &str,
    expected_revision: &str,
) -> Result<SaveOutcome, NoteRequestError> {
    if title.len() > MAX_TITLE_BYTES || body.len() > MAX_BODY_BYTES {
        return Err(NoteRequestError::Invalid(
            "note title or body exceeds its size limit".into(),
        ));
    }
    let existing = vault.read_page(canonical_url)?;
    let (current_revision, parsed) = match &existing {
        None => (MISSING_REVISION.to_owned(), None),
        Some(contents) => {
            let parsed = parse_owned_note(canonical_url, contents).ok_or_else(not_owned)?;
            (revision_for(contents), Some(parsed))
        }
    };
    if expected_revision != current_revision {
        return Ok(SaveOutcome::Stale {
            revision: current_revision,
            exists: parsed.is_some(),
            title: parsed
                .as_ref()
                .map(|note| note.title.clone())
                .unwrap_or_default(),
            body: parsed
                .as_ref()
                .map(|note| note.body.clone())
                .unwrap_or_default(),
        });
    }

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| NoteRequestError::Internal(error.into()))?;
    let created = parsed
        .as_ref()
        .map(|note| note.created.clone())
        .unwrap_or_else(|| now.clone());
    let effective_title = parsed
        .as_ref()
        .map(|note| note.title.clone())
        .unwrap_or_else(|| normalized_title(canonical_url, title).to_owned());
    let markdown = render_note(canonical_url, &effective_title, body, &created, &now)?;
    let replacing = existing.is_some();
    let published = vault.replace_page(canonical_url, &markdown)?;
    let outcome = match (replacing, published.warnings.is_empty()) {
        (false, true) => NoteSaveOutcome::Created,
        (false, false) => NoteSaveOutcome::CreatedWithWarning,
        (true, true) => NoteSaveOutcome::Replaced,
        (true, false) => NoteSaveOutcome::ReplacedWithWarning,
    };
    Ok(SaveOutcome::Saved {
        outcome,
        revision: revision_for(&markdown),
        relative_path: published.relative_path.to_string_lossy().into_owned(),
    })
}

fn not_owned() -> NoteRequestError {
    NoteRequestError::Conflict(OwnershipConflict(
        "existing file at this page's note path is not a Brauser page note for this page".into(),
    ))
}

fn normalized_title<'a>(canonical_url: &'a str, title: &'a str) -> &'a str {
    if title.trim().is_empty() {
        canonical_url
    } else {
        title.trim()
    }
}

fn normalized_body(body: &str) -> &str {
    body.trim_end_matches('\n')
}

fn revision_for(contents: &str) -> String {
    format!(
        "sha256:{}",
        hex::encode(Sha256::digest(contents.as_bytes()))
    )
}

fn render_note(
    canonical_url: &str,
    title: &str,
    body: &str,
    created: &str,
    updated: &str,
) -> Result<String, NoteRequestError> {
    let title_json =
        serde_json::to_string(title).map_err(|error| NoteRequestError::Internal(error.into()))?;
    let url_json = serde_json::to_string(canonical_url)
        .map_err(|error| NoteRequestError::Internal(error.into()))?;
    let digest = hex::encode(Sha256::digest(canonical_url.as_bytes()));
    let digest_json =
        serde_json::to_string(&digest).map_err(|error| NoteRequestError::Internal(error.into()))?;
    let body = normalized_body(body);
    Ok(format!(
        "---\ntitle: {title_json}\nurl: {url_json}\n{FRONTMATTER_KEY}\n  canonical_url: {url_json}\n  url_id: {digest_json}\n  created: {created}\n  updated: {updated}\n---\n\n{body}\n"
    ))
}

/// Parse a note this host previously wrote, or refuse anything it does not
/// recognize as its own. The side panel is the only editor, so this need not
/// tolerate arbitrary hand edits; it must never adopt an unrelated file.
fn parse_owned_note(canonical_url: &str, contents: &str) -> Option<ParsedNote> {
    let (frontmatter, rest) = contents.strip_prefix("---\n")?.split_once("\n---\n")?;
    let body = rest.strip_prefix('\n').unwrap_or(rest);
    // render_note always appends exactly one trailing newline after the body.
    let body = body.strip_suffix('\n').unwrap_or(body);

    let mut title = None;
    let mut fm_canonical: Option<String> = None;
    let mut fm_url_id: Option<String> = None;
    let mut created: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut in_block = false;
    for line in frontmatter.lines() {
        if line == FRONTMATTER_KEY {
            in_block = true;
            continue;
        }
        if in_block {
            if !line.starts_with("  ") {
                in_block = false;
            } else if let Some(value) = line.strip_prefix("  canonical_url: ") {
                fm_canonical = unquote(value);
            } else if let Some(value) = line.strip_prefix("  url_id: ") {
                fm_url_id = unquote(value);
            } else if let Some(value) = line.strip_prefix("  created: ") {
                // Timestamps are written as plain RFC 3339 scalars (§6.5), not
                // JSON-quoted like the string fields above.
                created = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("  kind: ") {
                kind = Some(value.to_owned());
            }
        }
        if let Some(value) = line.strip_prefix("title: ") {
            title = unquote(value);
        }
    }

    let title = title?;
    let fm_canonical = fm_canonical?;
    let fm_url_id = fm_url_id?;
    let created = created?;
    // §6.4: a page note has no kind key, or an explicit "page" kind. Anything
    // else (such as "summary") must never be treated as an owned page note.
    if kind.is_some_and(|value| value != "page") {
        return None;
    }
    let expected_digest = hex::encode(Sha256::digest(canonical_url.as_bytes()));
    if fm_canonical != canonical_url || fm_url_id != expected_digest {
        return None;
    }
    Some(ParsedNote {
        title,
        created,
        body: body.to_owned(),
    })
}

fn unquote(value: &str) -> Option<String> {
    serde_json::from_str(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use brauser_protocol::StorageConfig;

    fn vault() -> (tempfile::TempDir, Vault) {
        let root = tempfile::tempdir().unwrap();
        let storage = StorageConfig {
            root: root.path().to_string_lossy().into_owned(),
            profile: "neutral".into(),
            log_dir: "log".into(),
            pages_dir: "pages".into(),
            later_dir: "later".into(),
        };
        let vault = Vault::open(&storage).unwrap();
        (root, vault)
    }

    #[test]
    fn loading_an_absent_note_reports_missing_revision() {
        let (_root, vault) = vault();
        let loaded = load_note(&vault, "https://example.com/a").unwrap();
        assert!(!loaded.exists);
        assert_eq!(loaded.revision, "missing");
        assert_eq!(loaded.title, "");
        assert_eq!(loaded.body, "");
    }

    #[test]
    fn saving_a_new_note_creates_it_from_the_missing_revision() {
        let (_root, vault) = vault();
        let SaveOutcome::Saved {
            outcome, revision, ..
        } = save_note(
            &vault,
            "https://example.com/a",
            "A Title",
            "hello",
            "missing",
        )
        .unwrap()
        else {
            panic!("expected a save, not a conflict");
        };
        assert!(matches!(outcome, NoteSaveOutcome::Created));
        let loaded = load_note(&vault, "https://example.com/a").unwrap();
        assert!(loaded.exists);
        assert_eq!(loaded.revision, revision);
        assert_eq!(loaded.title, "A Title");
        assert_eq!(loaded.body, "hello");
    }

    #[test]
    fn saving_with_a_stale_revision_is_refused_and_returns_current_content() {
        let (_root, vault) = vault();
        save_note(&vault, "https://example.com/a", "Title", "first", "missing").unwrap();

        let SaveOutcome::Stale {
            revision,
            exists,
            title,
            body,
        } = save_note(
            &vault,
            "https://example.com/a",
            "Title",
            "second",
            "missing",
        )
        .unwrap()
        else {
            panic!("expected a stale-revision conflict");
        };
        assert!(exists);
        assert_eq!(title, "Title");
        assert_eq!(body, "first");
        assert_ne!(revision, "missing");

        // The refused save must not have changed the file.
        let loaded = load_note(&vault, "https://example.com/a").unwrap();
        assert_eq!(loaded.body, "first");
    }

    #[test]
    fn resaving_with_the_current_revision_preserves_title_and_created_time() {
        let (_root, vault) = vault();
        let SaveOutcome::Saved { revision, .. } = save_note(
            &vault,
            "https://example.com/a",
            "Original Title",
            "one",
            "missing",
        )
        .unwrap() else {
            panic!("expected a save");
        };
        let SaveOutcome::Saved {
            outcome,
            revision: next_revision,
            ..
        } = save_note(
            &vault,
            "https://example.com/a",
            "Ignored New Title",
            "two",
            &revision,
        )
        .unwrap()
        else {
            panic!("expected a save");
        };
        assert!(matches!(outcome, NoteSaveOutcome::Replaced));
        assert_ne!(revision, next_revision);

        let loaded = load_note(&vault, "https://example.com/a").unwrap();
        assert_eq!(loaded.title, "Original Title");
        assert_eq!(loaded.body, "two");
    }

    #[test]
    fn a_file_owned_by_a_different_page_is_never_adopted() {
        let (root, vault) = vault();
        // Write a note for one URL, then hand-craft a filename collision by
        // writing unrelated content directly at the generated path.
        let path = vault.page_relative_path("https://example.com/a").unwrap();
        let full = root.path().join(&path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, "not a brauser note").unwrap();

        assert!(matches!(
            load_note(&vault, "https://example.com/a"),
            Err(NoteRequestError::Conflict(_))
        ));
        assert!(matches!(
            save_note(&vault, "https://example.com/a", "T", "b", "missing"),
            Err(NoteRequestError::Conflict(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&full).unwrap(),
            "not a brauser note"
        );
    }

    #[test]
    fn a_summary_file_at_the_same_name_is_a_conflict_not_a_note() {
        let (root, vault) = vault();
        let url = "https://example.com/a";
        let path = vault.page_relative_path(url).unwrap();
        let full = root.path().join(&path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        // Everything but `kind` matches a real note for this URL, so only the
        // kind guard can refuse it.
        let owned = |kind_line: &str| {
            format!(
                "---\ntitle: \"T\"\nurl: \"{url}\"\nbrauser:\n{kind_line}  canonical_url: \"{url}\"\n  url_id: \"{}\"\n  created: 2026-01-01T00:00:00Z\n---\n\nbody\n",
                hex::encode(Sha256::digest(url.as_bytes()))
            )
        };
        std::fs::write(&full, owned("")).unwrap();
        assert!(load_note(&vault, url).unwrap().exists);

        std::fs::write(&full, owned("  kind: summary\n")).unwrap();
        assert!(matches!(
            load_note(&vault, url),
            Err(NoteRequestError::Conflict(_))
        ));
    }

    #[test]
    fn title_only_matters_on_creation() {
        let (_root, vault) = vault();
        save_note(&vault, "https://example.com/a", "", "body", "missing").unwrap();
        let loaded = load_note(&vault, "https://example.com/a").unwrap();
        // An empty title at creation falls back to the canonical URL.
        assert_eq!(loaded.title, "https://example.com/a");
    }
}
