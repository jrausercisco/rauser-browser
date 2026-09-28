//! The agent denylist (§7.3). Entries are hostnames the user never wants an
//! agent to read. The host stores only normalized entries and rejects any
//! other form rather than guessing what the user meant.

use std::collections::HashSet;
use std::net::Ipv6Addr;

use anyhow::{Result, bail};
use url::{Host, Url};

use crate::config::has_unsafe_display_chars;

pub(crate) const MAX_DENYLIST_ENTRIES: usize = 128;
const MAX_HOSTNAME_BYTES: usize = 253;
const MAX_LABEL_BYTES: usize = 63;
const NOT_NORMALIZED: &str = "a normalized hostname; AI privacy exclusions must be lowercase punycode with no scheme, path, port, wildcard, empty label, or leading or trailing dot";
/// How much of a rejected entry an error quotes.
const SHOWN_ENTRY_CHARS: usize = 64;

pub fn validate_denylist(entries: &[String]) -> Result<()> {
    if entries.len() > MAX_DENYLIST_ENTRIES {
        bail!("AI privacy exclusions exceed the 128-entry limit");
    }
    let mut seen = HashSet::new();
    for entry in entries {
        if !normalized_entry(entry) {
            bail!("{} is not {NOT_NORMALIZED}", shown_entry(entry));
        }
        if !seen.insert(entry.as_str()) {
            bail!("AI privacy exclusions contain a duplicate entry");
        }
    }
    Ok(())
}

/// A rejected entry quoted for an error the settings page shows: escaped,
/// so control and bidi characters cannot disguise it, and cut short.
fn shown_entry(entry: &str) -> String {
    let mut shown = String::from("\"");
    for ch in entry.chars().take(SHOWN_ENTRY_CHARS) {
        if ch == ' ' || (ch.is_ascii_graphic() && ch != '"' && ch != '\\') {
            shown.push(ch);
        } else {
            shown.extend(ch.escape_unicode());
        }
    }
    if entry.chars().nth(SHOWN_ENTRY_CHARS).is_some() {
        shown.push_str("...");
    }
    shown.push('"');
    shown
}

fn normalized_entry(entry: &str) -> bool {
    if entry.is_empty()
        || has_unsafe_display_chars(entry)
        || entry
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '/' | '?' | '#' | '@' | '*' | '\\'))
        || entry.starts_with('.')
        || entry.ends_with('.')
    {
        return false;
    }
    let bracketed = entry.starts_with('[') && entry.ends_with(']');
    if entry.contains(':') && !bracketed {
        return false;
    }
    match Host::parse(entry) {
        Ok(Host::Domain(domain)) => {
            domain == entry
                && entry.len() <= MAX_HOSTNAME_BYTES
                && entry
                    .split('.')
                    .all(|label| !label.is_empty() && label.len() <= MAX_LABEL_BYTES)
        }
        Ok(Host::Ipv4(address)) => address.to_string() == entry,
        Ok(Host::Ipv6(address)) => bracketed && ipv6_entry(address) == entry,
        Err(_) => false,
    }
}

/// Check the page's original URL, before any strip_params normalization. A
/// URL this cannot classify is denied: failing closed keeps content from an
/// unexpected scheme or unparseable address away from the agent.
pub fn denylisted(original: &str, entries: &[String]) -> bool {
    let Ok(url) = Url::parse(original) else {
        return true;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return true;
    }
    match url.host() {
        None => true,
        Some(Host::Domain(domain)) => {
            // A fully qualified "bank.com." names the same host as "bank.com".
            let domain = domain.strip_suffix('.').unwrap_or(domain);
            // Any other empty label ("bank.example..") cannot be classified.
            if domain.split('.').any(str::is_empty) {
                return true;
            }
            entries.iter().any(|entry| {
                !entry.starts_with('[')
                    && (domain == entry
                        || domain
                            .strip_suffix(entry.as_str())
                            .is_some_and(|rest| rest.ends_with('.')))
            })
        }
        // An IP literal matches the same address, however the URL writes it:
        // an IPv4-mapped IPv6 URL reaches the IPv4 host it maps.
        Some(Host::Ipv4(address)) => {
            entries.contains(&address.to_string())
                || entries.contains(&ipv6_entry(address.to_ipv6_mapped()))
        }
        Some(Host::Ipv6(address)) => {
            entries.contains(&ipv6_entry(address))
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| entries.contains(&mapped.to_string()))
        }
    }
}

