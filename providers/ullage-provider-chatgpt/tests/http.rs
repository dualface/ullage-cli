use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ullage_provider_chatgpt::{
    ChatGptApi, ChatGptApiErrorKind, ChatGptHttpConfig, OAuthTokenSet, ReqwestChatGptApi,
};

fn jwt(account_id: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "organizations": [{"id": account_id, "title": "Personal"}]
            }
        }))
        .unwrap(),
    );
    format!("{header}.{payload}.sig")
}

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

fn test_config(base_url: &str) -> ChatGptHttpConfig {
    ChatGptHttpConfig {
        client_id: "public-client".into(),
        token_endpoint: format!("{base_url}/token"),
        revoke_endpoint: format!("{base_url}/revoke"),
        usage_endpoint: format!("{base_url}/usage"),
    }
}

#[tokio::test]
async fn concrete_adapter_exchanges_lists_queries_and_revokes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let identity_token = jwt("workspace-123");
    let server = thread::spawn(move || {
        let token_body = serde_json::json!({
            "access_token": identity_token,
            "refresh_token": "refresh-secret",
            "id_token": jwt("workspace-123"),
            "expires_in": 3600
        })
        .to_string();
        let token = respond(listener.accept().unwrap().0, "200 OK", &[], &token_body);
        let usage = respond(
            listener.accept().unwrap().0,
            "200 OK",
            &[],
            include_str!("fixtures/single-window.json"),
        );
        let revoke = respond(listener.accept().unwrap().0, "200 OK", &[], "{}");
        vec![token, usage, revoke]
    });

    let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
    let tokens = api
        .exchange_code("auth-code", "pkce-verifier", "http://localhost/callback")
        .await
        .unwrap();
    let workspaces = api.list_workspaces(&tokens).await.unwrap();
    assert_eq!(workspaces[0].id, "workspace-123");
    assert_eq!(workspaces[0].label.as_deref(), Some("Personal"));
    let usage = api.query_usage(&tokens, "workspace-123").await.unwrap();
    assert_eq!(usage.plan_type.as_deref(), Some("plus"));
    assert!(usage.additional_rate_limits.is_empty());
    api.revoke(&tokens).await.unwrap();

    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("POST /token "));
    assert!(requests[0].contains("grant_type=authorization_code"));
    assert!(requests[0].contains("code_verifier=pkce-verifier"));
    assert!(requests[1].starts_with("GET /usage "));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("chatgpt-account-id: workspace-123")
    );
    assert!(!requests[0].to_ascii_lowercase().contains("originator:"));
    assert!(!requests[0].to_ascii_lowercase().contains("version:"));
    assert!(!requests[2].to_ascii_lowercase().contains("originator:"));
    assert!(!requests[2].to_ascii_lowercase().contains("version:"));
    assert!(!requests[2].contains("access-secret"));
    assert!(requests[2].contains("refresh-secret"));
}

#[tokio::test]
async fn concrete_adapter_classifies_rate_limit_without_returning_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        respond(
            listener.accept().unwrap().0,
            "429 Too Many Requests",
            &[("Retry-After", "15")],
            r#"{"sensitive":"must not escape"}"#,
        )
    });
    let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
    let tokens = OAuthTokenSet::new("access-secret", None, None).unwrap();
    let error = api.query_usage(&tokens, "workspace-123").await.unwrap_err();
    assert_eq!(error.kind, ChatGptApiErrorKind::RateLimited);
    assert_eq!(error.retry_after_seconds, Some(15));
    assert!(!error.message.contains("sensitive"));
    server.join().unwrap();
}

#[tokio::test]
async fn concrete_adapter_classifies_oauth_forbidden_as_invalid_authentication() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        respond(
            listener.accept().unwrap().0,
            "403 Forbidden",
            &[],
            r#"{"error":"policy rejected the OAuth client"}"#,
        )
    });
    let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
    let error = api
        .exchange_code("auth-code", "pkce-verifier", "http://localhost/callback")
        .await
        .unwrap_err();
    assert_eq!(error.kind, ChatGptApiErrorKind::AuthenticationInvalid);
    assert!(!error.message.contains("policy rejected"));
    server.join().unwrap();
}

#[tokio::test]
async fn query_usage_sends_originator_and_version_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        respond(
            listener.accept().unwrap().0,
            "200 OK",
            &[],
            include_str!("fixtures/single-window.json"),
        )
    });
    let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
    let tokens = OAuthTokenSet::new("access-secret", None, None).unwrap();
    api.query_usage(&tokens, "workspace-123").await.unwrap();
    let request = server.join().unwrap();
    let headers = request.to_ascii_lowercase();
    assert!(headers.contains("originator: codex_cli_rs"));
    assert!(headers.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))));
}

