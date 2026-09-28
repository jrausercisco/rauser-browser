//! Short-lived, process-local grants for folder selection and capture consent.
//! Only the native host can mint these grants; the extension cannot authorize
//! a path, redirect writes, or broaden capture by sending an arbitrary config.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use brauser_protocol::{ConfigSnapshot, SiteConfig};
use uuid::Uuid;

use crate::brand::APP_NAME;
use crate::config;
use crate::dialog::{self, DialogText};
use crate::vault::selected_root_identity;

const TOKEN_LIFETIME: Duration = Duration::from_secs(5 * 60);
const MAX_PENDING_GRANTS: usize = 32;
const MAX_CONSENT_SUMMARY_BYTES: usize = 8192;

pub struct FolderSelection {
    pub path: String,
    pub picker_token: String,
}

pub struct ConfigConfirmation {
    pub consent_token: String,
    pub summary: String,
}

struct PickerGrant {
    root: String,
    identity: String,
    revision: String,
    expires_at: Instant,
}

struct ConsentGrant {
    before: ConfigSnapshot,
    after: ConfigSnapshot,
    revision: String,
    expires_at: Instant,
}

#[derive(Default)]
pub struct ConsentAuthority {
    picker_grants: HashMap<String, PickerGrant>,
    consent_grants: HashMap<String, ConsentGrant>,
}

impl ConsentAuthority {
    pub fn new() -> Self {
        Self::default()
    }

    /// Blocks this connection until the user closes the picker, which runs
    /// in a child process (see `dialog`).
    pub fn choose_folder(&mut self, revision: &str) -> Result<Option<FolderSelection>> {
        let Some(selected) = dialog::pick_folder(&format!("Choose {APP_NAME} notes folder"))?
        else {
            return Ok(None);
        };
        let canonical = fs::canonicalize(&selected)
            .with_context(|| format!("resolving selected folder {}", selected.display()))?;
        if !canonical.is_dir() {
            bail!("selected notes folder is not a directory");
        }
        let root = canonical
            .to_str()
            .context("selected notes folder path cannot be represented as UTF-8")?
            .to_owned();
        let identity = selected_root_identity(Path::new(&root))?;
        let picker_token = self.mint_picker(root.clone(), identity, revision);
        Ok(Some(FolderSelection {
            path: root,
            picker_token,
        }))
    }

    /// The dialog text is derived only from the actual policy delta. A caller
    /// supplied string never appears in the native confirmation as a claim.
    pub fn confirm_config(
        &mut self,
        current: &ConfigSnapshot,
        revision: &str,
        next: &ConfigSnapshot,
        picker_token: Option<&str>,
    ) -> Result<Option<ConfigConfirmation>> {
        config::validate(next)?;
        self.check_picker(current, revision, next, picker_token)?;
        let changes = approval_lines(current, next);
        let summary = if changes.is_empty() {
            "No additional capture access is requested.".to_owned()
        } else {
            changes.join("\n")
        };
        if summary.len() > MAX_CONSENT_SUMMARY_BYTES {
            bail!("too many privacy changes for one confirmation; add fewer sites at once");
        }
        if !changes.is_empty() {
            let description =
                format!("{APP_NAME} will make these changes:\n\n{summary}\n\nAllow these changes?");
            if !dialog::confirm(DialogText {
                title: &format!("Confirm {APP_NAME} settings"),
                description: &description,
            })? {
                return Ok(None);
            }
        }
        let consent_token = self.mint_consent(current.clone(), next.clone(), revision);
        Ok(Some(ConfigConfirmation {
            consent_token,
            summary,
        }))
    }

    /// Validate grants immediately before the revision-checked config write.
    /// Grants are removed only after the write succeeds.
    pub fn authorize_update(
        &self,
        current: &ConfigSnapshot,
        revision: &str,
        next: &ConfigSnapshot,
        picker_token: Option<&str>,
        consent_token: Option<&str>,
    ) -> Result<Option<String>> {
        config::validate(next)?;
        let selected_identity = self.check_picker(current, revision, next, picker_token)?;
        if !approval_lines(current, next).is_empty() {
            let token =
                consent_token.context("native confirmation is required for this config change")?;
            let Some(grant) = self.consent_grants.get(token) else {
                bail!("native confirmation token is unknown or already used");
            };
            if grant.expires_at <= Instant::now()
                || grant.revision != revision
                || grant.before != *current
                || grant.after != *next
            {
                bail!("native confirmation is stale or does not match this change");
            }
        }
        Ok(selected_identity)
    }

    /// Call only after ConfigStore::update returns a committed revision.
    pub fn consume_update(
        &mut self,
        _current: &ConfigSnapshot,
        _revision: &str,
        _next: &ConfigSnapshot,
        picker_token: Option<&str>,
        consent_token: Option<&str>,
    ) {
        if let Some(token) = picker_token {
            self.picker_grants.remove(token);
        }
        if let Some(token) = consent_token {
            self.consent_grants.remove(token);
        }
    }

    fn check_picker(
        &self,
        current: &ConfigSnapshot,
        revision: &str,
        next: &ConfigSnapshot,
        picker_token: Option<&str>,
    ) -> Result<Option<String>> {
        let old_root = current
            .storage
            .as_ref()
            .map(|storage| storage.root.as_str());
        let new_root = next.storage.as_ref().map(|storage| storage.root.as_str());
        if new_root.is_none() || (new_root == old_root && picker_token.is_none()) {
            return Ok(None);
        }
        let root = new_root.expect("checked above");
        let token = picker_token.context("a native folder selection is required")?;
        let Some(grant) = self.picker_grants.get(token) else {
            bail!("folder selection token is unknown or already used");
        };
        if grant.expires_at <= Instant::now() || grant.revision != revision || grant.root != root {
            bail!("folder selection is stale or does not match the proposed notes folder");
        }
        if selected_root_identity(Path::new(root))? != grant.identity {
            bail!("selected notes folder changed; choose it again");
        }
        Ok(Some(grant.identity.clone()))
    }

