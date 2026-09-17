//! Validation helpers for OAuth redirect URIs accepted on the local control channel.

use std::net::IpAddr;

const MAX_REDIRECT_URI_BYTES: usize = 2048;

/// Returns `Ok(())` when `redirect_uri` is an HTTP URL whose host is a loopback literal.
///
/// The authority is parsed strictly: userinfo, fragments, malformed IPv6
/// brackets, empty or non-numeric ports, and out-of-range ports are all
/// rejected rather than silently ignored.
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
    let authority = remainder
        .split(&['/', '?'][..])
        .next()
        .filter(|value| !value.is_empty())
        .ok_or("redirect URI must include a host")?;
    if authority.contains('@') {
        return Err("redirect URI must not include userinfo");
    }
    if !authority_is_loopback(authority) {
        return Err("redirect URI host must be a loopback address");
    }
    Ok(())
}

fn authority_is_loopback(authority: &str) -> bool {
    if let Some(rest) = authority.strip_prefix('[') {
        // A bracketed IPv6 literal must close with ']' and carry at most a
        // port; anything after the bracket that is not ":port" is malformed.
        let Some((host, suffix)) = rest.split_once(']') else {
            return false;
        };
        if !suffix.is_empty() && !port_suffix_is_valid(suffix) {
            return false;
        }
        return host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    }
    if authority.contains(']') || authority.matches(':').count() > 1 {
        // Stray bracket or an unbracketed IPv6 literal.
        return false;
    }
    let host = match authority.split_once(':') {
        Some((host, port)) => {
            if !port_is_valid(port) {
                return false;
            }
            host
        }
        None => authority,
    };
    if host.is_empty() {
        return false;
    }
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn port_suffix_is_valid(suffix: &str) -> bool {
    suffix.strip_prefix(':').is_some_and(port_is_valid)
}

fn port_is_valid(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::validate_loopback_http_redirect_uri;

    #[test]
    fn accepts_registered_loopback_spellings() {
        for uri in [
            "http://localhost:1455/auth/callback",
            "http://localhost",
            "http://127.0.0.1:1456/callback",
            "http://[::1]:8080/callback",
            "http://[::1]",
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
            "http://10.0.0.8/callback",
            "http://[2001:db8::1]/callback",
        ] {
            assert!(validate_loopback_http_redirect_uri(uri).is_err(), "{uri}");
        }
    }

    #[test]
    fn rejects_malformed_authorities() {
        for uri in [
            "http://[::1]junk/callback",
            "http://[::1]junk:8080/callback",
            "http://[::1/callback",
            "http://::1/callback",
            "http://127.0.0.1:notaport/callback",
            "http://127.0.0.1:/callback",
            "http://127.0.0.1:99999/callback",
            "http://localhost:80x/callback",
            "http://:8080/callback",
            "http:///callback",
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