#[tokio::test]
async fn query_usage_classifies_cloudflare_challenge_as_network() {
    let cases = [
        (
            "403 Forbidden",
            &[
                ("Content-Type", "text/html; charset=UTF-8"),
                ("cf-mitigated", "challenge"),
                ("cf-ray", "9a1b2c3d4e5f6g7h-SIN"),
            ][..],
            "<html>challenge</html>",
            true,
        ),
        (
            "403 Forbidden",
            &[("Content-Type", "text/html")][..],
            "<html>challenge</html>",
            false,
        ),
        (
            "503 Service Unavailable",
            &[
                ("Content-Type", "text/html"),
                ("cf-mitigated", "challenge"),
                ("cf-ray", "deadbeef1234-SIN"),
            ][..],
            "<html>challenge</html>",
            true,
        ),
    ];

    for (status, headers, body, expect_cf_ray) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let headers = headers.to_vec();
        let body = body.to_owned();
        let server =
            thread::spawn(move || respond(listener.accept().unwrap().0, status, &headers, &body));
        let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
        let tokens = OAuthTokenSet::new("access-secret", None, None).unwrap();
        let error = api.query_usage(&tokens, "workspace-123").await.unwrap_err();
        assert_eq!(error.kind, ChatGptApiErrorKind::Network);
        assert!(error.message.contains("Cloudflare edge challenge"));
        if expect_cf_ray {
            assert!(error.message.contains("cf-ray "));
        } else {
            assert!(!error.message.contains("cf-ray"));
        }
        assert!(!error.message.contains("access-secret"));
        server.join().unwrap();
    }
}

#[tokio::test]
async fn query_usage_classifies_json_unauthorized_as_invalid_authentication() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        respond(
            listener.accept().unwrap().0,
            "401 Unauthorized",
            &[],
            r#"{"detail":"Could not parse your authentication token"}"#,
        )
    });
    let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
    let tokens = OAuthTokenSet::new("access-secret", None, None).unwrap();
    let error = api.query_usage(&tokens, "workspace-123").await.unwrap_err();
    assert_eq!(error.kind, ChatGptApiErrorKind::AuthenticationInvalid);
    assert!(!error.message.contains("Could not parse"));
    assert!(!error.message.contains("access-secret"));
    server.join().unwrap();
}

#[tokio::test]
async fn structured_json_forbidden_is_not_an_edge_challenge() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        respond(
            listener.accept().unwrap().0,
            "403 Forbidden",
            &[("Content-Type", "application/problem+json")],
            r#"{"error":"policy rejected the OAuth client"}"#,
        )
    });
    let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
    let error = api
        .exchange_code("auth-code", "pkce-verifier", "http://localhost/callback")
        .await
        .unwrap_err();
    assert_eq!(error.kind, ChatGptApiErrorKind::AuthenticationInvalid);
    server.join().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        respond(
            listener.accept().unwrap().0,
            "403 Forbidden",
            &[("Content-Type", "application/problem+json; charset=utf-8")],
            r#"{"detail":"not a member of this workspace"}"#,
        )
    });
    let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
    let tokens = OAuthTokenSet::new("access-secret", None, None).unwrap();
    let error = api.query_usage(&tokens, "workspace-123").await.unwrap_err();
    assert_eq!(error.kind, ChatGptApiErrorKind::WorkspaceAccessDenied);
    server.join().unwrap();
}

#[tokio::test]
async fn query_usage_classifies_json_forbidden_as_workspace_access_denied() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        respond(
            listener.accept().unwrap().0,
            "403 Forbidden",
            &[],
            r#"{"detail":"not a member of this workspace"}"#,
        )
    });
    let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
    let tokens = OAuthTokenSet::new("access-secret", None, None).unwrap();
    let error = api.query_usage(&tokens, "workspace-123").await.unwrap_err();
    assert_eq!(error.kind, ChatGptApiErrorKind::WorkspaceAccessDenied);
    assert!(!error.message.contains("not a member"));
    server.join().unwrap();
}

#[tokio::test]
async fn concrete_adapter_accepts_nullable_additional_rate_limits() {
    let bodies = [
        r#"{
            "rate_limit": {"allowed": true},
            "additional_rate_limits": null
        }"#,
        r#"{
            "rate_limit": {"allowed": true},
            "additional_rate_limits": [
                {"limit_name": "codex_other", "rate_limit": null}
            ]
        }"#,
    ];

    for body in bodies {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server =
            thread::spawn(move || respond(listener.accept().unwrap().0, "200 OK", &[], body));
        let api = ReqwestChatGptApi::new(test_config(&base_url)).unwrap();
        let tokens = OAuthTokenSet::new("access-secret", None, None).unwrap();
        let usage = api.query_usage(&tokens, "workspace-123").await.unwrap();
        if let Some(additional) = usage.additional_rate_limits.first() {
            assert_eq!(additional.limit_name, "codex_other");
            assert_eq!(additional.rate_limit, None);
        } else {
            assert!(usage.additional_rate_limits.is_empty());
        }
        server.join().unwrap();
    }
}