    fn mint_picker(&mut self, root: String, identity: String, revision: &str) -> String {
        self.prune();
        let token = Uuid::new_v4().to_string();
        self.picker_grants.insert(
            token.clone(),
            PickerGrant {
                root,
                identity,
                revision: revision.to_owned(),
                expires_at: Instant::now() + TOKEN_LIFETIME,
            },
        );
        token
    }

    fn mint_consent(
        &mut self,
        before: ConfigSnapshot,
        after: ConfigSnapshot,
        revision: &str,
    ) -> String {
        self.prune();
        let token = Uuid::new_v4().to_string();
        self.consent_grants.insert(
            token.clone(),
            ConsentGrant {
                before,
                after,
                revision: revision.to_owned(),
                expires_at: Instant::now() + TOKEN_LIFETIME,
            },
        );
        token
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.picker_grants.retain(|_, grant| grant.expires_at > now);
        self.consent_grants
            .retain(|_, grant| grant.expires_at > now);
        if self.picker_grants.len() >= MAX_PENDING_GRANTS {
            self.picker_grants.clear();
        }
        if self.consent_grants.len() >= MAX_PENDING_GRANTS {
            self.consent_grants.clear();
        }
    }
}

fn approval_lines(current: &ConfigSnapshot, next: &ConfigSnapshot) -> Vec<String> {
    let mut lines = Vec::new();
    // A hand-edited but unusable config is not a trusted prior grant. Repair
    // must ask again before it makes any of those rules effective.
    let trusted_current = config::validate(current).is_ok();
    if !(trusted_current && current.capture_enabled) && next.capture_enabled {
        lines.push("Turn on automatic visit logging.".to_owned());
    }
    for site in &next.sites {
        if !trusted_current || !current.sites.iter().any(|prior| site_covers(prior, site)) {
            lines.push(format!(
                "Allow visit logging for {:?}",
                format!("{}{}", site.origin, site.path_prefix)
            ));
        }
    }
    let old_strip = if trusted_current {
        current.strip_params.clone()
    } else {
        config::default_strip_params()
    };
    for param in &old_strip {
        if !next.strip_params.iter().any(|value| value == param) {
            lines.push(format!(
                "Keep the query parameter {param:?} in logged URLs."
            ));
        }
    }
    let prior_repeat = if trusted_current {
        current.near_repeat_secs
    } else {
        config::default_near_repeat_secs()
    };
    if next.near_repeat_secs < prior_repeat {
        lines.push(format!(
            "Shorten repeat suppression from {} to {} seconds.",
            prior_repeat, next.near_repeat_secs
        ));
    }
    if let Some(next_storage) = &next.storage {
        let old_storage = if trusted_current {
            current.storage.as_ref()
        } else {
            None
        };
        for (name, old, new) in [
            (
                "profile",
                old_storage.map(|value| value.profile.as_str()),
                next_storage.profile.as_str(),
            ),
            (
                "visit log folder",
                old_storage.map(|value| value.log_dir.as_str()),
                next_storage.log_dir.as_str(),
            ),
            (
                "page notes folder",
                old_storage.map(|value| value.pages_dir.as_str()),
                next_storage.pages_dir.as_str(),
            ),
            (
                "read-later folder",
                old_storage.map(|value| value.later_dir.as_str()),
                next_storage.later_dir.as_str(),
            ),
        ] {
            if old != Some(new) {
                if let Some(old) = old {
                    lines.push(format!("Change {name} from {old:?} to {new:?}."));
                } else {
                    lines.push(format!("Set {name} to {new:?}."));
                }
            }
        }
    }
    lines
}

fn site_covers(prior: &SiteConfig, next: &SiteConfig) -> bool {
    if prior.origin != next.origin {
        return false;
    }
    prior.path_prefix == "/"
        || prior.path_prefix == next.path_prefix
        || (next.path_prefix.starts_with(&prior.path_prefix)
            && (prior.path_prefix.ends_with('/')
                || next.path_prefix.as_bytes().get(prior.path_prefix.len()) == Some(&b'/')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use brauser_protocol::StorageConfig;

    #[test]
    fn fresh_picker_token_can_reconfirm_the_same_path() {
        let folder = tempfile::tempdir().unwrap();
        let storage = StorageConfig {
            root: folder.path().to_string_lossy().into_owned(),
            profile: "neutral".into(),
            log_dir: "log".into(),
            pages_dir: "pages".into(),
            later_dir: "later".into(),
        };
        let snapshot = ConfigSnapshot {
            storage: Some(storage.clone()),
            capture_enabled: false,
            sites: Vec::new(),
            strip_params: Vec::new(),
            near_repeat_secs: 300,
        };
        let mut authority = ConsentAuthority::new();
        let identity = selected_root_identity(folder.path()).unwrap();
        let token = authority.mint_picker(storage.root, identity.clone(), "revision");
        assert_eq!(
            authority
                .check_picker(&snapshot, "revision", &snapshot, Some(&token))
                .unwrap(),
            Some(identity)
        );
    }
}
