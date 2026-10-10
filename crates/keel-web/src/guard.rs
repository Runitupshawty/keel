//! The client never takes the daemon token from its address: a token in a URL ends up in
//! the browser history, bookmarks, referrers and proxy logs. An address that looks like it
//! carries one is refused (and scrubbed from the address bar by the browser glue).

/// Whether `query` (`location.search`) or `fragment` (`location.hash`) seems to carry a
/// secret: a parameter named like one (`token`, `auth`, `key`, `secret`, `bearer`,
/// `password`) or any value of 32 or more hex digits (the daemon token is 64).
pub fn url_carries_token(query: &str, fragment: &str) -> bool {
    let params = |s: &str| -> Vec<(String, String)> {
        s.trim_start_matches(['?', '#'])
            .split(['&', ';'])
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (k.to_ascii_lowercase(), v.to_owned())
            })
            .collect()
    };
    let named = |k: &str| {
        ["token", "auth", "key", "secret", "bearer", "password"]
            .iter()
            .any(|n| k.contains(n))
    };
    let hexy = |s: &str| {
        s.split(|c: char| !c.is_ascii_hexdigit())
            .any(|run| run.len() >= 32)
    };
    params(query)
        .into_iter()
        .chain(params(fragment))
        .any(|(k, v)| named(&k) || hexy(&k) || hexy(&v))
}

/// Whether the page came over a connection others on the way can read: not `https:` and
/// not a loopback host. The client then warns that the token and files cross the network
/// in the clear.
pub fn insecure(protocol: &str, hostname: &str) -> bool {
    let loopback = matches!(
        hostname.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "[::1]" | "::1"
    ) || hostname.to_ascii_lowercase().ends_with(".localhost");
    protocol != "https:" && !loopback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_unless_tls_or_loopback() {
        assert!(!insecure("https:", "keel.example"));
        assert!(!insecure("http:", "localhost"));
        assert!(!insecure("http:", "127.0.0.1"));
        assert!(!insecure("http:", "[::1]"));
        assert!(insecure("http:", "192.0.2.7"));
        assert!(insecure("http:", "keel.example"));
        assert!(insecure("http:", "127.0.0.1.evil.example"));
    }

    #[test]
    fn refuses_a_token_in_the_query_string_or_fragment() {
        let token = "ab".repeat(32);
        assert!(url_carries_token(&format!("?token={token}"), ""));
        assert!(url_carries_token("?TOKEN=x", ""));
        assert!(url_carries_token("?access_token=x", ""));
        assert!(url_carries_token("?auth=x", ""));
        assert!(url_carries_token(&format!("?t={token}"), ""));
        assert!(url_carries_token(&format!("?{token}"), ""));
        assert!(url_carries_token("", &format!("#token={token}")));
        assert!(url_carries_token("", &format!("#/browse?k={token}")));
        assert!(url_carries_token("?view=grid&apikey=1", ""));
    }

    #[test]
    fn ordinary_addresses_pass() {
        assert!(!url_carries_token("", ""));
        assert!(!url_carries_token("?", "#"));
        assert!(!url_carries_token("?view=grid&q=invoice", "#search"));
        assert!(!url_carries_token("?id=0123456789abcdef", ""));
    }
}
