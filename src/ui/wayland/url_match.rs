//! Matching a pasted URL against saved sites by base domain, like
//! Bitwarden's default: `login.github.com` matches a saved `github.com` and
//! a saved `accounts.github.com`.

/// Second-level labels that registries sell under country domains, as in
/// `co.uk` or `com.au`. A heuristic rather than the public suffix list.
const REGISTRY_LABELS: &[&str] = &[
    "ac", "co", "com", "edu", "gob", "go", "gov", "ltd", "mil", "ne", "net", "nic", "or", "org",
    "plc", "sch",
];

/// The host in `text` when it looks like a URL or a domain: an optional
/// scheme, user and port around a dotted host name.
fn host(text: &str) -> Option<String> {
    let rest = text.split_once("://").map_or(text, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => host,
        _ => host_port,
    };
    let host = host.trim_end_matches('.').to_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let valid = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && label.chars().all(|c| c.is_alphanumeric() || c == '-')
        });
    let is_ipv4 = labels.len() == 4 && labels.iter().all(|label| label.parse::<u8>().is_ok());
    let named = labels
        .last()
        .is_some_and(|tld| tld.chars().any(char::is_alphabetic));
    (valid && (is_ipv4 || named)).then_some(host)
}

/// The host a filter names, when the filter is a single URL or domain.
pub(super) fn query_host(filter: &str) -> Option<String> {
    let filter = filter.trim();
    if filter.is_empty() || filter.contains(char::is_whitespace) {
        return None;
    }
    host(filter)
}

/// The host of a saved URI. App URIs such as `androidapp://` name no site.
pub(super) fn uri_host(uri: &str) -> Option<String> {
    let uri = uri.trim();
    if let Some((scheme, _)) = uri.split_once("://") {
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return None;
        }
    }
    host(uri)
}

/// The registrable part of `host`: `github.com` for `login.github.com`,
/// `example.co.uk` for `www.example.co.uk`. Addresses are kept whole.
pub(super) fn base_domain(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').collect();
    if labels.iter().all(|label| label.parse::<u8>().is_ok()) {
        return host.to_string();
    }
    let n = labels.len();
    let keep = if n >= 3 && labels[n - 1].len() == 2 && REGISTRY_LABELS.contains(&labels[n - 2]) {
        3
    } else {
        2
    };
    labels[n.saturating_sub(keep)..].join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_host_reads_urls_and_domains() {
        assert_eq!(
            query_host("https://alice@Login.GitHub.com:8443/settings?tab=1#x").as_deref(),
            Some("login.github.com")
        );
        assert_eq!(query_host("github.com").as_deref(), Some("github.com"));
        assert_eq!(query_host("github.com/").as_deref(), Some("github.com"));
        assert_eq!(query_host("10.0.0.1:8080").as_deref(), Some("10.0.0.1"));
        for not_url in [
            "github",
            "two words.com",
            "1.5",
            "a..b",
            "",
            "-x.com",
            "notes:",
        ] {
            assert_eq!(query_host(not_url), None, "{not_url}");
        }
    }

    #[test]
    fn uri_host_skips_app_uris() {
        assert_eq!(
            uri_host("https://accounts.github.com/login").as_deref(),
            Some("accounts.github.com")
        );
        assert_eq!(uri_host("github.com").as_deref(), Some("github.com"));
        assert_eq!(uri_host("androidapp://com.github.android"), None);
        assert_eq!(uri_host("iosapp://com.github"), None);
    }

    #[test]
    fn base_domain_keeps_the_registrable_part() {
        assert_eq!(base_domain("login.github.com"), "github.com");
        assert_eq!(base_domain("github.com"), "github.com");
        assert_eq!(base_domain("www.example.co.uk"), "example.co.uk");
        assert_eq!(base_domain("shop.example.com.au"), "example.com.au");
        assert_eq!(base_domain("mail.example.de"), "example.de");
        assert_eq!(base_domain("192.168.1.1"), "192.168.1.1");
    }
}
