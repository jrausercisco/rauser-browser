//! Create-only page notes. Existing files are read for identity, never edited.

use anyhow::{Result, bail};
use rauser_protocol::PageNoteOutcome;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::vault::Vault;

const MAX_TITLE_BYTES: usize = 2048;
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub struct PageNoteCreation {
    pub outcome: PageNoteOutcome,
    pub relative_path: Option<String>,
    pub message: Option<String>,
}

pub fn create_page_note(
    vault: &Vault,
    canonical_url: &str,
    title: &str,
    body: &str,
) -> Result<PageNoteCreation> {
    if title.len() > MAX_TITLE_BYTES || body.len() > MAX_BODY_BYTES {
        bail!("page title or note body exceeds its size limit");
    }
    if body.contains("<!-- rauser:start -->") || body.contains("<!-- rauser:end -->") {
        bail!("note body contains a reserved Rauser marker");
    }

    if let Some(existing) = vault.read_page(canonical_url)? {
        return handle_existing(vault, canonical_url, title, body, &existing);
    }

    let markdown = render_page_note(canonical_url, title, body, None)?;
    match vault.create_page(canonical_url, &markdown) {
        Ok(created) => {
            let message = if created.warnings.is_empty() {
                None
            } else {
                Some(format!(
                    "page was created, but {} post-publication operation(s) warned",
                    created.warnings.len()
                ))
            };
            let outcome = if message.is_some() {
                PageNoteOutcome::CreatedWithWarning
            } else {
                PageNoteOutcome::Created
            };
            Ok(PageNoteCreation {
                outcome,
                relative_path: Some(created.relative_path.to_string_lossy().into_owned()),
                message,
            })
        }
        Err(error) => {
            // A second host may have won the no-clobber publication race.
            // Only report already-present after reading its owned identity.
            if let Some(existing) = vault.read_page(canonical_url)? {
                handle_existing(vault, canonical_url, title, body, &existing)
            } else {
                Err(error)
            }
        }
    }
}

fn handle_existing(
    vault: &Vault,
    canonical_url: &str,
    title: &str,
    body: &str,
    existing: &str,
) -> Result<PageNoteCreation> {
    if matches_proposal(canonical_url, title, body, existing, None)? {
        return Ok(PageNoteCreation {
            outcome: PageNoteOutcome::AlreadyPresent,
            relative_path: vault
                .page_relative_path(canonical_url)?
                .to_str()
                .map(str::to_owned),
            message: None,
        });
    }

    // An existing page note is never changed in M1. Keep every proposed title
    // or body in a separate, create-only file so a user can review it safely.
    let original_path = vault.page_relative_path(canonical_url)?;
    let original_path = original_path.to_string_lossy();
    let proposal_id = proposal_id(canonical_url, title, body);
    if let Some(review) = vault.read_review_artifact(canonical_url, &proposal_id)? {
        return existing_review(
            vault,
            canonical_url,
            title,
            body,
            &original_path,
            &proposal_id,
            &review,
        );
    }
    let markdown = render_page_note(
        canonical_url,
        title,
        body,
        Some((&original_path, &proposal_id)),
    )?;
    let review = match vault.create_review_artifact(canonical_url, &proposal_id, &markdown) {
        Ok(review) => review,
        Err(error) => {
            // Another host may have published the same proposal meanwhile.
            if let Some(review) = vault.read_review_artifact(canonical_url, &proposal_id)? {
                return existing_review(
                    vault,
                    canonical_url,
                    title,
                    body,
                    &original_path,
                    &proposal_id,
                    &review,
                );
            }
            return Err(error);
        }
    };
    let warning = if review.warnings.is_empty() {
        String::new()
    } else {
        format!(" ({} publication warning(s))", review.warnings.len())
    };
    Ok(PageNoteCreation {
        outcome: PageNoteOutcome::Conflict,
        relative_path: Some(review.relative_path.to_string_lossy().into_owned()),
        message: Some(format!(
            "Existing page note was left unchanged; review the sibling draft{warning}"
        )),
    })
}

