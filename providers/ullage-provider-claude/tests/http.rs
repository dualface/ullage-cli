use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use ullage_core::ProviderError;
use ullage_provider_claude::{
    AuthorizationCodeExchange, ClaudeApi, ClaudeHttpConfig, HttpClaudeApi,
};

fn read_request(mut stream: &TcpStream) -> String {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 2048];
    let header_end = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "client closed before sending headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "client closed before sending body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8(bytes).unwrap()
}

fn respond(mut stream: TcpStream, status: &str, headers: &[(&str, &str)], body: &str) -> String {
    let request = read_request(&stream);
    let has_content_type = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )
    .unwrap();
    if !has_content_type {
        write!(stream, "Content-Type: application/json\r\n").unwrap();
    }
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    write!(stream, "\r\n{body}").unwrap();
    request
}

fn test_config(base_url: &str) -> ClaudeHttpConfig {
    ClaudeHttpConfig {
        api_base: base_url.to_owned(),
        token_endpoint: format!("{base_url}/token"),
        revoke_endpoint: format!("{base_url}/revoke"),
    }
}

fn request_body(request: &str) -> serde_json::Value {
    let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
    serde_json::from_str(body).unwrap()
}

#[tokio::test]
async fn exchange_refresh_and_revoke_use_the_oauth_wire_shape() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let exchange = respond(
            listener.accept().unwrap().0,
            "200 OK",
            &[],
            r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#,
        );
        let refresh = respond(
            listener.accept().unwrap().0,
            "200 OK",
            &[],
            r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600,"token_type":"Bearer"}"#,
        );
        let revoke = respond(listener.accept().unwrap().0, "200 OK", &[], "{}");
        vec![exchange, refresh, revoke]
    });

    let api = HttpClaudeApi::with_config(test_config(&base_url)).unwrap();
    let tokens = api
        .exchange_code(AuthorizationCodeExchange {
            code: "code-1".into(),
            state: "state-1".into(),
            redirect_uri: "https://platform.claude.com/oauth/code/callback".into(),
            code_verifier: "verifier-1".into(),
        })
        .await
        .unwrap();
    assert_eq!(tokens.access_token, "access-1");
    let refreshed = api.refresh_token("refresh-1").await.unwrap();
    assert_eq!(refreshed.access_token, "access-2");
    api.revoke_token("refresh-2", "refresh_token")
        .await
        .unwrap();

    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("POST /token "));
    let exchange_body = request_body(&requests[0]);
    assert_eq!(exchange_body["grant_type"], "authorization_code");
    assert_eq!(exchange_body["code"], "code-1");
    assert_eq!(exchange_body["state"], "state-1");
    assert_eq!(exchange_body["code_verifier"], "verifier-1");
    assert_eq!(
        exchange_body["redirect_uri"],
        "https://platform.claude.com/oauth/code/callback"
    );
    let refresh_body = request_body(&requests[1]);
    assert_eq!(refresh_body["grant_type"], "refresh_token");
    assert_eq!(refresh_body["refresh_token"], "refresh-1");
    let revoke_body = request_body(&requests[2]);
    assert_eq!(revoke_body["token"], "refresh-2");
    assert_eq!(revoke_body["token_type_hint"], "refresh_token");
    for request in &requests {
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("user-agent: ullage/{}", env!("CARGO_PKG_VERSION"))),
            "{request}"
        );
    }
}

#[tokio::test]
async fn revoke_reports_the_access_token_hint() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || respond(listener.accept().unwrap().0, "200 OK", &[], "{}"));
    let api = HttpClaudeApi::with_config(test_config(&base_url)).unwrap();
    api.revoke_token("access-1", "access_token").await.unwrap();
    let request = server.join().unwrap();
    let body = request_body(&request);
    assert_eq!(body["token"], "access-1");
    assert_eq!(body["token_type_hint"], "access_token");
}

#[tokio::test]
async fn authenticated_gets_send_bearer_and_beta_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let profile = respond(listener.accept().unwrap().0, "200 OK", &[], "{}");
        let usage = respond(listener.accept().unwrap().0, "200 OK", &[], "{}");
        vec![profile, usage]
    });
    let api = HttpClaudeApi::with_config(test_config(&base_url)).unwrap();
    api.profile("access-1").await.unwrap();
    api.usage("access-1").await.unwrap();
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /api/oauth/profile "));
    assert!(requests[1].starts_with("GET /api/oauth/usage "));
    for request in &requests {
        let headers = request.to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer access-1"));
        assert!(headers.contains("anthropic-beta: oauth-2025-04-20"));
    }
}

#[tokio::test]
async fn token_endpoint_errors_classify_oauth_error_codes() {
    let cases = [
        (
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"refresh token expired"}"#,
            "AuthenticationInvalid",
        ),
        (
            "400 Bad Request",
            r#"{"error":"invalid_client","error_description":"bad client"}"#,
            "ProtocolIncompatible",
        ),
        (
            "400 Bad Request",
            r#"{"error":"unsupported_grant_type"}"#,
            "ProtocolIncompatible",
        ),
        ("400 Bad Request", "not json at all", "ProtocolIncompatible"),
        ("400 Bad Request", "{}", "ProtocolIncompatible"),
        (
            "401 Unauthorized",
            r#"{"error":"anything"}"#,
            "AuthenticationInvalid",
        ),
        ("403 Forbidden", "{}", "AuthenticationInvalid"),
        ("408 Request Timeout", "{}", "Network"),
        ("500 Internal Server Error", "{}", "Network"),
        ("503 Service Unavailable", "{}", "Network"),
        ("418 I'm a Teapot", "{}", "ProtocolIncompatible"),
    ];
    for (status, body, expected_kind) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server =
            thread::spawn(move || respond(listener.accept().unwrap().0, status, &[], body));
        let api = HttpClaudeApi::with_config(test_config(&base_url)).unwrap();
        let error = api.refresh_token("refresh-1").await.unwrap_err();
        let kind = match &error {
            ProviderError::AuthenticationInvalid { .. } => "AuthenticationInvalid",
            ProviderError::RateLimited { .. } => "RateLimited",
            ProviderError::Network { .. } => "Network",
            ProviderError::ProtocolIncompatible { .. } => "ProtocolIncompatible",
            ProviderError::UnsupportedCapability { .. } => "UnsupportedCapability",
        };
        assert_eq!(kind, expected_kind, "{status} {body}");
        // Error bodies are never echoed: descriptions may carry anything.
        assert!(!format!("{error:?}").contains("expired"));
        assert!(!format!("{error:?}").contains("bad client"));
        server.join().unwrap();
    }
}

#[tokio::test]
async fn a_429_carries_the_retry_after_delay() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        respond(
            listener.accept().unwrap().0,
            "429 Too Many Requests",
            &[("Retry-After", "42")],
            "{}",
        )
    });
    let api = HttpClaudeApi::with_config(test_config(&base_url)).unwrap();
    let error = api.usage("access-1").await.unwrap_err();
    assert!(matches!(
        error,
        ProviderError::RateLimited {
            retry_after_seconds: Some(42),
            ..
        }
    ));
    server.join().unwrap();
}
