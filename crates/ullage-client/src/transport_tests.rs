use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;

use ullage_protocol::{AccountId, CredentialBackendId, ProviderId};

use super::*;

struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn request_write_failure_is_daemon_unavailable() {
    let error = write_request(
        &mut FailingWriter,
        &ControlRequest::new("write-test", ControlCommand::ListProviders),
    )
    .unwrap_err();
    assert!(matches!(error, ClientError::DaemonUnavailable));
}

#[test]
fn system_client_accepts_a_private_same_user_socket() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-transport-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let socket_path = directory.join("control.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut encoded = String::new();
        BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
        let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
        let response = ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: request.request_id,
            result: ControlResult::Ack,
            diagnostic: None,
            daemon_version: None,
        };
        serde_json::to_writer(&mut stream, &response).unwrap();
        stream.write_all(b"\n").unwrap();
    });

    let client = SystemClient {
        endpoint: Some(socket_path.clone()),
    };
    let response = client
        .send(&ControlRequest::new(
            "transport-test",
            ControlCommand::ListProviders,
        ))
        .unwrap();
    assert_eq!(response.result, ControlResult::Ack);

    server.join().unwrap();
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn readiness_rejects_stopping_and_protocol_mismatch_responses() {
    for result in [
        ControlResult::DaemonStatus(DaemonStatusPayload {
            shutting_down: true,
            accounts: Vec::new(),
            credential_backend: CredentialBackendId::native(),
        }),
        ControlResult::ProtocolMismatch {
            supported_version: CONTROL_PROTOCOL_VERSION,
        },
    ] {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-readiness-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut encoded = String::new();
            BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
            let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
            let response = ControlResponse {
                version: CONTROL_PROTOCOL_VERSION,
                request_id: request.request_id,
                result,
                diagnostic: None,
                daemon_version: None,
            };
            serde_json::to_writer(&mut stream, &response).unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };

        // `NotReady` is transient: the readiness probe reports "not ready"
        // so callers retry inside their budget instead of failing.
        assert!(matches!(
            client.daemon_is_ready(Duration::from_secs(1)),
            Ok(false)
        ));
        server.join().unwrap();
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}

#[test]
fn readiness_retries_through_not_ready_until_ready() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-readiness-retry-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let socket_path = directory.join("control.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || {
        let mut answered = 0;
        while let Ok((mut stream, _)) = listener.accept() {
            let mut encoded = String::new();
            BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
            let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
            answered += 1;
            let result = if answered < 3 {
                ControlResult::DaemonStatus(DaemonStatusPayload {
                    shutting_down: true,
                    accounts: Vec::new(),
                    credential_backend: CredentialBackendId::native(),
                })
            } else {
                ControlResult::DaemonStatus(DaemonStatusPayload {
                    shutting_down: false,
                    accounts: Vec::new(),
                    credential_backend: CredentialBackendId::native(),
                })
            };
            let response = ControlResponse {
                version: CONTROL_PROTOCOL_VERSION,
                request_id: request.request_id,
                result,
                diagnostic: None,
                daemon_version: None,
            };
            serde_json::to_writer(&mut stream, &response).unwrap();
            stream.write_all(b"\n").unwrap();
        }
    });
    let client = SystemClient {
        endpoint: Some(socket_path.clone()),
    };

    client.wait_for_service_ready().unwrap();
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_dir(directory).unwrap();
    // The accept loop stays blocked on the unlinked socket until the test
    // process exits; dropping the handle detaches it.
    drop(server);
}

#[test]
fn stop_probe_recognizes_authenticated_non_ready_daemons() {
    for result in [
        ControlResult::DaemonStatus(DaemonStatusPayload {
            shutting_down: true,
            accounts: Vec::new(),
            credential_backend: CredentialBackendId::native(),
        }),
        ControlResult::ProtocolMismatch {
            supported_version: CONTROL_PROTOCOL_VERSION,
        },
    ] {
        let response = ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: "stop-probe".into(),
            result,
            diagnostic: None,
            daemon_version: None,
        };
        assert_eq!(
            classify_readiness_response(&response, "stop-probe").unwrap(),
            DaemonReadiness::NotReady
        );
    }
}

