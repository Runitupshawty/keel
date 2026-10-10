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

#[cfg(test)]
mod tests {
    use super::*;

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