fn existing_review(
    vault: &Vault,
    canonical_url: &str,
    title: &str,
    body: &str,
    original_path: &str,
    proposal_id: &str,
    existing: &str,
) -> Result<PageNoteCreation> {
    if !matches_proposal(
        canonical_url,
        title,
        body,
        existing,
        Some((original_path, proposal_id)),
    )? {
        bail!("review artifact name is occupied by different content");
    }
    Ok(PageNoteCreation {
        outcome: PageNoteOutcome::Conflict,
        relative_path: vault
            .review_relative_path(canonical_url, proposal_id)?
            .to_str()
            .map(str::to_owned),
        message: Some("Existing page note was left unchanged; review the sibling draft".into()),
    })
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

fn proposal_id(canonical_url: &str, title: &str, body: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rauser-page-note-proposal-v1\0");
    for value in [
        canonical_url,
        normalized_title(canonical_url, title),
        normalized_body(body),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn matches_proposal(
    canonical_url: &str,
    title: &str,
    body: &str,
    contents: &str,
    review: Option<(&str, &str)>,
) -> Result<bool> {
    if !matches_owned_identity(canonical_url, contents) {
        return Ok(false);
    }
    let Some((frontmatter, rendered_body)) = contents
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
    else {
        return Ok(false);
    };
    let title_line = format!(
        "title: {}",
        serde_json::to_string(normalized_title(canonical_url, title))?
    );
    let exactly_one =
        |expected: &str| frontmatter.lines().filter(|line| *line == expected).count() == 1;
    let key_count = |prefix: &str| {
        frontmatter
            .lines()
            .filter(|line| line.starts_with(prefix))
            .count()
    };
    if !exactly_one(&title_line) || key_count("title:") != 1 {
        return Ok(false);
    }
    if let Some((original_path, proposal_id)) = review {
        let review_line = format!("  review_of: {}", serde_json::to_string(original_path)?);
        let proposal_line = format!("  proposal_id: {}", serde_json::to_string(proposal_id)?);
        if !exactly_one(&review_line)
            || !exactly_one(&proposal_line)
            || key_count("  review_of:") != 1
            || key_count("  proposal_id:") != 1
        {
            return Ok(false);
        }
    } else if key_count("  review_of:") != 0 || key_count("  proposal_id:") != 0 {
        return Ok(false);
    }
    let Some((_, existing_body)) = rendered_body.split_once("<!-- rauser:end -->\n\n## My notes\n")
    else {
        return Ok(false);
    };
    Ok(existing_body == format!("{}\n", normalized_body(body)))
}

fn render_page_note(
    canonical_url: &str,
    title: &str,
    body: &str,
    review: Option<(&str, &str)>,
) -> Result<String> {
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let digest = hex::encode(Sha256::digest(canonical_url.as_bytes()));
    let title = serde_json::to_string(normalized_title(canonical_url, title))?;
    let url = serde_json::to_string(canonical_url)?;
    let digest = serde_json::to_string(&digest)?;
    let review_keys = review
        .map(|(path, proposal_id)| -> Result<String> {
            Ok(format!(
                "  review_of: {}\n  proposal_id: {}\n",
                serde_json::to_string(path)?,
                serde_json::to_string(proposal_id)?,
            ))
        })
        .transpose()?
        .unwrap_or_default();
    let review_notice = if review.is_some() {
        "> Review draft: the existing page note was not changed. Copy any desired content into it manually.\n\n"
    } else {
        ""
    };
    let body = normalized_body(body);
    Ok(format!(
        "---\ntitle: {title}\nurl: {url}\nrauser:\n  canonical_url: {url}\n  url_id: {digest}\n{review_keys}  created: {now}\n---\n\n{review_notice}<!-- rauser:start -->\n<!-- rauser:end -->\n\n## My notes\n{body}\n"
    ))
}

fn matches_owned_identity(canonical_url: &str, contents: &str) -> bool {
    let Some((frontmatter, body)) = contents
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
    else {
        return false;
    };
    let Ok(url) = serde_json::to_string(canonical_url) else {
        return false;
    };
    let Ok(digest) = serde_json::to_string(&hex::encode(Sha256::digest(canonical_url.as_bytes())))
    else {
        return false;
    };
    let frontmatter_lines: Vec<_> = frontmatter.lines().collect();
    let exactly_one = |expected: &str| {
        frontmatter_lines
            .iter()
            .filter(|line| **line == expected)
            .count()
            == 1
    };
    let key_count = |prefix: &str| {
        frontmatter_lines
            .iter()
            .filter(|line| line.starts_with(prefix))
            .count()
    };
    let marker_lines: Vec<_> = body.lines().collect();
    let marker_index = |expected: &str| {
        let mut positions = marker_lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| (*line == expected).then_some(index));
        let first = positions.next()?;
        positions.next().is_none().then_some(first)
    };
    exactly_one("rauser:")
        && key_count("rauser:") == 1
        && exactly_one(&format!("url: {url}"))
        && key_count("url:") == 1
        && exactly_one(&format!("  canonical_url: {url}"))
        && key_count("  canonical_url:") == 1
        && exactly_one(&format!("  url_id: {digest}"))
        && key_count("  url_id:") == 1
        && matches!(
            (marker_index("<!-- rauser:start -->"), marker_index("<!-- rauser:end -->")),
            (Some(start), Some(end)) if start < end
        )
}