#[test]
fn stop_waits_through_shutting_down_until_the_endpoint_disappears() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-stop-wait-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let socket_path = directory.join("control.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut encoded = String::new();
        BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
        let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
        let response = ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: request.request_id,
            result: ControlResult::DaemonStatus(DaemonStatusPayload {
                shutting_down: true,
                accounts: Vec::new(),
                credential_backend: CredentialBackendId::native(),
            }),
            diagnostic: None,
            daemon_version: None,
        };
        serde_json::to_writer(&mut stream, &response).unwrap();
        stream.write_all(b"\n").unwrap();
    });
    let client = SystemClient {
        endpoint: Some(socket_path.clone()),
    };

    client.wait_for_service_stopped().unwrap();
    server.join().unwrap();
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn readiness_read_is_bounded_by_its_budget() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-readiness-timeout-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let socket_path = directory.join("control.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut encoded = String::new();
        BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
        std::thread::sleep(Duration::from_millis(200));
    });
    let client = SystemClient {
        endpoint: Some(socket_path.clone()),
    };
    let started = std::time::Instant::now();

    assert!(matches!(
        client.daemon_is_ready(Duration::from_millis(50)),
        Ok(false)
    ));
    assert!(started.elapsed() < Duration::from_millis(150));
    server.join().unwrap();
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn readiness_slow_trickle_cannot_reset_its_budget() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-readiness-trickle-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let socket_path = directory.join("control.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut encoded = String::new();
        BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
        for _ in 0..10 {
            if stream.write_all(b"{").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let client = SystemClient {
        endpoint: Some(socket_path.clone()),
    };
    let started = std::time::Instant::now();

    assert!(matches!(
        client.daemon_is_ready(Duration::from_millis(60)),
        Ok(false)
    ));
    assert!(started.elapsed() < Duration::from_millis(150));
    server.join().unwrap();
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn default_unix_endpoint_is_scoped_to_the_effective_user() {
    let endpoint = default_unix_control_socket();
    // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
    let user_id = unsafe { libc::geteuid() };
    let expected_parent = format!("ullage-{user_id}");

    assert_eq!(endpoint.file_name().unwrap(), "control.sock");
    assert_eq!(
        endpoint.parent().unwrap().file_name().unwrap(),
        std::ffi::OsStr::new(&expected_parent)
    );
}

#[test]
fn probe_reads_are_bounded_above_the_sign_in_timeout() {
    let probe = ControlRequest::new(
        "t",
        ControlCommand::Probe {
            account_id: "claude-a".into(),
            wait: true,
        },
    );
    assert_eq!(send_timeout(&probe), PROBE_WAIT_TIMEOUT);
    assert!(PROBE_WAIT_TIMEOUT > SIGN_IN_TIMEOUT);

    let sign_in = ControlRequest::new(
        "t",
        ControlCommand::CompleteAuth {
            provider: ProviderId::new("claude"),
            account: AccountId::new("claude-a"),
            request: ullage_protocol::AuthCompleteRequest {
                flow_id: "f".into(),
                authorization_code: None,
                redirect_uri: None,
            },
        },
    );
    assert_eq!(send_timeout(&sign_in), SIGN_IN_TIMEOUT);

    let control = ControlRequest::new("t", ControlCommand::ListProviders);
    assert_eq!(send_timeout(&control), CONTROL_TIMEOUT);
}

#[test]
fn relative_socket_path_is_an_endpoint_error_not_a_protocol_error() {
    let client = SystemClient {
        endpoint: Some(PathBuf::from("relative/control.sock")),
    };
    let error = client
        .send(&ControlRequest::new("t", ControlCommand::ListProviders))
        .unwrap_err();
    assert!(matches!(error, ClientError::InvalidEndpoint));
}

#[test]
fn unsafe_permitted_socket_is_unavailable_so_readiness_retries() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-perms-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let socket_path = directory.join("control.sock");
    let _listener = UnixListener::bind(&socket_path).unwrap();
    // The state a client sees between the daemon's bind and its chmod.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o777)).unwrap();

    let client = SystemClient {
        endpoint: Some(socket_path.clone()),
    };
    let error = client
        .send(&ControlRequest::new("t", ControlCommand::ListProviders))
        .unwrap_err();
    assert!(matches!(error, ClientError::DaemonUnavailable));
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

fn daemon_script(directory: &std::path::Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(directory).unwrap();
    let script = directory.join(name);
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    script
}

fn daemon_log(directory: &std::path::Path) -> (PathBuf, std::fs::File) {
    let path = directory.join("daemon-error.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    (path, file)
}

#[test]
fn run_daemon_process_reports_stderr_tail_on_early_exit() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-daemon-early-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let script = daemon_script(
        &directory,
        "noisy-daemon",
        "#!/bin/sh\nprintf 'bind failed: address in use\\n' >&2\nexit 1\n",
    );
    let (log_path, log_sink) = daemon_log(&directory);

    // Spawning can transiently fail (EAGAIN) when the whole suite runs in
    // parallel; the behavior under test is the early-exit report.
    let mut result = run_daemon_process(
        script.clone(),
        &[],
        log_path.clone(),
        log_sink.try_clone().unwrap(),
        Duration::from_secs(5),
        |_| Ok(false),
    );
    for _ in 0..3 {
        if !matches!(result, Err(ClientError::DaemonProcess)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        result = run_daemon_process(
            script.clone(),
            &[],
            log_path.clone(),
            log_sink.try_clone().unwrap(),
            Duration::from_secs(5),
            |_| Ok(false),
        );
    }
    let error = result.unwrap_err();
    let ClientError::DaemonProcessOutput(detail) = error else {
        panic!("expected DaemonProcessOutput, got {error:?}");
    };
    assert!(detail.contains("daemon exited during startup"), "{detail}");
    assert!(detail.contains("bind failed: address in use"), "{detail}");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn run_daemon_process_reports_stderr_tail_on_readiness_timeout() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-daemon-timeout-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let script = daemon_script(
        &directory,
        "stuck-daemon",
        "#!/bin/sh\nprintf 'still loading plugins\\n' >&2\nsleep 30\n",
    );
    let (log_path, log_sink) = daemon_log(&directory);

    // Spawning can transiently fail (EAGAIN) when the whole suite runs in
    // parallel; the behavior under test is the readiness-timeout report.
    let mut result = run_daemon_process(
        script.clone(),
        &[],
        log_path.clone(),
        log_sink.try_clone().unwrap(),
        Duration::from_millis(120),
        |_| Ok(false),
    );
    for _ in 0..3 {
        if !matches!(result, Err(ClientError::DaemonProcess)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        result = run_daemon_process(
            script.clone(),
            &[],
            log_path.clone(),
            log_sink.try_clone().unwrap(),
            Duration::from_millis(120),
            |_| Ok(false),
        );
    }
    let error = result.unwrap_err();
    let ClientError::DaemonProcessOutput(detail) = error else {
        panic!("expected DaemonProcessOutput, got {error:?}");
    };
    assert!(detail.contains("did not become ready"), "{detail}");
    assert!(detail.contains("still loading plugins"), "{detail}");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn run_daemon_process_returns_once_readiness_reports_ready() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-daemon-ready-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    // The daemon must outlive the first readiness check: a shorter sleep
    // lets try_wait observe the exit first and the test flakes.
    let script = daemon_script(&directory, "daemon", "#!/bin/sh\nsleep 30\n");
    let (log_path, log_sink) = daemon_log(&directory);

    // Spawning can transiently fail (EAGAIN) when the whole suite runs in
    // parallel; the behavior under test is the readiness return.
    let mut result = run_daemon_process(
        script.clone(),
        &[],
        log_path.clone(),
        log_sink.try_clone().unwrap(),
        Duration::from_secs(5),
        |_| Ok(true),
    );
    for _ in 0..3 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        result = run_daemon_process(
            script.clone(),
            &[],
            log_path.clone(),
            log_sink.try_clone().unwrap(),
            Duration::from_secs(5),
            |_| Ok(true),
        );
    }
    result.unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn run_daemon_waits_out_a_stopping_daemon_instead_of_spawning() {
    // The endpoint serves NotReady twice, then Ready: `daemon run` must
    // wait inside its budget and never spawn a colliding child.
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-run-wait-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let socket_path = directory.join("control.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || {
        for poll in 0..3 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut encoded = String::new();
            if BufReader::new(&mut stream).read_line(&mut encoded).is_err() {
                return;
            }
            let Ok(request) = serde_json::from_str::<ControlRequest>(&encoded) else {
                return;
            };
            let response = ControlResponse {
                version: CONTROL_PROTOCOL_VERSION,
                request_id: request.request_id,
                result: ControlResult::DaemonStatus(DaemonStatusPayload {
                    shutting_down: poll < 2,
                    accounts: Vec::new(),
                    credential_backend: CredentialBackendId::native(),
                }),
                diagnostic: None,
                daemon_version: None,
            };
            if serde_json::to_writer(&mut stream, &response).is_err()
                || stream.write_all(b"\n").is_err()
            {
                return;
            }
        }
    });
    let client = SystemClient {
        endpoint: Some(socket_path.clone()),
    };

    client.run_daemon().unwrap();
    server.join().unwrap();
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn run_daemon_reports_a_daemon_that_never_frees_the_endpoint() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-run-held-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let socket_path = directory.join("control.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut encoded = String::new();
            if BufReader::new(&mut stream).read_line(&mut encoded).is_err() {
                continue;
            }
            let Ok(request) = serde_json::from_str::<ControlRequest>(&encoded) else {
                continue;
            };
            let response = ControlResponse {
                version: CONTROL_PROTOCOL_VERSION,
                request_id: request.request_id,
                result: ControlResult::DaemonStatus(DaemonStatusPayload {
                    shutting_down: true,
                    accounts: Vec::new(),
                    credential_backend: CredentialBackendId::native(),
                }),
                diagnostic: None,
                daemon_version: None,
            };
            if serde_json::to_writer(&mut stream, &response).is_err()
                || stream.write_all(b"\n").is_err()
            {
                continue;
            }
        }
    });
    let client = SystemClient {
        endpoint: Some(socket_path.clone()),
    };

    let started = std::time::Instant::now();
    let error = client.run_daemon().unwrap_err();
    assert!(
        matches!(error, ClientError::DaemonStillRunning),
        "{error:?}"
    );
    assert!(started.elapsed() >= Duration::from_secs(5));
    drop(server);
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn daemon_stop_fails_while_a_non_service_daemon_still_answers() {
    // `service::stop` must see "not installed": point the systemd unit
    // directory at an empty temp config root.
    let config_root = std::env::temp_dir().join(format!(
        "ullage-cli-stop-config-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&config_root).unwrap();
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &config_root);
    }
    let result = std::panic::catch_unwind(|| {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-stop-live-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut encoded = String::new();
                BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
                let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
                let response = ControlResponse {
                    version: CONTROL_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    result: ControlResult::DaemonStatus(DaemonStatusPayload {
                        shutting_down: false,
                        accounts: Vec::new(),
                        credential_backend: CredentialBackendId::native(),
                    }),
                    diagnostic: None,
                    daemon_version: None,
                };
                serde_json::to_writer(&mut stream, &response).unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });
        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };
        let error = client.manage_service(ServiceAction::Stop).unwrap_err();
        assert!(matches!(error, ClientError::DaemonStillRunning));

        // Once nothing answers, stop succeeds again.
        drop(server);
        std::fs::remove_file(&socket_path).unwrap();
        client.manage_service(ServiceAction::Stop).unwrap();
        std::fs::remove_dir(directory).unwrap();
    });
    unsafe {
        std::env::remove_var("XDG_CONFIG_HOME");
    }
    std::fs::remove_dir_all(config_root).unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn daemon_install_fails_while_a_non_service_daemon_still_answers() {
    // `service::stop` must see "not installed": point the systemd unit
    // directory at an empty temp config root.
    let config_root = std::env::temp_dir().join(format!(
        "ullage-cli-install-config-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&config_root).unwrap();
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &config_root);
    }
    let result = std::panic::catch_unwind(|| {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-install-live-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut encoded = String::new();
                BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
                let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
                let response = ControlResponse {
                    version: CONTROL_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    result: ControlResult::DaemonStatus(DaemonStatusPayload {
                        shutting_down: false,
                        accounts: Vec::new(),
                        credential_backend: CredentialBackendId::native(),
                    }),
                    diagnostic: None,
                    daemon_version: None,
                };
                serde_json::to_writer(&mut stream, &response).unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });
        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };
        let error = client.manage_service(ServiceAction::Install).unwrap_err();
        assert!(matches!(error, ClientError::DaemonStillRunning));

        // The foreign daemon blocks install before any manifest is written:
        // the stop guard runs ahead of `service::manage(Install)`.
        #[cfg(target_os = "linux")]
        assert!(!config_root.join("systemd/user/ullage.service").exists());

        drop(server);
        std::fs::remove_file(&socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    });
    unsafe {
        std::env::remove_var("XDG_CONFIG_HOME");
    }
    std::fs::remove_dir_all(config_root).unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn daemon_error_log_sweep_removes_only_stale_files() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-log-sweep-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let active = directory.join("daemon-error-1-0.log");
    let also_active = directory.join("daemon-error-2-0.log");
    let unrelated = directory.join("control.sock");
    for path in [&active, &also_active, &unrelated] {
        std::fs::File::create(path).unwrap();
    }

    // A sweep at launch time must leave fresh logs alone: they may belong
    // to another launcher still starting its daemon.
    sweep_daemon_error_logs(&directory);
    assert!(active.exists());
    assert!(also_active.exists());
    assert!(unrelated.exists());

    // Everything older than the cutoff is stale and removed; unrelated
    // files are never touched.
    sweep_daemon_error_logs_before(
        &directory,
        std::time::SystemTime::now() + Duration::from_secs(60),
    );
    assert!(!active.exists());
    assert!(!also_active.exists());
    assert!(unrelated.exists());

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn daemon_error_log_retries_a_fresh_name_on_collision() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-log-collision-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    // A recycled PID inside the retention window can collide with a
    // fresh log left by an earlier process; the create must retry with
    // the next name instead of failing the launch.
    let taken = directory.join("daemon-error-taken.log");
    std::fs::File::create(&taken).unwrap();

    let mut names = ["daemon-error-taken.log", "daemon-error-free.log"]
        .into_iter()
        .map(str::to_owned);
    let (path, _file) = create_daemon_error_log(&directory, || names.next().unwrap()).unwrap();
    assert_eq!(path, directory.join("daemon-error-free.log"));

    // Exhausting every generated name surfaces DaemonProcess rather than
    // truncating or blocking on someone else's file.
    let mut stuck = || "daemon-error-taken.log".to_owned();
    let result = create_daemon_error_log(&directory, &mut stuck);
    assert!(matches!(result, Err(ClientError::DaemonProcess)));

    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn daemon_log_directory_rejects_symlinks_and_foreign_dirs() {
    use std::os::unix::fs::PermissionsExt;

    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-logdir-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();

    // A world-writable parent lets a neighbour pre-create the log dir;
    // loose permissions must fail the launch instead of being adopted.
    let loose = directory.join("loose");
    std::fs::create_dir(&loose).unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        ensure_daemon_log_directory(&loose),
        Err(ClientError::DaemonProcess)
    ));

    // A symlink must never be followed into attacker-chosen territory.
    let link = directory.join("link");
    std::os::unix::fs::symlink(&loose, &link).unwrap();
    assert!(matches!(
        ensure_daemon_log_directory(&link),
        Err(ClientError::DaemonProcess)
    ));

    // A missing path is created fresh with private permissions.
    let fresh = directory.join("fresh");
    ensure_daemon_log_directory(&fresh).unwrap();
    let metadata = std::fs::symlink_metadata(&fresh).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o700);

    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn read_log_tail_does_not_follow_a_symlink() {
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-logtail-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let secret = directory.join("secret.txt");
    std::fs::write(&secret, b"do-not-leak").unwrap();
    let link = directory.join("daemon-error-swapped.log");
    std::os::unix::fs::symlink(&secret, &link).unwrap();

    // The name was swapped between create and read-back; the tail must
    // come out empty rather than streaming the link target.
    assert!(read_log_tail(&link).is_empty());
    assert_eq!(read_log_tail(&secret), b"do-not-leak");

    std::fs::remove_dir_all(directory).unwrap();
}
