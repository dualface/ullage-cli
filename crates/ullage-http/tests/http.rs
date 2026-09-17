use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ullage_core::ProviderRegistry;
use ullage_daemon::{ControlService, DaemonEngine, MemorySnapshotStore, SystemClock};
use ullage_http::{HttpBindConfig, HttpServer};

mod common;
use common::*;

#[tokio::test(flavor = "multi_thread")]
async fn rejects_disallowed_bind_and_names_the_setting() {
    let engine = DaemonEngine::new(
        ullage_daemon::DaemonConfig::default(),
        Arc::new(ProviderRegistry::default()),
        Arc::new(SystemClock),
        Arc::new(MemorySnapshotStore::default()),
    )
    .await
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let error = match HttpServer::bind(
        HttpBindConfig {
            binds: vec!["0.0.0.0:0".parse().unwrap()],
            allowed_origins: Vec::new(),
            probe_min_interval: Duration::from_secs(60),
            device_store_path: directory.path().join("devices.json"),
        },
        ControlService::new(engine),
    )
    .await
    {
        Ok(_) => panic!("disallowed http.bind should fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("http.bind"), "{error}");
    assert!(error.contains("loopback"), "{error}");
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn serves_two_loopback_addresses_and_whitelists_every_listener() {
    let engine = DaemonEngine::new(
        ullage_daemon::DaemonConfig::default(),
        Arc::new(ProviderRegistry::default()),
        Arc::new(SystemClock),
        Arc::new(MemorySnapshotStore::default()),
    )
    .await
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let first = SocketAddr::from(([127, 0, 0, 1], port));
    let second = SocketAddr::from(([127, 0, 0, 2], port));
    let server = HttpServer::bind(
        HttpBindConfig {
            binds: vec![first, second],
            allowed_origins: Vec::new(),
            probe_min_interval: Duration::from_secs(60),
            device_store_path: directory.path().join("devices.json"),
        },
        ControlService::new(engine.clone()),
    )
    .await
    .unwrap();
    assert_eq!(server.local_addrs(), vec![first, second]);
    let task = tokio::spawn(server.run());

    for (address, host) in [(first, second), (second, first)] {
        let response = exchange(
            address,
            &format!("GET /v1/status HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(response.status, 401);
    }
    let forbidden = exchange(
        first,
        &format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.3:{port}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(forbidden.status, 403);

    engine.shutdown();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticates_with_bearer_token_and_maps_control_errors() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let missing = get(harness.addr, "/v1/status", None, "");
    assert_eq!(missing.status, 401);
    assert_eq!(missing.body, "{\"error\":\"unauthorized\"}");
    let wrong_method_without_bearer = exchange(
        harness.addr,
        &format!(
            "POST /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            harness.addr.port()
        ),
    );
    assert_eq!(wrong_method_without_bearer.status, 401);
    let unauthenticated_body = exchange(
        harness.addr,
        &format!(
            "POST /v1/accounts/primary/probe HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            harness.addr.port(),
            1024 * 1024 + 1,
            "x".repeat(1024 * 1024 + 1)
        ),
    );
    assert_eq!(unauthenticated_body.status, 401);
    let wrong = get(harness.addr, "/v1/status", Some("wrong-token"), "");
    assert_eq!(wrong.status, 401);
    assert_eq!(wrong.body, missing.body);
    let status = get(harness.addr, "/v1/status", Some(&harness.token), "");
    assert_eq!(status.status, 200);
    assert!(status.body.contains("daemon_status"), "{}", status.body);
    assert!(
        status.body.contains("\"version\":10") || status.body.contains("\"version\": 10"),
        "{}",
        status.body
    );
    let illegal = get(
        harness.addr,
        "/v1/status?account=primary",
        Some(&harness.token),
        "",
    );
    assert_eq!(illegal.status, 400);
    assert_eq!(status.header("cache-control"), Some("no-store"));
    let unknown = get(harness.addr, "/v1/missing", Some(&harness.token), "");
    assert_eq!(unknown.status, 404);
    let missing_account = get(
        harness.addr,
        "/v1/accounts/missing",
        Some(&harness.token),
        "",
    );
    assert_eq!(missing_account.status, 404);
    let probe = post(
        harness.addr,
        "/v1/accounts/primary/probe",
        &harness.token,
        "",
    );
    assert_eq!(probe.status, 200);
    assert!(probe.body.contains("\"probe\""), "{}", probe.body);
    let auth_invalid = post(
        harness.addr,
        "/v1/accounts/primary/probe?wait=true",
        &harness.token,
        "",
    );
    assert_eq!(auth_invalid.status, 429);
    assert!(auth_invalid.header("retry-after").is_some());
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pairs_without_bearer_authenticates_and_honors_revocation() {
    let harness = Harness::start_unpaired().await;
    assert_eq!(
        get(harness.addr, "/v1/status", Some("random"), "").status,
        401
    );

    let pair_code = harness.service.create_pair_code().unwrap();
    let body = serde_json::json!({
        "pair_code": pair_code.code.to_ascii_lowercase().replace('-', ""),
        "device_name": "pro\u{0}2026",
    })
    .to_string();
    let paired = post_pair(harness.addr, &body, "application/json; charset=utf-8");
    assert_eq!(paired.status, 200, "{}", paired.body);
    let payload: serde_json::Value = serde_json::from_str(&paired.body).unwrap();
    let token = payload["device_token"].as_str().unwrap();
    let device_id = payload["device_id"].as_str().unwrap();
    assert_eq!(device_id.len(), 12);
    assert_eq!(payload["device_name"], "pro2026");
    assert_eq!(get(harness.addr, "/v1/status", Some(token), "").status, 200);
    let wrong_method = exchange(
        harness.addr,
        &format!(
            "GET /v1/pair HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            harness.addr.port()
        ),
    );
    assert_eq!(wrong_method.status, 405);
    assert_eq!(wrong_method.header("allow"), Some("POST, OPTIONS"));
    let devices = harness.service.list_devices();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "pro2026");
    let persisted =
        std::fs::read_to_string(harness._directory.path().join("devices.json")).unwrap();
    assert!(!persisted.contains(token));

    assert!(harness.service.revoke_device(device_id).unwrap());
    assert_eq!(get(harness.addr, "/v1/status", Some(token), "").status, 401);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_route_enforces_json_shape_name_limit_body_limit_and_host() {
    let harness = Harness::start_unpaired().await;
    let forbidden = exchange(
        harness.addr,
        &format!(
            "POST /v1/pair HTTP/1.1\r\nHost: evil.example:{}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
            harness.addr.port()
        ),
    );
    assert_eq!(forbidden.status, 403);
    harness.shutdown().await;

    for (body, content_type, expected) in [
        ("not-json".to_owned(), "application/json", 400),
        ("{}".to_owned(), "application/json", 400),
        (
            serde_json::json!({"pair_code":"222-222","device_name":"x".repeat(65)}).to_string(),
            "application/json",
            400,
        ),
        (
            serde_json::json!({"pair_code":"222-222","device_name":"device"}).to_string(),
            "text/plain",
            400,
        ),
        (
            format!(
                "{{\"pair_code\":\"222-222\",\"device_name\":\"{}\"}}",
                "x".repeat(4096)
            ),
            "application/json",
            413,
        ),
    ] {
        let harness = Harness::start_unpaired().await;
        let response = post_pair(harness.addr, &body, content_type);
        assert_eq!(response.status, expected, "{}", response.body);
        harness.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_route_rejects_query_parameters_without_consuming_the_code() {
    let harness = Harness::start_unpaired().await;
    let code = harness.service.create_pair_code().unwrap();
    let body = serde_json::json!({"pair_code":code.code,"device_name":"device"}).to_string();

    let illegal = post_pair_at(
        harness.addr,
        "/v1/pair?unexpected=true",
        &body,
        "application/json",
    );
    assert_eq!(illegal.status, 400, "{}", illegal.body);
    assert!(harness.service.list_devices().is_empty());

    let paired = post_pair(harness.addr, &body, "application/json");
    assert_eq!(paired.status, 200, "{}", paired.body);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_rate_limit_is_per_ip_and_sets_retry_after() {
    let harness = Harness::start_unpaired().await;
    let code = harness.service.create_pair_code().unwrap();
    let mut wrong_code = code.code.clone().into_bytes();
    wrong_code[0] = if wrong_code[0] == b'2' { b'3' } else { b'2' };
    let wrong_code = String::from_utf8(wrong_code).unwrap();
    let wrong = serde_json::json!({"pair_code":wrong_code,"device_name":"device"}).to_string();
    assert_eq!(
        post_pair(harness.addr, &wrong, "application/json").status,
        401
    );
    let valid = serde_json::json!({"pair_code":code.code,"device_name":"device"}).to_string();
    let limited = post_pair(harness.addr, &valid, "application/json");
    assert_eq!(limited.status, 429);
    assert_eq!(limited.header("retry-after"), Some("1"));
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_device_store_prevents_http_startup_and_names_the_file() {
    let engine = DaemonEngine::new(
        ullage_daemon::DaemonConfig::default(),
        Arc::new(ProviderRegistry::default()),
        Arc::new(SystemClock),
        Arc::new(MemorySnapshotStore::default()),
    )
    .await
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = directory.path().join("devices.json");
    std::fs::write(&path, b"not-json").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let error = match HttpServer::bind(
        HttpBindConfig {
            binds: vec!["127.0.0.1:0".parse().unwrap()],
            allowed_origins: Vec::new(),
            probe_min_interval: Duration::from_secs(60),
            device_store_path: path.clone(),
        },
        ControlService::new(engine),
    )
    .await
    {
        Ok(_) => panic!("corrupt device store should fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("devices.json"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn host_and_origin_checks_follow_the_whitelist() {
    let harness = Harness::start(
        vec!["https://gui.example.test".into()],
        Duration::from_secs(60),
    )
    .await;
    let bad_host = exchange(
        harness.addr,
        &format!(
            "GET /v1/status HTTP/1.1\r\nHost: evil.example:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            harness.addr.port(),
            harness.token
        ),
    );
    assert_eq!(bad_host.status, 403);
    assert!(bad_host.header("access-control-allow-origin").is_none());
    let denied_origin = get(
        harness.addr,
        "/v1/status",
        Some(&harness.token),
        "Origin: https://evil.example\r\n",
    );
    assert_eq!(denied_origin.status, 200);
    assert!(
        denied_origin
            .header("access-control-allow-origin")
            .is_none()
    );
    assert_ne!(
        denied_origin.header("access-control-allow-origin"),
        Some("*")
    );
    let allowed = get(
        harness.addr,
        "/v1/status",
        Some(&harness.token),
        "Origin: https://gui.example.test\r\n",
    );
    assert_eq!(allowed.status, 200);
    assert_eq!(
        allowed.header("access-control-allow-origin"),
        Some("https://gui.example.test")
    );
    assert_eq!(allowed.header("vary"), Some("Origin"));
    assert!(allowed.header("access-control-allow-credentials").is_none());
    let preflight = exchange(
        harness.addr,
        &format!(
            "OPTIONS /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: https://gui.example.test\r\nAccess-Control-Request-Method: GET\r\nConnection: close\r\n\r\n",
            harness.addr.port()
        ),
    );
    assert_eq!(preflight.status, 204);
    assert_eq!(
        preflight.header("access-control-allow-origin"),
        Some("https://gui.example.test")
    );
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_cooldown_returns_429_with_retry_after() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let first = post(
        harness.addr,
        "/v1/accounts/primary/probe?wait=false",
        &harness.token,
        "",
    );
    assert!(
        first.status == 202 || first.status == 200,
        "{}",
        first.status
    );
    let second = post(
        harness.addr,
        "/v1/accounts/primary/probe?wait=false",
        &harness.token,
        "",
    );
    assert_eq!(second.status, 429);
    assert!(second.header("retry-after").is_some());
    assert!(second.body.contains("rate_limited"), "{}", second.body);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_reads_snapshots_without_calling_the_provider() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let usage = get(harness.addr, "/v1/usage", Some(&harness.token), "");
    assert_eq!(usage.status, 200);
    assert!(usage.body.contains("snapshots"), "{}", usage.body);
    let accounts = get(harness.addr, "/v1/accounts", Some(&harness.token), "");
    assert_eq!(accounts.status, 200);
    assert!(accounts.body.contains("primary"), "{}", accounts.body);
    let encoded = get(
        harness.addr,
        "/v1/accounts/team%2Fa",
        Some(&harness.token),
        "",
    );
    assert_eq!(encoded.status, 200, "{}", encoded.body);
    assert!(encoded.body.contains("team/a"), "{}", encoded.body);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_bodies_are_rejected_without_taking_the_listener_down() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let body = "x".repeat(1024 * 1024 + 1);
    let oversized = exchange(
        harness.addr,
        &format!(
            "POST /v1/accounts/primary/probe HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            harness.addr.port(),
            harness.token,
            body.len()
        ),
    );
    assert_eq!(oversized.status, 413);
    let status = get(harness.addr, "/v1/status", Some(&harness.token), "");
    assert_eq!(status.status, 200);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_host_header_is_forbidden() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let response = exchange(
        harness.addr,
        "GET /v1/status HTTP/1.1\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(response.status, 403, "{}", response.body);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ipv6_loopback_host_gate_accepts_bracketed_localhost() {
    let engine = DaemonEngine::new(
        ullage_daemon::DaemonConfig::default(),
        Arc::new(ProviderRegistry::default()),
        Arc::new(SystemClock),
        Arc::new(MemorySnapshotStore::default()),
    )
    .await
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let server = HttpServer::bind(
        HttpBindConfig {
            binds: vec!["[::1]:0".parse().unwrap()],
            allowed_origins: Vec::new(),
            probe_min_interval: Duration::from_secs(60),
            device_store_path: directory.path().join("devices.json"),
        },
        ControlService::new(engine.clone()),
    )
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    let task = tokio::spawn(server.run());

    let gated = exchange(
        addr,
        &format!(
            "GET /v1/status HTTP/1.1\r\nHost: [::1]:{}\r\nConnection: close\r\n\r\n",
            addr.port()
        ),
    );
    assert_eq!(gated.status, 401, "{}", gated.body);
    let portless = exchange(
        addr,
        "GET /v1/status HTTP/1.1\r\nHost: [::1]\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(portless.status, 403);

    engine.shutdown();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn options_preflight_maps_known_unknown_and_malformed_paths() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    for (path, expected) in [
        ("/v1/pair", 204),
        ("/v1/status", 204),
        ("/v1/missing", 404),
        ("/v1/%ff", 400),
        ("/v1/%00", 400),
    ] {
        let response = exchange(
            harness.addr,
            &format!(
                "OPTIONS {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                harness.addr.port()
            ),
        );
        assert_eq!(response.status, expected, "{path}: {}", response.body);
    }
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_duplicate_and_malformed_query_parameters() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let duplicate = get(
        harness.addr,
        "/v1/usage?account=a&account=b",
        Some(&harness.token),
        "",
    );
    assert_eq!(duplicate.status, 400, "{}", duplicate.body);
    let bad_wait = post(
        harness.addr,
        "/v1/accounts/primary/probe?wait=bogus",
        &harness.token,
        "",
    );
    assert_eq!(bad_wait.status, 400, "{}", bad_wait.body);
    let bare_flag = get(
        harness.addr,
        "/v1/status?flag",
        Some(&harness.token),
        "",
    );
    assert_eq!(bare_flag.status, 400, "{}", bare_flag.body);
    let diagnose_off = get(
        harness.addr,
        "/v1/status?diagnose=0",
        Some(&harness.token),
        "",
    );
    assert_eq!(diagnose_off.status, 200, "{}", diagnose_off.body);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_cooldown_is_per_account_and_never_poisons_missing_accounts() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    for attempt in 0..2 {
        let missing = post(
            harness.addr,
            "/v1/accounts/ghost/probe?wait=false",
            &harness.token,
            "",
        );
        assert_eq!(missing.status, 404, "attempt {attempt}: {}", missing.body);
    }
    let first = post(
        harness.addr,
        "/v1/accounts/primary/probe?wait=false",
        &harness.token,
        "",
    );
    assert!(first.status == 202 || first.status == 200, "{}", first.body);
    let other = post(
        harness.addr,
        "/v1/accounts/team%2Fa/probe?wait=false",
        &harness.token,
        "",
    );
    assert!(
        other.status == 202 || other.status == 200,
        "cooldown leaked across accounts: {}",
        other.body
    );
    let limited = post(
        harness.addr,
        "/v1/accounts/primary/probe?wait=false",
        &harness.token,
        "",
    );
    assert_eq!(limited.status, 429);
    harness.shutdown().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn authenticate_storage_failure_maps_to_500() {
    use std::os::unix::fs::PermissionsExt;
    let engine = DaemonEngine::new(
        ullage_daemon::DaemonConfig::default(),
        Arc::new(ProviderRegistry::default()),
        Arc::new(SystemClock),
        Arc::new(MemorySnapshotStore::default()),
    )
    .await
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    // A device whose stored `last_seen_at` is old makes `authenticate` persist
    // the new timestamp; the read-only directory turns that into a storage
    // error instead of a silent pass.
    let store_path = directory.path().join("devices.json");
    std::fs::write(
        &store_path,
        serde_json::json!({
            "devices": [{
                "id": "ABCDEFGHJKMN",
                "name": "stale-device",
                // hex sha256 of "test-device-token"
                "token_hash": "fdc2f4194f79710d879d596f606d94f5e85f07f53b42d2b13f2e9aeb74d78c39",
                "created_at": "2020-01-01T00:00:00Z",
                "last_seen_at": "2020-01-01T00:00:00Z"
            }]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&store_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    // The store only fails on write; lock the directory before binding so the
    // open-time read still succeeds but the persist-time tmp file cannot be
    // created.
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let server = HttpServer::bind(
        HttpBindConfig {
            binds: vec!["127.0.0.1:0".parse().unwrap()],
            allowed_origins: Vec::new(),
            probe_min_interval: Duration::from_secs(60),
            device_store_path: store_path,
        },
        ControlService::new(engine.clone()),
    )
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    let task = tokio::spawn(server.run());

    let response = get(addr, "/v1/status", Some("test-device-token"), "");
    assert_eq!(response.status, 500, "{}", response.body);
    assert!(response.body.contains("storage"), "{}", response.body);

    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    engine.shutdown();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_request_body_times_out_as_408() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let response = exchange_with_timeout(
        harness.addr,
        &format!(
            "POST /v1/accounts/primary/probe HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial-body",
            harness.addr.port(),
            harness.token
        ),
        Duration::from_secs(20),
    );
    assert_eq!(response.status, 408, "{}", response.body);
    assert!(response.body.contains("request_timeout"), "{}", response.body);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chunked_body_over_the_limit_is_413() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let oversized = "x".repeat(1024 * 1024 + 1);
    let response = exchange_with_timeout(
        harness.addr,
        &format!(
            "POST /v1/accounts/primary/probe HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            harness.addr.port(),
            harness.token,
            oversized.len(),
            oversized
        ),
        Duration::from_secs(5),
    );
    assert_eq!(response.status, 413, "{}", response.body);
    let status = get(harness.addr, "/v1/status", Some(&harness.token), "");
    assert_eq!(status.status, 200);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_limit_closes_overflow_connections() {
    use std::io::Read;
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let mut held = Vec::new();
    for _ in 0..256 {
        held.push(std::net::TcpStream::connect(harness.addr).unwrap());
    }
    // Once every permit is held, the next accepted socket is closed at once.
    // Probe until that happens so the test does not depend on accept timing.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut probe = std::net::TcpStream::connect(harness.addr).unwrap();
        probe
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut byte = [0_u8; 1];
        match probe.read(&mut byte) {
            Ok(0) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                break;
            }
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "connection limit did not engage"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(held);
    harness.shutdown().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unavailable_non_loopback_bind_is_skipped() {
    let engine = DaemonEngine::new(
        ullage_daemon::DaemonConfig::default(),
        Arc::new(ProviderRegistry::default()),
        Arc::new(SystemClock),
        Arc::new(MemorySnapshotStore::default()),
    )
    .await
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let loopback = SocketAddr::from(([127, 0, 0, 1], port));
    // A private-range address no local interface holds: the bind fails and the
    // server must keep only the working listener instead of dying.
    let missing = SocketAddr::from(([10, 255, 255, 1], port));
    let service = ControlService::new(engine.clone());
    let server = HttpServer::bind(
        HttpBindConfig {
            binds: vec![loopback, missing],
            allowed_origins: Vec::new(),
            probe_min_interval: Duration::from_secs(60),
            device_store_path: directory.path().join("devices.json"),
        },
        service.clone(),
    )
    .await
    .unwrap();
    assert_eq!(server.local_addrs(), vec![loopback]);
    drop(server);

    let error = match HttpServer::bind(
        HttpBindConfig {
            binds: vec![missing],
            allowed_origins: Vec::new(),
            probe_min_interval: Duration::from_secs(60),
            device_store_path: directory.path().join("devices.json"),
        },
        service,
    )
    .await
    {
        Ok(_) => panic!("unavailable non-loopback bind should fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("could not listen"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_timeout_and_does_not_attach_diagnostics_by_default() {
    let harness = Harness::start(Vec::new(), Duration::ZERO).await;
    let _ok = post(
        harness.addr,
        "/v1/accounts/primary/probe",
        &harness.token,
        "",
    );
    let invalid = post(
        harness.addr,
        "/v1/accounts/primary/probe",
        &harness.token,
        "",
    );
    assert_eq!(invalid.status, 409, "{}", invalid.body);
    assert!(!invalid.body.contains("bad credential"), "{}", invalid.body);
    let diagnosed = post(
        harness.addr,
        "/v1/accounts/primary/probe?diagnose=1",
        &harness.token,
        "",
    );
    assert_eq!(diagnosed.status, 429, "{}", diagnosed.body);
    harness.shutdown().await;
}