/// The WHATWG form the browser's URL (and so the settings page) produces.
/// std's Display differs: it writes an IPv4-mapped address dotted.
fn ipv6_entry(address: Ipv6Addr) -> String {
    Host::<String>::Ipv6(address).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn list(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn denylist_rejects_scheme_path_port_wildcard_trailing_dot() {
        for entry in [
            "https://a.com",
            "a.com/x",
            "a.com:443",
            "*.a.com",
            "a.com.",
            ".a.com",
            "A.com",
            "bücher.de",
            "a..com",
            "a com",
            "user@a.com",
            "a.com?x",
            "a.com#x",
            "a\\com",
            "::1",
            "[::1]:443",
            "[0:0:0:0:0:0:0:1]",
            "01.2.3.4",
            "1.2.3",
            "a.com\u{202e}",
            "",
        ] {
            assert!(
                validate_denylist(&list(&[entry])).is_err(),
                "{entry:?} was accepted"
            );
        }
        let long_label = format!("{}.com", "a".repeat(64));
        assert!(validate_denylist(&[long_label]).is_err());
        let long_name = format!("{}.com", vec!["a".repeat(60); 5].join("."));
        assert!(long_name.len() > MAX_HOSTNAME_BYTES);
        assert!(validate_denylist(&[long_name]).is_err());
    }

    #[test]
    fn denylist_error_names_the_bad_entry_safely() {
        let error = validate_denylist(&list(&["bank.example", "a..com"])).unwrap_err();
        assert!(
            error.to_string().starts_with("\"a..com\" is not"),
            "{error}"
        );
        // Control and bidi characters are escaped, and a long entry is cut.
        let error = validate_denylist(&list(&["a.com\u{202e}\n"])).unwrap_err();
        assert!(
            !config::has_unsafe_display_chars(&error.to_string()),
            "{error}"
        );
        let error = validate_denylist(&[format!("{}.com", "a".repeat(400))]).unwrap_err();
        assert!(error.to_string().len() < 400, "{error}");
    }

    #[test]
    fn denylist_accepts_punycode_ipv4_bracketed_ipv6() {
        validate_denylist(&list(&[
            "bank.example",
            "xn--bcher-kva.de",
            "mail.corp.example",
            "10.0.0.1",
            "[::1]",
            "[2001:db8::1]",
        ]))
        .unwrap();
        validate_denylist(&[]).unwrap();
    }

    #[test]
    fn denylist_rejects_duplicates_and_limit() {
        assert!(validate_denylist(&list(&["a.com", "a.com"])).is_err());
        let full: Vec<String> = (0..MAX_DENYLIST_ENTRIES)
            .map(|index| format!("h{index}.example"))
            .collect();
        validate_denylist(&full).unwrap();
        let mut over = full;
        over.push("extra.example".into());
        assert!(validate_denylist(&over).is_err());
    }

    #[test]
    fn denylist_matches_host_and_subdomains_any_scheme_port() {
        let entries = list(&["bank.example"]);
        for url in [
            "https://bank.example/",
            "http://bank.example:8080/login",
            "https://www.bank.example/a?b=c",
            "https://a.b.bank.example/",
            "https://BANK.example/",
            "https://bank.example./",
        ] {
            assert!(denylisted(url, &entries), "{url} was allowed");
        }
        for url in [
            "https://notbank.example/",
            "https://bank.example.org/",
            "https://example/",
        ] {
            assert!(!denylisted(url, &entries), "{url} was denied");
        }
        let unicode = list(&["xn--bcher-kva.de"]);
        assert!(denylisted("https://shop.bücher.de/", &unicode));
    }

    #[test]
    fn denylist_ip_literal_matches_exactly() {
        let entries = list(&["10.0.0.1", "[2001:db8::1]"]);
        assert!(denylisted("http://10.0.0.1:8000/", &entries));
        assert!(!denylisted("http://10.0.0.10/", &entries));
        assert!(denylisted("https://[2001:db8:0::1]/", &entries));
        assert!(!denylisted("https://[2001:db8::2]/", &entries));
        // An IP entry never matches a hostname by suffix.
        assert!(!denylisted("https://host.example/", &entries));
    }

    #[test]
    fn denylist_ipv6_uses_the_url_serialization() {
        // The settings page normalizes through the browser's URL, which
        // writes an IPv4-mapped address in hex, as the url crate does.
        validate_denylist(&list(&["[::ffff:a00:1]"])).unwrap();
        assert!(validate_denylist(&list(&["[::ffff:10.0.0.1]"])).is_err());
        let entries = list(&["[::ffff:a00:1]"]);
        assert!(denylisted("http://[::ffff:10.0.0.1]/", &entries));
        assert!(denylisted("http://[::ffff:a00:1]:8080/", &entries));
    }

    #[test]
    fn denylist_ip_matches_its_ipv4_mapped_form() {
        // "IP literals match exactly" means the same address, and an
        // IPv4-mapped IPv6 URL reaches the same IPv4 host.
        let ipv4 = list(&["10.0.0.1"]);
        assert!(denylisted("http://[::ffff:10.0.0.1]/", &ipv4));
        assert!(denylisted("http://[::ffff:a00:1]/", &ipv4));
        assert!(!denylisted("http://[::ffff:10.0.0.2]/", &ipv4));
        let mapped = list(&["[::ffff:a00:1]"]);
        assert!(denylisted("http://10.0.0.1/", &mapped));
        assert!(!denylisted("http://10.0.0.2/", &mapped));
    }

    #[test]
    fn denylist_checks_original_url_not_normalized() {
        // Normalization refuses credentials and would drop the fragment and
        // tracking parameters; the denylist still classifies the original.
        let entries = list(&["bank.example"]);
        let original = "https://user:secret@bank.example/?utm_source=x#frag";
        assert!(crate::capture::canonical_url(original, &[]).is_err());
        assert!(denylisted(original, &entries));
    }

    #[test]
    fn unparseable_or_hostless_url_is_denied() {
        for url in [
            "not a url",
            "file:///etc/passwd",
            "data:text/html,hi",
            "chrome://settings",
            "ftp://files.example/",
            "",
        ] {
            assert!(denylisted(url, &[]), "{url:?} was allowed");
        }
        assert!(!denylisted("https://example.com/", &[]));
    }

    #[test]
    fn host_with_empty_label_is_denied() {
        // The url crate keeps empty labels, so "bank.example.." stays that
        // way after parsing; stripping one dot would leave it unmatched.
        let entries = list(&["bank.example"]);
        for url in [
            "https://bank.example../",
            "https://bank.example.../",
            "https://www..bank.example/",
            "https://other..example/",
        ] {
            assert!(denylisted(url, &entries), "{url} was allowed");
            assert!(denylisted(url, &[]), "{url} was allowed with no entries");
        }
    }
}
