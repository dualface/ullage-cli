//! Validation helpers for OAuth redirect URIs accepted on the local control channel.

const MAX_REDIRECT_URI_BYTES: usize = 2048;

/// Returns `Ok(())` when `redirect_uri` is an HTTP URL whose host is a loopback literal.
pub fn validate_loopback_http_redirect_uri(redirect_uri: &str) -> Result<(), &'static str> {
    if redirect_uri.is_empty() || redirect_uri.len() > MAX_REDIRECT_URI_BYTES {
        return Err("redirect URI is empty or too long");
    }
    let Some((scheme, remainder)) = redirect_uri.split_once("://") else {
        return Err("redirect URI must include a scheme");
    };
    if scheme != "http" {
        return Err("redirect URI must use the http scheme");
    }
    if remainder.contains('#') {
        return Err("redirect URI must not include a fragment");
    }
    if remainder.contains('@') {
        return Err("redirect URI must not include userinfo");
    }
    let authority = remainder
        .split(&['/', '?'][..])
        .next()
        .filter(|value| !value.is_empty())
        .ok_or("redirect URI must include a host")?;
    if !host_is_loopback_literal(authority) {
        return Err("redirect URI host must be a loopback address");
    }
    Ok(())
}

fn host_is_loopback_literal(authority: &str) -> bool {
    if let Some(host) = authority.strip_prefix('[') {
        let Some((host, _)) = host.split_once(']') else {
            return false;
        };
        return host.eq_ignore_ascii_case("::1");
    }
    let host = authority.split(':').next().unwrap_or(authority);
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host.eq_ignore_ascii_case("::1")
}

#[cfg(test)]
mod tests {
    use super::validate_loopback_http_redirect_uri;

    #[test]
    fn accepts_registered_loopback_spellings() {
        for uri in [
            "http://localhost:1455/auth/callback",
            "http://127.0.0.1:1456/callback",
            "http://[::1]:8080/callback",
        ] {
            assert_eq!(validate_loopback_http_redirect_uri(uri), Ok(()), "{uri}");
        }
    }

    #[test]
    fn rejects_non_loopback_and_non_http_values() {
        for uri in [
            "",
            "https://127.0.0.1/callback",
            "http://example.test/callback",
            "http://user@127.0.0.1/callback",
            "http://127.0.0.1/callback#fragment",
        ] {
            assert!(validate_loopback_http_redirect_uri(uri).is_err(), "{uri}");
        }
    }

    #[test]
    fn auth_start_request_deserializes_without_redirect_uri() {
        let request: crate::AuthStartRequest =
            serde_json::from_str(r#"{"method":"browser_o_auth"}"#).unwrap();
        assert_eq!(request.method, Some(crate::AuthMethod::BrowserOAuth));
        assert_eq!(request.redirect_uri, None);
    }
}
