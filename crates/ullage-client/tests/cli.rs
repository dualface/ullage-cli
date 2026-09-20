use std::sync::Mutex;

use chrono::{TimeZone, Utc};
use ullage_cli::{ClientError, ControlClient, ExitCode, OutputFormat, run_from, run_from_with};
use ullage_protocol::{
    Account, AccountError, AccountId, AccountStatusPayload, AuthChallenge, AuthMethod,
    CONTROL_PROTOCOL_VERSION, ControlCommand, ControlError, ControlRequest, ControlResponse,
    ControlResult, CredentialBackendId, DaemonStatusPayload, DevicePayload, MeasurementUnit,
    PairCodePayload, PartialFailure, ProbePayload, ProviderDescriptor, ProviderError, ProviderId,
    QueryOutcome, SanitizedErrorPayload, SnapshotPayload, UsageMeasurement, UsageWindow,
    UsageWindowKind,
};

mod common;
use common::*;

#[test]
fn maps_the_complete_command_surface_to_control_requests() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let result = match &request.command {
            ControlCommand::DaemonStatus => ControlResult::DaemonStatus(DaemonStatusPayload {
                shutting_down: false,
                accounts: Vec::new(),
                credential_backend: CredentialBackendId::LinuxSecretService,
            }),
            ControlCommand::ListProviders => ControlResult::Providers(Vec::new()),
            ControlCommand::AddAccount { provider, label } => ControlResult::Account(Account {
                id: AccountId::new("account-1"),
                provider: provider.clone(),
                label: label.clone(),
                enabled: true,
                metrics: Vec::new(),
            }),
            ControlCommand::ListAccounts => ControlResult::Accounts(Vec::new()),
            ControlCommand::ShowAccount { account } => ControlResult::Account(Account {
                id: account.clone(),
                provider: ProviderId::new("claude"),
                label: None,
                enabled: true,
                metrics: Vec::new(),
            }),
            ControlCommand::SetAccountEnabled { account, enabled } => {
                ControlResult::Account(Account {
                    id: account.clone(),
                    provider: ProviderId::new("claude"),
                    label: None,
                    enabled: *enabled,
                    metrics: Vec::new(),
                })
            }
            ControlCommand::SetAccountLabel { account, label } => ControlResult::Account(Account {
                id: account.clone(),
                provider: ProviderId::new("claude"),
                label: label.clone(),
                enabled: true,
                metrics: Vec::new(),
            }),
            ControlCommand::SetAccountMetrics { account, metrics } => {
                ControlResult::Account(Account {
                    id: account.clone(),
                    provider: ProviderId::new("claude"),
                    label: None,
                    enabled: true,
                    metrics: metrics.clone(),
                })
            }
            ControlCommand::RemoveAccount { .. } | ControlCommand::Logout { .. } => {
                ControlResult::Ack
            }
            ControlCommand::StartAuth { .. } => ControlResult::AuthChallenge(AuthChallenge {
                flow_id: "flow-secret".into(),
                method: AuthMethod::BrowserOAuth,
                verification_uri: Some("https://example.test/login".into()),
                user_code: Some("secret-code".into()),
                expires_at: None,
                input: None,
            }),
            ControlCommand::AuthStatus { .. } | ControlCommand::CompleteAuth { .. } => {
                ControlResult::AuthState(ullage_protocol::AuthState::NotAuthenticated)
            }
            ControlCommand::RetireDuplicateAccounts { .. } => ControlResult::Accounts(Vec::new()),
            ControlCommand::ListWorkspaces { .. } | ControlCommand::SelectWorkspace { .. } => {
                unreachable!("workspace controls have no CLI command")
            }
            ControlCommand::Probe {
                account_id,
                wait: true,
            } => ControlResult::Probe(ProbePayload {
                account_id: account_id.clone(),
                usage: QueryOutcome::Complete {
                    data: empty_usage(),
                },
                metrics: Vec::new(),
            }),
            ControlCommand::Probe { wait: false, .. } => ControlResult::Ack,
            ControlCommand::Show { .. } => ControlResult::Snapshots(Vec::new()),
            ControlCommand::CreatePairCode => ControlResult::PairCode(PairCodePayload {
                code: "ABC-DEF".into(),
                expires_at: Utc.with_ymd_and_hms(2026, 9, 1, 12, 5, 0).unwrap(),
            }),
            ControlCommand::ListDevices => ControlResult::Devices(Vec::new()),
            ControlCommand::RevokeDevice { .. } => ControlResult::Ack,
            ControlCommand::QueryUsage { .. } => unreachable!(),
        };
        Ok(response(request, result))
    }

    let client = MockClient::new(responder);
    let commands: &[&[&str]] = &[
        &["ullage", "daemon", "status"],
        &["ullage", "provider", "list"],
        &["ullage", "account", "add", "claude", "--label", "work"],
        &["ullage", "account", "list"],
        &["ullage", "account", "show", "account-1"],
        &["ullage", "account", "enable", "account-1"],
        &["ullage", "account", "disable", "account-1"],
        &["ullage", "account", "label", "account-1", "work"],
        &["ullage", "account", "label", "account-1"],
        &["ullage", "account", "remove", "account-1"],
        &["ullage", "device", "pair"],
        &["ullage", "device", "list"],
        &["ullage", "device", "revoke", "DEVICE123456"],
        &[
            "ullage",
            "auth",
            "login",
            "claude",
            "--account",
            "account-1",
            "--method",
            "browser-oauth",
        ],
        &[
            "ullage",
            "auth",
            "complete",
            "claude",
            "--account",
            "account-1",
            "flow-secret",
            "--redirect-uri",
            "https://platform.claude.com/oauth/code/callback",
        ],
        &[
            "ullage",
            "auth",
            "status",
            "claude",
            "--account",
            "account-1",
        ],
        &[
            "ullage",
            "auth",
            "logout",
            "claude",
            "--account",
            "account-1",
        ],
        &["ullage", "probe", "account-1"],
        &["ullage", "show", "account-1"],
        &["ullage", "show", "--all"],
    ];

    for command in commands {
        let output = run_from(command.iter().copied(), &client);
        assert_eq!(
            output.code,
            ExitCode::Success,
            "{command:?}: {}",
            output.stderr
        );
    }

    let requests = client.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| matches!(request, ControlCommand::Probe { wait: true, .. }))
    );
    assert!(requests.iter().any(|request| matches!(
        request,
        ControlCommand::Show {
            account_id: Some(_)
        }
    )));
    assert!(
        requests
            .iter()
            .any(|request| matches!(request, ControlCommand::Show { account_id: None }))
    );
}

#[test]
fn warns_when_the_running_daemon_is_older_than_the_cli() {
    fn older_daemon(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut response = response(request, ControlResult::Providers(Vec::new()));
        response.daemon_version = Some("0.0.1".into());
        Ok(response)
    }

    let output = run_from(
        ["ullage", "provider", "list"],
        &MockClient::new(older_daemon),
    );
    assert_eq!(output.code, ExitCode::Success);
    assert!(output.stderr.contains("warning"), "{}", output.stderr);
    assert!(output.stderr.contains("0.0.1"), "{}", output.stderr);
    assert!(
        output.stderr.contains("daemon install"),
        "{}",
        output.stderr
    );
}

#[test]
fn warns_when_the_daemon_predates_version_reporting() {
    fn legacy_daemon(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut response = response(request, ControlResult::Providers(Vec::new()));
        response.daemon_version = None;
        Ok(response)
    }

    let output = run_from(
        ["ullage", "provider", "list"],
        &MockClient::new(legacy_daemon),
    );
    assert_eq!(output.code, ExitCode::Success);
    assert!(output.stderr.contains("warning"), "{}", output.stderr);
    assert!(
        output.stderr.contains("daemon install"),
        "{}",
        output.stderr
    );
}

#[test]
fn stays_silent_when_the_daemon_matches_or_exceeds_the_cli() {
    fn current_daemon(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(request, ControlResult::Providers(Vec::new())))
    }

    fn newer_daemon(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut response = response(request, ControlResult::Providers(Vec::new()));
        response.daemon_version = Some("999.0.0".into());
        Ok(response)
    }

    for client in [
        MockClient::new(current_daemon),
        MockClient::new(newer_daemon),
    ] {
        let output = run_from(["ullage", "provider", "list"], &client);
        assert_eq!(output.code, ExitCode::Success);
        assert!(!output.stderr.contains("warning"), "{}", output.stderr);
    }
}

#[test]
fn probe_can_trigger_without_waiting() {
    let client = MockClient::new(|request| Ok(response(request, ControlResult::Ack)));
    let output = run_from(["ullage", "probe", "primary", "--no-wait"], &client);

    assert_eq!(output.code, ExitCode::Success);
    assert!(matches!(
        client.requests.lock().unwrap()[0],
        ControlCommand::Probe { wait: false, .. }
    ));
}

#[test]
fn compact_json_is_a_stable_snapshot() {
    let client = MockClient::new(snapshot_result);
    let output = run_from(["ullage", "--output", "json", "show", "primary"], &client);

    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(
        output.stdout,
        concat!(
            "{\"result\":\"snapshots\",\"payload\":[",
            "{\"account_id\":\"primary\",\"usage\":{\"outcome\":\"complete\",\"data\":{",
            "\"provider\":\"claude\",",
            "\"account_label\":\"[redacted]\",\"plan\":\"pro\",",
            "\"subscription_expires_at\":null,\"observed_at\":\"2026-08-27T12:00:00Z\",",
            "\"windows\":[]}},\"last_success_at\":\"2026-08-27T12:00:00Z\",",
            "\"stale\":false,\"last_error\":null,\"last_error_at\":null,",
            "\"metrics\":[]}]}\n"
        )
    );
}

#[test]
fn pretty_json_is_multiline_and_round_trips() {
    let client = MockClient::new(snapshot_result);
    let output = run_from(
        ["ullage", "--output", "pretty-json", "show", "--all"],
        &client,
    );

    assert_eq!(output.code, ExitCode::Success);
    assert!(output.stdout.contains("\n  \"result\": \"snapshots\""));
    serde_json::from_str::<ControlResult>(&output.stdout).unwrap();
}

#[test]
fn partial_success_has_data_and_a_distinct_exit_code() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Partial {
                    data: empty_usage(),
                    failures: vec![PartialFailure {
                        scope: "secret-scope".into(),
                        message: "sensitive detail".into(),
                    }],
                },
            )]),
        ))
    }
    let raw = run_from(
        ["ullage", "--raw", "show", "--all"],
        &MockClient::new(responder),
    );

    assert_eq!(raw.code, ExitCode::Partial);
    assert!(raw.stdout.contains("1 partial result(s)"), "{}", raw.stdout);
    assert!(!raw.stdout.contains("sensitive detail"));

    let output = run_from(["ullage", "show", "--all"], &MockClient::new(responder));

    assert_eq!(output.code, ExitCode::Partial);
    assert!(
        output.stdout.contains("! 1 item(s) unavailable"),
        "{}",
        output.stdout
    );
    assert!(!output.stdout.contains("sensitive detail"));

    for format in ["json", "pretty-json"] {
        let output = run_from(
            ["ullage", "--output", format, "show", "--all"],
            &MockClient::new(responder),
        );
        assert_eq!(output.code, ExitCode::Partial);
        assert!(!output.stdout.contains("person@example.test"));
        assert!(!output.stdout.contains("secret-scope"));
        assert!(!output.stdout.contains("sensitive detail"));
        assert!(output.stdout.contains("[redacted]"));
    }
}

#[test]
fn rejects_partial_results_without_failures() {
    fn empty_failures(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Partial {
                    data: empty_usage(),
                    failures: Vec::new(),
                },
            )]),
        ))
    }

    let output = run_from(
        ["ullage", "show", "--all"],
        &MockClient::new(empty_failures),
    );
    assert_eq!(output.code, ExitCode::ProtocolError);
    assert!(output.stderr.contains("invalid_daemon_response"));
}

#[test]
fn maps_auth_network_and_daemon_failures_to_stable_exit_codes() {
    fn auth(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Provider(
                ProviderError::AuthenticationInvalid {
                    message: "/private/user/path".into(),
                },
            )),
        ))
    }
    fn network(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Provider(ProviderError::Network {
                message: "person@example.test".into(),
            })),
        ))
    }
    fn unavailable(_: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Err(ClientError::DaemonUnavailable)
    }

    let auth_output = run_from(
        ["ullage", "--output", "json", "probe", "primary"],
        &MockClient::new(auth),
    );
    let network_output = run_from(
        ["ullage", "--output", "json", "probe", "primary"],
        &MockClient::new(network),
    );
    let daemon_output = run_from(
        ["ullage", "--output", "json", "show", "primary"],
        &MockClient::new(unavailable),
    );

    assert_eq!(auth_output.code, ExitCode::AuthenticationInvalid);
    assert_eq!(network_output.code, ExitCode::NetworkFailure);
    assert_eq!(daemon_output.code, ExitCode::DaemonUnavailable);
    assert_eq!(
        auth_output.stderr,
        "{\"status\":\"error\",\"error\":{\"kind\":\"authentication_invalid\"}}\n"
    );
    assert!(!auth_output.stderr.contains("/private"));
    assert!(!network_output.stderr.contains('@'));
}

#[test]
fn rejects_business_results_the_daemon_cannot_produce() {
    fn disabled_add(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let ControlCommand::AddAccount { provider, label } = &request.command else {
            unreachable!();
        };
        Ok(response(
            request,
            ControlResult::Account(Account {
                id: AccountId::new("account-1"),
                provider: provider.clone(),
                label: label.clone(),
                enabled: false,
                metrics: Vec::new(),
            }),
        ))
    }
    fn account_error(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Account(AccountError::NotFound(
                AccountId::new("primary"),
            ))),
        ))
    }
    fn provider_error(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Provider(ProviderError::Network {
                message: "redacted by the CLI".into(),
            })),
        ))
    }
    fn registry_error(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Registry(
                ullage_protocol::RegistryError::NotFound(ProviderId::new("claude")),
            )),
        ))
    }
    fn timeout_error(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Timeout),
        ))
    }
    fn storage_error(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Storage),
        ))
    }
    fn bare_usage(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Usage(QueryOutcome::Complete {
                data: empty_usage(),
            }),
        ))
    }

    let cases: &[(Responder, &[&str])] = &[
        (disabled_add, &["ullage", "account", "add", "claude"]),
        (account_error, &["ullage", "probe", "primary"]),
        (account_error, &["ullage", "show", "primary"]),
        (provider_error, &["ullage", "account", "add", "claude"]),
        (provider_error, &["ullage", "show", "primary"]),
        (provider_error, &["ullage", "probe", "primary", "--no-wait"]),
        (registry_error, &["ullage", "probe", "primary", "--no-wait"]),
        (timeout_error, &["ullage", "probe", "primary", "--no-wait"]),
        (storage_error, &["ullage", "probe", "primary", "--no-wait"]),
        (bare_usage, &["ullage", "probe", "primary"]),
    ];
    for (responder, arguments) in cases {
        let output = run_from(arguments.iter().copied(), &MockClient::new(*responder));
        assert_eq!(output.code, ExitCode::ProtocolError, "{arguments:?}");
        assert!(output.stderr.contains("invalid_daemon_response"));
    }
}

#[test]
fn account_mutation_storage_errors_keep_the_storage_contract() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Storage),
        ))
    }
    let client = MockClient::new(responder);
    for command in [
        vec!["ullage", "--output", "json", "account", "add", "cursor"],
        vec![
            "ullage",
            "--output",
            "json",
            "account",
            "enable",
            "account-1",
        ],
        vec![
            "ullage",
            "--output",
            "json",
            "account",
            "disable",
            "account-1",
        ],
    ] {
        let output = run_from(command, &client);
        assert_eq!(output.code, ExitCode::Failure);
        assert!(output.stderr.contains("\"kind\":\"storage\""));
    }
}

#[test]
fn invalid_auth_status_has_a_distinct_exit_code_and_redacted_reason() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::AuthState(ullage_protocol::AuthState::Invalid {
                reason: "person@example.test secret-token".into(),
                account_key: None,
            }),
        ))
    }

    let output = run_from(
        [
            "ullage",
            "--output",
            "json",
            "auth",
            "status",
            "claude",
            "--account",
            "primary",
        ],
        &MockClient::new(responder),
    );

    assert_eq!(output.code, ExitCode::AuthenticationInvalid);
    assert!(output.stdout.contains("[redacted]"));
    assert!(!output.stdout.contains("person@example.test"));
    assert!(!output.stdout.contains("secret-token"));
}

#[test]
fn diagnose_surfaces_invalid_auth_state_detail() {
    const DETAIL: &str = "oauth exchange 400: invalid_grant";

    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::AuthState(ullage_protocol::AuthState::Invalid {
                reason: DETAIL.into(),
                account_key: None,
            }),
        ))
    }

    let json = run_from(
        [
            "ullage",
            "--diagnose",
            "--output",
            "json",
            "auth",
            "status",
            "claude",
            "--account",
            "primary",
        ],
        &MockClient::new(responder),
    );
    assert_eq!(json.code, ExitCode::AuthenticationInvalid);
    assert!(json.stdout.contains(DETAIL), "{}", json.stdout);

    let table = run_from(
        [
            "ullage",
            "--diagnose",
            "auth",
            "status",
            "claude",
            "--account",
            "primary",
        ],
        &MockClient::new(responder),
    );
    assert_eq!(table.code, ExitCode::AuthenticationInvalid);
    assert!(table.stderr.contains(DETAIL), "{}", table.stderr);
}

#[test]
fn errors_honor_the_selected_output_format() {
    fn unavailable(_: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Err(ClientError::DaemonUnavailable)
    }
    let table = run_from(["ullage", "show", "primary"], &MockClient::new(unavailable));
    let pretty = run_from(
        ["ullage", "--output", "pretty-json", "show", "primary"],
        &MockClient::new(unavailable),
    );
    let usage = run_from(
        ["ullage", "--output", "json", "show"],
        &MockClient::new(unavailable),
    );

    assert!(table.stderr.contains("error: daemon_unavailable\n"));
    assert!(table.stderr.contains("hint: start the daemon"));
    assert!(pretty.stderr.contains("\n  \"status\": \"error\""));
    assert!(pretty.stderr.contains("\"hint\":"));
    assert!(usage.stderr.contains("\"kind\":\"usage\""));
    assert!(usage.stderr.contains("\"message\":"));
    assert!(!usage.stderr.contains("\"hint\":"));
}

#[test]
fn rejects_mismatched_and_forged_responses() {
    fn wrong_id(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut response = response(request, ControlResult::Ack);
        response.request_id = "other".into();
        Ok(response)
    }
    fn wrong_variant(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(request, ControlResult::Ack))
    }

    for client in [MockClient::new(wrong_id), MockClient::new(wrong_variant)] {
        let output = run_from(["ullage", "show", "primary"], &client);
        assert_eq!(output.code, ExitCode::ProtocolError);
        assert!(output.stderr.contains("invalid_daemon_response"));
    }
}

#[test]
fn unexpected_diagnostics_are_rejected_and_opt_in_is_honoured() {
    const DETAIL: &str = "oauth exchange 400: invalid_grant";

    fn auth_error() -> ControlResult {
        ControlResult::Error(ControlError::Provider(
            ProviderError::AuthenticationInvalid {
                message: "provider authentication is invalid".into(),
            },
        ))
    }

    fn with_detail(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut response = response(request, auth_error());
        response.diagnostic = Some(DETAIL.into());
        Ok(response)
    }
    fn diagnostic_on_success(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut response = response(
            request,
            ControlResult::AuthState(ullage_protocol::AuthState::Authenticated {
                account_label: None,
                expires_at: None,
                account_key: None,
            }),
        );
        response.diagnostic = Some(DETAIL.into());
        Ok(response)
    }

    let rejected = run_from(
        ["ullage", "auth", "status", "claude", "--account", "primary"],
        &MockClient::new(with_detail),
    );
    assert_eq!(rejected.code, ExitCode::ProtocolError);
    assert!(rejected.stderr.contains("invalid_daemon_response"));
    assert!(!rejected.stderr.contains(DETAIL));

    let shown = run_from(
        [
            "ullage",
            "--diagnose",
            "auth",
            "status",
            "claude",
            "--account",
            "primary",
        ],
        &MockClient::new(with_detail),
    );
    assert_eq!(shown.code, ExitCode::AuthenticationInvalid);
    assert!(
        shown.stderr.contains("authentication_invalid"),
        "{}",
        shown.stderr
    );
    assert!(shown.stderr.contains(DETAIL), "{}", shown.stderr);

    fn leak_on_show(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        assert!(request.diagnostics);
        let mut response = response(
            request,
            ControlResult::Error(ControlError::AccountNotFound {
                account_id: "primary".into(),
            }),
        );
        response.diagnostic = Some(DETAIL.into());
        Ok(response)
    }
    let show = run_from(
        ["ullage", "--diagnose", "show", "primary"],
        &MockClient::new(leak_on_show),
    );
    assert_eq!(show.code, ExitCode::ProtocolError);
    assert!(show.stderr.contains("invalid_daemon_response"));
    assert!(!show.stderr.contains(DETAIL));

    fn leak_on_snapshot(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut response = response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete {
                    data: empty_usage(),
                },
            )]),
        );
        response.diagnostic = Some(DETAIL.into());
        Ok(response)
    }
    let snapshot = run_from(
        ["ullage", "--diagnose", "show", "primary"],
        &MockClient::new(leak_on_snapshot),
    );
    assert_eq!(snapshot.code, ExitCode::ProtocolError);
    assert!(snapshot.stderr.contains("invalid_daemon_response"));
    assert!(!snapshot.stderr.contains(DETAIL));

    let success = run_from(
        [
            "ullage",
            "--diagnose",
            "auth",
            "status",
            "claude",
            "--account",
            "primary",
        ],
        &MockClient::new(diagnostic_on_success),
    );
    assert_eq!(success.code, ExitCode::ProtocolError);
    assert!(success.stderr.contains("invalid_daemon_response"));
    assert!(!success.stderr.contains(DETAIL));
}

#[test]
fn diagnose_surfaces_probe_provider_error_detail() {
    const DETAIL: &str = "oauth exchange 400: invalid_grant";

    fn probe_error() -> ControlResult {
        ControlResult::Error(ControlError::Provider(
            ProviderError::AuthenticationInvalid {
                message: "provider authentication is invalid".into(),
            },
        ))
    }

    fn with_detail(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        assert!(request.diagnostics);
        let mut response = response(request, probe_error());
        response.diagnostic = Some(DETAIL.into());
        Ok(response)
    }
    fn without_detail(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        assert!(!request.diagnostics);
        Ok(response(request, probe_error()))
    }
    fn unexpected_detail(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        assert!(!request.diagnostics);
        let mut response = response(request, probe_error());
        response.diagnostic = Some(DETAIL.into());
        Ok(response)
    }

    let table = run_from(
        ["ullage", "--diagnose", "probe", "primary"],
        &MockClient::new(with_detail),
    );
    assert_eq!(table.code, ExitCode::AuthenticationInvalid);
    assert_eq!(
        table.stderr,
        format!("error: authentication_invalid\ndetail: {DETAIL}\n")
    );

    let json = run_from(
        [
            "ullage",
            "--diagnose",
            "--output",
            "json",
            "probe",
            "primary",
        ],
        &MockClient::new(with_detail),
    );
    assert_eq!(json.code, ExitCode::AuthenticationInvalid);
    assert!(json.stderr.contains("\"kind\":\"authentication_invalid\""));
    assert!(
        json.stderr.contains(&format!("\"detail\":\"{DETAIL}\"")),
        "{}",
        json.stderr
    );

    let hidden = run_from(
        ["ullage", "probe", "primary"],
        &MockClient::new(without_detail),
    );
    assert_eq!(hidden.code, ExitCode::AuthenticationInvalid);
    assert_eq!(hidden.stderr, "error: authentication_invalid\n");
    assert!(!hidden.stderr.contains(DETAIL));

    let rejected = run_from(
        ["ullage", "probe", "primary"],
        &MockClient::new(unexpected_detail),
    );
    assert_eq!(rejected.code, ExitCode::ProtocolError);
    assert!(rejected.stderr.contains("invalid_daemon_response"));
    assert!(!rejected.stderr.contains(DETAIL));
}

#[test]
fn unknown_accounts_have_a_stable_error() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let error = match &request.command {
            ControlCommand::Probe { .. } | ControlCommand::Show { .. } => {
                ControlError::AccountNotFound {
                    account_id: "missing".into(),
                }
            }
            _ => ControlError::Account(AccountError::NotFound(AccountId::new("missing"))),
        };
        Ok(response(request, ControlResult::Error(error)))
    }
    let client = MockClient::new(responder);
    let commands: &[&[&str]] = &[
        &["ullage", "--output", "json", "account", "show", "missing"],
        &["ullage", "--output", "json", "account", "enable", "missing"],
        &[
            "ullage", "--output", "json", "account", "disable", "missing",
        ],
        &["ullage", "--output", "json", "probe", "missing"],
        &[
            "ullage",
            "--output",
            "json",
            "probe",
            "missing",
            "--no-wait",
        ],
        &["ullage", "--output", "json", "show", "missing"],
    ];

    for command in commands {
        let output = run_from(command.iter().copied(), &client);
        assert_eq!(output.code, ExitCode::Failure);
        assert!(output.stderr.contains("\"kind\":\"account_not_found\""));
    }
}

#[test]
fn rejects_results_for_a_different_account_or_provider() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let result = match &request.command {
            ControlCommand::AddAccount { .. }
            | ControlCommand::ShowAccount { .. }
            | ControlCommand::SetAccountEnabled { .. } => ControlResult::Account(Account {
                id: AccountId::new("other"),
                provider: ProviderId::new("other-provider"),
                label: None,
                enabled: true,
                metrics: Vec::new(),
            }),
            ControlCommand::Probe { .. } => ControlResult::Probe(ProbePayload {
                account_id: "other".into(),
                usage: QueryOutcome::Complete {
                    data: empty_usage(),
                },
                metrics: Vec::new(),
            }),
            ControlCommand::Show { .. } => ControlResult::Snapshots(vec![snapshot(
                "other",
                QueryOutcome::Complete {
                    data: empty_usage(),
                },
            )]),
            _ => unreachable!(),
        };
        Ok(response(request, result))
    }
    let client = MockClient::new(responder);
    let commands: &[&[&str]] = &[
        &["ullage", "account", "add", "claude"],
        &["ullage", "account", "show", "primary"],
        &["ullage", "account", "enable", "primary"],
        &["ullage", "account", "disable", "primary"],
        &["ullage", "probe", "primary"],
        &["ullage", "show", "primary"],
    ];

    for command in commands {
        let output = run_from(command.iter().copied(), &client);
        assert_eq!(output.code, ExitCode::ProtocolError, "{command:?}");
    }
}

#[test]
fn rejects_an_added_account_with_a_different_label() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let ControlCommand::AddAccount { provider, .. } = &request.command else {
            unreachable!();
        };
        Ok(response(
            request,
            ControlResult::Account(Account {
                id: AccountId::new("primary"),
                provider: provider.clone(),
                label: Some("personal".into()),
                enabled: true,
                metrics: Vec::new(),
            }),
        ))
    }

    let output = run_from(
        ["ullage", "account", "add", "claude", "--label", "work"],
        &MockClient::new(responder),
    );

    assert_eq!(output.code, ExitCode::ProtocolError);
    assert!(output.stderr.contains("invalid_daemon_response"));
}

#[test]
fn rejects_duplicate_snapshots_for_single_and_all_queries() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let snapshot = snapshot(
            "primary",
            QueryOutcome::Complete {
                data: empty_usage(),
            },
        );
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot.clone(), snapshot]),
        ))
    }

    for command in [
        &["ullage", "show", "primary"][..],
        &["ullage", "show", "--all"][..],
    ] {
        let output = run_from(command.iter().copied(), &MockClient::new(responder));
        assert_eq!(output.code, ExitCode::ProtocolError);
        assert!(output.stderr.contains("invalid_daemon_response"));
    }
}

#[test]
fn rejects_duplicate_ids_in_list_and_daemon_status_results() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let result = match &request.command {
            ControlCommand::ListProviders => {
                let provider = ProviderDescriptor {
                    id: ProviderId::new("claude"),
                    display_name: "Claude".into(),
                    capabilities: Vec::new(),
                };
                ControlResult::Providers(vec![provider.clone(), provider])
            }
            ControlCommand::ListAccounts => {
                let account = Account {
                    id: AccountId::new("primary"),
                    provider: ProviderId::new("claude"),
                    label: None,
                    enabled: true,
                    metrics: Vec::new(),
                };
                ControlResult::Accounts(vec![account.clone(), account])
            }
            ControlCommand::DaemonStatus => {
                let account = AccountStatusPayload {
                    account_id: "primary".into(),
                    provider: ProviderId::new("claude"),
                    enabled: true,
                    in_flight: false,
                    consecutive_failures: 0,
                    next_probe_at: None,
                    has_snapshot: false,
                    stale: false,
                    last_error: None,
                };
                ControlResult::DaemonStatus(DaemonStatusPayload {
                    shutting_down: false,
                    accounts: vec![account.clone(), account],
                    credential_backend: CredentialBackendId::LinuxSecretService,
                })
            }
            _ => unreachable!(),
        };
        Ok(response(request, result))
    }

    for command in [
        &["ullage", "provider", "list"][..],
        &["ullage", "account", "list"][..],
        &["ullage", "daemon", "status"][..],
    ] {
        let output = run_from(command.iter().copied(), &MockClient::new(responder));
        assert_eq!(output.code, ExitCode::ProtocolError);
        assert!(output.stderr.contains("invalid_daemon_response"));
    }
}

#[test]
fn rejects_impossible_snapshot_and_status_state_combinations() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let result = match &request.command {
            ControlCommand::DaemonStatus => ControlResult::DaemonStatus(DaemonStatusPayload {
                shutting_down: false,
                accounts: vec![AccountStatusPayload {
                    account_id: "primary".into(),
                    provider: ProviderId::new("claude"),
                    enabled: true,
                    in_flight: false,
                    consecutive_failures: 1,
                    next_probe_at: None,
                    has_snapshot: false,
                    stale: true,
                    last_error: Some(SanitizedErrorPayload::Network),
                }],
                credential_backend: CredentialBackendId::LinuxSecretService,
            }),
            ControlCommand::Show {
                account_id: Some(_),
            } => ControlResult::Snapshots(vec![SnapshotPayload {
                last_error: Some(SanitizedErrorPayload::Network),
                last_error_at: Some(Utc.with_ymd_and_hms(2026, 8, 27, 12, 1, 0).unwrap()),
                metrics: Vec::new(),
                ..snapshot(
                    "primary",
                    QueryOutcome::Complete {
                        data: empty_usage(),
                    },
                )
            }]),
            ControlCommand::Show { account_id: None } => {
                ControlResult::Snapshots(vec![SnapshotPayload {
                    stale: true,
                    last_error: Some(SanitizedErrorPayload::Network),
                    last_error_at: None,
                    metrics: Vec::new(),
                    ..snapshot(
                        "primary",
                        QueryOutcome::Complete {
                            data: empty_usage(),
                        },
                    )
                }])
            }
            _ => unreachable!(),
        };
        Ok(response(request, result))
    }

    for command in [
        &["ullage", "daemon", "status"][..],
        &["ullage", "show", "primary"][..],
        &["ullage", "show", "--all"][..],
    ] {
        let output = run_from(command.iter().copied(), &MockClient::new(responder));
        assert_eq!(output.code, ExitCode::ProtocolError);
        assert!(output.stderr.contains("invalid_daemon_response"));
    }
}

#[test]
fn rejects_terminal_controls_in_daemon_results_for_human_and_json_output() {
    fn bidi(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Providers(vec![ProviderDescriptor {
                id: ProviderId::new("claude"),
                display_name: "safe\u{202e}txt".into(),
                capabilities: Vec::new(),
            }]),
        ))
    }
    fn c1(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Providers(vec![ProviderDescriptor {
                id: ProviderId::new("claude"),
                display_name: "safe\u{009b}txt".into(),
                capabilities: Vec::new(),
            }]),
        ))
    }

    let cases: &[(Responder, &[&str], char)] = &[
        (bidi, &["ullage", "provider", "list"], '\u{202e}'),
        (
            bidi,
            &["ullage", "--output", "pretty-json", "provider", "list"],
            '\u{202e}',
        ),
        (
            c1,
            &["ullage", "--output", "json", "provider", "list"],
            '\u{009b}',
        ),
        (
            c1,
            &["ullage", "--output", "pretty-json", "provider", "list"],
            '\u{009b}',
        ),
    ];
    for (responder, command, control) in cases {
        let output = run_from(command.iter().copied(), &MockClient::new(*responder));
        assert_eq!(output.code, ExitCode::ProtocolError);
        assert!(output.stderr.contains("invalid_daemon_response"));
        assert!(!output.stdout.contains(*control));
        assert!(!output.stderr.contains(*control));
    }
}

#[test]
fn rejects_unsafe_controls_in_command_arguments_before_sending() {
    for label in ["safe\u{202e}txt", "safe\u{009b}txt"] {
        let client = MockClient::new(|_| unreachable!());
        let output = run_from(
            ["ullage", "account", "add", "claude", "--label", label],
            &client,
        );

        assert_eq!(output.code, ExitCode::Usage);
        assert!(output.stderr.contains("error: usage\n"));
        assert!(output.stderr.contains("--label"));
        assert!(!output.stderr.contains(label));
        assert!(client.requests.lock().unwrap().is_empty());
    }
}

/// Every free-text argument on every leaf subcommand must reject control
/// characters at parse time through `free_text_argument`. Walking the clap
/// schema — not a hand-mirrored list — is what makes this auditable: a new
/// `String` field without the marker fails this test.
#[test]
fn every_free_text_argument_rejects_control_characters() {
    use clap::{CommandFactory, Parser};
    use ullage_cli::Cli;

    // Canonical argv for every leaf subcommand: all positionals (including
    // optional ones) first, then the required options.
    let canonical: &[(&[&str], &[&str])] = &[
        (&["daemon", "install"], &[]),
        (&["daemon", "start"], &[]),
        (&["daemon", "stop"], &[]),
        (&["daemon", "run"], &[]),
        (&["daemon", "status"], &[]),
        (&["daemon", "uninstall"], &[]),
        (&["provider", "list"], &[]),
        (&["account", "add"], &["claude"]),
        (&["account", "list"], &[]),
        (&["account", "show"], &["claude-a"]),
        (&["account", "enable"], &["claude-a"]),
        (&["account", "disable"], &["claude-a"]),
        (&["account", "label"], &["claude-a", "label-a"]),
        (&["account", "metrics"], &["claude-a", "usage"]),
        (&["account", "remove"], &["claude-a"]),
        (&["auth", "login"], &["claude"]),
        (
            &["auth", "complete"],
            &["claude", "flow-1", "--account", "claude-a"],
        ),
        (&["auth", "status"], &["claude", "--account", "claude-a"]),
        (&["auth", "logout"], &["claude", "--account", "claude-a"]),
        (&["probe"], &["claude-a"]),
        (&["show"], &["claude-a"]),
        (&["device", "pair"], &[]),
        (&["device", "list"], &[]),
        (&["device", "revoke"], &["dev-1"]),
    ];

    // Collect every leaf path from the schema and require full coverage.
    fn leaf_paths(command: &clap::Command, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        let mut is_leaf = true;
        for subcommand in command.get_subcommands() {
            if subcommand.get_name() == "help" {
                continue;
            }
            is_leaf = false;
            path.push(subcommand.get_name().to_owned());
            leaf_paths(subcommand, path, out);
            path.pop();
        }
        if is_leaf && !path.is_empty() {
            out.push(path.clone());
        }
    }
    let mut command = Cli::command();
    command.build();
    let mut paths = Vec::new();
    leaf_paths(&command, &mut Vec::new(), &mut paths);
    let mut expected: Vec<Vec<String>> = canonical
        .iter()
        .map(|(path, _)| path.iter().map(ToString::to_string).collect())
        .collect();
    paths.sort();
    expected.sort();
    assert_eq!(paths, expected, "leaf subcommand coverage drifted");

    const BAD: &str = "safe\u{0007}value";
    for (path, tokens) in canonical {
        let leaf = path.iter().fold(&command, |command, name| {
            command.find_subcommand(name).expect("listed leaf exists")
        });
        for argument in leaf.get_arguments() {
            if !argument.get_action().takes_values() || !argument.get_possible_values().is_empty() {
                continue;
            }
            let mut argv: Vec<String> = std::iter::once("ullage")
                .chain(path.iter().copied())
                .map(str::to_owned)
                .chain(tokens.iter().copied().map(str::to_owned))
                .collect();
            if let Some(long) = argument.get_long() {
                let flag = format!("--{long}");
                match argv.iter().position(|token| token == &flag) {
                    // The canonical argv already carries the flag: replace
                    // its value instead of passing the flag twice.
                    Some(index) => argv[index + 1] = BAD.to_owned(),
                    None => {
                        argv.push(flag);
                        argv.push(BAD.to_owned());
                    }
                }
            } else {
                // Positionals occupy the leading canonical tokens after the
                // program name and subcommand path; the clap index is
                // one-based among positionals.
                let index = argument
                    .get_index()
                    .expect("non-option argument is positional");
                argv[1 + path.len() + index - 1] = BAD.to_owned();
            }
            let error = match Cli::try_parse_from(&argv) {
                Err(error) => error,
                Ok(_) => panic!("{path:?} accepted {BAD:?}: {argv:?}"),
            };
            assert!(
                error.to_string().contains("disallowed control characters"),
                "{path:?} {}: {error}",
                argument.get_id()
            );
        }
    }
}

#[test]
fn reveal_never_exposes_error_details_in_json_output() {
    fn partial(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Partial {
                    data: empty_usage(),
                    failures: vec![PartialFailure {
                        scope: "secret-scope".into(),
                        message: "secret-partial".into(),
                    }],
                },
            )]),
        ))
    }
    fn stale(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![SnapshotPayload {
                stale: true,
                last_error: Some(SanitizedErrorPayload::Network),
                last_error_at: Some(Utc.with_ymd_and_hms(2026, 8, 27, 12, 1, 0).unwrap()),
                metrics: Vec::new(),
                ..snapshot(
                    "primary",
                    QueryOutcome::Complete {
                        data: empty_usage(),
                    },
                )
            }]),
        ))
    }
    fn invalid(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::AuthState(ullage_protocol::AuthState::Invalid {
                reason: "secret-auth".into(),
                account_key: None,
            }),
        ))
    }

    let cases: &[(Responder, &[&str])] = &[
        (partial, &["show", "--all"]),
        (stale, &["show", "primary"]),
        (
            invalid,
            &["auth", "status", "claude", "--account", "primary"],
        ),
    ];
    for format in ["json", "pretty-json"] {
        for (responder, command) in cases {
            let mut arguments = vec!["ullage", "--output", format, "--reveal"];
            arguments.extend_from_slice(command);
            let output = run_from(arguments, &MockClient::new(*responder));
            for secret in ["secret-scope", "secret-partial", "secret-auth"] {
                assert!(!output.stdout.contains(secret));
            }
        }
    }
}

#[test]
fn rejects_wrong_account_state_and_mismatched_errors() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let result = match &request.command {
            ControlCommand::SetAccountEnabled { account, enabled } => {
                ControlResult::Account(Account {
                    id: account.clone(),
                    provider: ProviderId::new("claude"),
                    label: None,
                    enabled: !enabled,
                    metrics: Vec::new(),
                })
            }
            ControlCommand::ShowAccount { .. } => ControlResult::Error(ControlError::Account(
                AccountError::NotFound(AccountId::new("other")),
            )),
            ControlCommand::DaemonStatus => ControlResult::Error(ControlError::Account(
                AccountError::NotFound(AccountId::new("primary")),
            )),
            ControlCommand::AddAccount { .. } => ControlResult::Error(ControlError::Registry(
                ullage_protocol::RegistryError::NotFound(ProviderId::new("other-provider")),
            )),
            _ => unreachable!(),
        };
        Ok(response(request, result))
    }
    let client = MockClient::new(responder);
    let commands: &[&[&str]] = &[
        &["ullage", "account", "enable", "primary"],
        &["ullage", "account", "disable", "primary"],
        &["ullage", "account", "show", "primary"],
        &["ullage", "daemon", "status"],
        &["ullage", "account", "add", "claude"],
    ];

    for command in commands {
        let output = run_from(command.iter().copied(), &client);
        assert_eq!(output.code, ExitCode::ProtocolError, "{command:?}");
    }
}

#[test]
fn rejects_registry_duplicate_and_accepts_a_provider_negotiated_auth_method() {
    fn duplicate(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Registry(
                ullage_protocol::RegistryError::Duplicate(ProviderId::new("claude")),
            )),
        ))
    }
    fn negotiated_method(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::AuthChallenge(AuthChallenge {
                flow_id: "flow".into(),
                method: AuthMethod::BrowserOAuth,
                verification_uri: None,
                user_code: None,
                expires_at: None,
                input: None,
            }),
        ))
    }

    let duplicate_output = run_from(
        ["ullage", "account", "add", "claude"],
        &MockClient::new(duplicate),
    );
    let method_output = run_from(
        [
            "ullage",
            "auth",
            "login",
            "claude",
            "--account",
            "primary",
            "--method",
            "device-code",
        ],
        &MockClient::new(negotiated_method),
    );

    assert_eq!(duplicate_output.code, ExitCode::ProtocolError);
    assert_eq!(method_output.code, ExitCode::Success);
}

#[test]
fn accepts_registry_not_found_from_a_probe() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Registry(
                ullage_protocol::RegistryError::NotFound(ProviderId::new("missing")),
            )),
        ))
    }

    let output = run_from(
        ["ullage", "--output", "json", "probe", "primary"],
        &MockClient::new(responder),
    );

    assert_eq!(output.code, ExitCode::Failure);
    assert!(output.stderr.contains("provider_registry_error"));
    assert!(!output.stderr.contains("invalid_daemon_response"));
}

#[test]
fn authentication_uri_is_hidden_unless_revealed() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::AuthChallenge(AuthChallenge {
                flow_id: "flow".into(),
                method: AuthMethod::BrowserOAuth,
                verification_uri: Some("https://example.test/login?state=sensitive".into()),
                user_code: None,
                expires_at: None,
                input: None,
            }),
        ))
    }
    let client = MockClient::new(responder);
    for format in ["table", "json", "pretty-json"] {
        let output = run_from(
            [
                "ullage",
                "--output",
                format,
                "auth",
                "login",
                "claude",
                "--account",
                "primary",
            ],
            &client,
        );
        assert_eq!(output.code, ExitCode::Success);
        assert!(output.stdout.contains("[redacted]"));
        assert!(!output.stdout.contains("state=sensitive"));
    }
    let revealed = run_from(
        [
            "ullage",
            "--output",
            "json",
            "--reveal",
            "auth",
            "login",
            "claude",
            "--account",
            "primary",
        ],
        &client,
    );
    assert!(revealed.stdout.contains("state=sensitive"));
}

#[test]
fn rejects_c0_controls_in_forged_account_results_for_every_output_format() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Accounts(vec![Account {
                id: AccountId::new("unsafe\u{1b}[31m"),
                provider: ProviderId::new("claude"),
                label: Some("person@example.test".into()),
                enabled: true,
                metrics: Vec::new(),
            }]),
        ))
    }
    for arguments in [
        &["ullage", "--output", "table", "account", "list"][..],
        &["ullage", "--output", "json", "account", "list"][..],
        &["ullage", "--output", "pretty-json", "account", "list"][..],
    ] {
        let output = run_from(arguments.iter().copied(), &MockClient::new(responder));
        assert_eq!(output.code, ExitCode::ProtocolError);
        assert!(output.stderr.contains("invalid_daemon_response"));
        assert!(!output.stdout.contains("example.test"));
        assert!(!output.stdout.contains('\u{1b}'));
        assert!(!output.stderr.contains('\u{1b}'));
    }
}

#[test]
fn authentication_values_cannot_inject_terminal_controls() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::AuthChallenge(AuthChallenge {
                flow_id: "\u{1b}[31mflow".into(),
                method: AuthMethod::DeviceCode,
                verification_uri: Some("https://example.test/login".into()),
                user_code: Some("\u{1b}[31mcode".into()),
                expires_at: None,
                input: None,
            }),
        ))
    }
    let client = MockClient::new(responder);
    let hidden = run_from(
        ["ullage", "auth", "login", "claude", "--account", "primary"],
        &client,
    );
    let revealed = run_from(
        [
            "ullage",
            "--reveal",
            "auth",
            "login",
            "claude",
            "--account",
            "primary",
        ],
        &client,
    );

    assert_eq!(hidden.code, ExitCode::ProtocolError);
    assert_eq!(revealed.code, ExitCode::ProtocolError);
    assert!(hidden.stdout.is_empty());
    assert!(revealed.stdout.is_empty());
    assert!(!hidden.stdout.contains('\u{1b}'));
    assert!(!revealed.stdout.contains('\u{1b}'));
    assert!(!hidden.stderr.contains('\u{1b}'));
    assert!(!revealed.stderr.contains('\u{1b}'));
}

#[test]
fn daemon_run_uses_the_runtime_and_usage_errors_are_distinct() {
    let run = run_from(
        ["ullage", "daemon", "run"],
        &MockClient::with_daemon_result(Ok(())),
    );
    let usage = run_from(["ullage", "show"], &MockClient::with_daemon_result(Ok(())));

    assert_eq!(run.code, ExitCode::Success);
    assert_eq!(usage.code, ExitCode::Usage);
}

#[test]
fn daemon_run_honors_json_output() {
    let output = run_from(
        ["ullage", "--output", "json", "daemon", "run"],
        &MockClient::with_daemon_result(Ok(())),
    );

    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(output.stdout, "{\"result\":\"ack\"}\n");
}

#[test]
fn device_pair_supports_every_output_format() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        assert_eq!(request.command, ControlCommand::CreatePairCode);
        Ok(response(
            request,
            ControlResult::PairCode(PairCodePayload {
                code: "ABC-DEF".into(),
                expires_at: Utc.with_ymd_and_hms(2026, 9, 1, 12, 5, 0).unwrap(),
            }),
        ))
    }

    let client = MockClient::new(responder);
    let table = run_from(["ullage", "--color", "never", "device", "pair"], &client);
    assert_eq!(table.code, ExitCode::Success);
    assert!(table.stdout.contains("CODE        ABC-DEF\n"));
    assert!(
        table
            .stdout
            .contains("EXPIRES_AT  2026-09-01T12:05:00+00:00\n")
    );
    assert!(table.stdout.contains("HTTP API address"));
    assert!(table.stdout.contains("one-time"));
    assert!(table.stdout.contains("300 seconds"));
    assert!(table.stdout.contains("new code invalidates it"));

    for format in ["json", "pretty-json"] {
        let output = run_from(["ullage", "--output", format, "device", "pair"], &client);
        assert_eq!(output.code, ExitCode::Success, "{format}");
        let value: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
        assert_eq!(value["result"], "pair_code");
        assert_eq!(value["payload"]["code"], "ABC-DEF");
        assert_eq!(value["payload"]["expires_at"], "2026-09-01T12:05:00Z");
    }
}

#[test]
fn device_list_supports_every_output_format_without_tokens() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        assert_eq!(request.command, ControlCommand::ListDevices);
        let now = Utc::now();
        Ok(response(
            request,
            ControlResult::Devices(vec![DevicePayload {
                id: "DEVICE123456".into(),
                name: "client-host".into(),
                created_at: now - chrono::Duration::hours(2),
                last_seen_at: now - chrono::Duration::minutes(3),
            }]),
        ))
    }

    let client = MockClient::new(responder);
    for format in ["table", "json", "pretty-json"] {
        let output = run_from(
            [
                "ullage", "--output", format, "--color", "never", "device", "list",
            ],
            &client,
        );
        assert_eq!(output.code, ExitCode::Success, "{format}");
        assert!(output.stdout.contains("DEVICE123456"), "{format}");
        assert!(output.stdout.contains("client-host"), "{format}");
        let lower = output.stdout.to_ascii_lowercase();
        assert!(!lower.contains("token"), "{format}: {}", output.stdout);
        assert!(!lower.contains("hash"), "{format}: {}", output.stdout);
    }

    let table = run_from(["ullage", "--color", "never", "device", "list"], &client);
    assert!(
        table
            .stdout
            .contains("| DEVICE ID    | NAME        | CREATED   | LAST SEEN |"),
        "{}",
        table.stdout
    );
    assert!(table.stdout.contains("2h00m ago"));
    assert!(table.stdout.contains("3m ago"));
}

#[test]
fn device_list_empty_state_matches_other_list_tables() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(request, ControlResult::Devices(Vec::new())))
    }

    let output = run_from(
        ["ullage", "--color", "never", "device", "list"],
        &MockClient::new(responder),
    );
    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(
        output.stdout,
        concat!(
            "+-----------+------+---------+-----------+\n",
            "| DEVICE ID | NAME | CREATED | LAST SEEN |\n",
            "+-----------+------+---------+-----------+\n",
            "+-----------+------+---------+-----------+\n",
        )
    );
}

#[test]
fn device_revoke_supports_every_output_format_and_not_found() {
    fn revoked(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        assert_eq!(
            request.command,
            ControlCommand::RevokeDevice {
                device_id: "DEVICE123456".into(),
            }
        );
        Ok(response(request, ControlResult::Ack))
    }
    fn missing(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::DeviceNotFound {
                device_id: "MISSING12345".into(),
            }),
        ))
    }

    for format in ["table", "json", "pretty-json"] {
        let output = run_from(
            [
                "ullage",
                "--output",
                format,
                "device",
                "revoke",
                "DEVICE123456",
            ],
            &MockClient::new(revoked),
        );
        assert_eq!(output.code, ExitCode::Success, "{format}");
        assert!(
            output
                .stdout
                .contains(if format == "table" { "ok" } else { "ack" })
        );

        let output = run_from(
            [
                "ullage",
                "--output",
                format,
                "device",
                "revoke",
                "MISSING12345",
            ],
            &MockClient::new(missing),
        );
        assert_eq!(output.code, ExitCode::Failure, "{format}");
        assert!(output.stderr.contains("device_not_found"), "{format}");
    }
}

#[test]
fn retired_http_command_is_unknown() {
    let output = run_from(
        ["ullage", "http", "token"],
        &MockClient::new(|_| unreachable!()),
    );
    assert_eq!(output.code, ExitCode::Usage);
    assert!(output.stderr.contains("unrecognized subcommand 'http'"));
}

#[test]
fn daemon_service_commands_use_the_local_service_manager() {
    for command in ["install", "start", "stop", "uninstall"] {
        let output = run_from(
            ["ullage", "daemon", command],
            &MockClient::with_daemon_result(Ok(())),
        );
        assert_eq!(output.code, ExitCode::Success, "{command}");
        assert_eq!(output.stdout, "ok\n", "{command}");
    }

    let failure = run_from(
        ["ullage", "daemon", "install"],
        &MockClient::with_daemon_result(Err(ClientError::DaemonProcess)),
    );
    assert_eq!(failure.code, ExitCode::Failure);
    assert_eq!(failure.stderr, "error: daemon_service_failed\n");
}

#[test]
fn daemon_status_distinguishes_stopped_and_not_installed_services() {
    for (installed, expected) in [
        (true, "STATUS   stopped\nSERVICE  installed\n"),
        (false, "STATUS   stopped\nSERVICE  not-installed\n"),
    ] {
        let output = run_from(
            ["ullage", "--color", "never", "daemon", "status"],
            &StoppedServiceClient { installed },
        );
        assert_eq!(output.code, ExitCode::Success);
        assert_eq!(output.stdout, expected);
    }

    let json = run_from(
        ["ullage", "--output", "json", "daemon", "status"],
        &StoppedServiceClient { installed: true },
    );
    assert_eq!(
        json.stdout,
        "{\"service\":\"installed\",\"status\":\"stopped\"}\n"
    );
}

#[test]
fn daemon_status_reports_the_credential_backend_in_table_and_json() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::DaemonStatus(DaemonStatusPayload {
                shutting_down: false,
                accounts: Vec::new(),
                credential_backend: CredentialBackendId::FileFallback,
            }),
        ))
    }
    let client = MockClient::new(responder);
    let table = run_from(["ullage", "--color", "never", "daemon", "status"], &client);
    assert_eq!(table.code, ExitCode::Success);
    assert_eq!(
        table.stdout,
        "STATUS              running\nACCOUNTS            0\nACTIVE_PROBES       0\nCREDENTIAL_BACKEND  file_fallback\n"
    );

    let json = run_from(["ullage", "--output", "json", "daemon", "status"], &client);
    assert_eq!(json.code, ExitCode::Success);
    assert!(
        json.stdout
            .contains("\"credential_backend\":\"file_fallback\"")
    );
    assert!(!json.stderr.contains("invalid_daemon_response"));
}

#[test]
fn usage_errors_do_not_echo_untrusted_arguments() {
    let output = run_from(
        [
            "ullage",
            "auth",
            "login",
            "claude",
            "--account",
            "primary",
            "--method",
            "person@example.test\u{1b}\nsecret-token",
        ],
        &MockClient::with_daemon_result(Ok(())),
    );

    assert_eq!(output.code, ExitCode::Usage);
    assert!(output.stderr.contains("--method"));
    assert!(!output.stderr.contains("person@example.test"));
    assert!(!output.stderr.contains("secret-token"));
    assert!(!output.stderr.contains('\u{1b}'));
}

#[test]
fn unknown_flag_with_control_chars_uses_static_hint() {
    let output = run_from(
        ["ullage", "--not-a-real-flag", "safe\u{1b}txt"],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(output.code, ExitCode::Usage);
    assert!(
        output
            .stderr
            .contains("a command argument contains disallowed control characters")
    );
    assert!(!output.stderr.contains("--not-a-real-flag"));
}

#[test]
fn boolean_flag_does_not_own_following_positional_control_chars() {
    let output = run_from(
        ["ullage", "show", "--all", "bad\u{1b}txt"],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(output.code, ExitCode::Usage);
    assert!(
        output
            .stderr
            .contains("a command argument contains disallowed control characters")
    );
    assert!(!output.stderr.contains("--all"));
}

#[test]
fn inline_flag_assignment_control_chars_name_the_option() {
    let output = run_from(
        [
            "ullage",
            "auth",
            "login",
            "claude",
            "--account",
            "primary",
            "--method=bad\u{1b}txt",
        ],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(output.code, ExitCode::Usage);
    assert!(output.stderr.contains("--method"));
    assert!(!output.stderr.contains("bad"));
}

#[test]
fn option_terminator_makes_later_tokens_positional_for_control_reject() {
    let output = run_from(
        ["ullage", "show", "--", "--method=bad\u{1b}txt"],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(output.code, ExitCode::Usage);
    assert!(
        output
            .stderr
            .contains("a command argument contains disallowed control characters")
    );
    assert!(!output.stderr.contains("--method"));
}

#[test]
fn unknown_option_after_value_flag_uses_generic_control_hint() {
    let output = run_from(
        [
            "ullage",
            "auth",
            "login",
            "claude",
            "--account",
            "primary",
            "--method",
            "--not-real\u{1b}txt",
        ],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(output.code, ExitCode::Usage);
    assert!(
        output
            .stderr
            .contains("a command argument contains disallowed control characters")
    );
    assert!(!output.stderr.contains("--method"));
}

#[test]
fn output_inference_stops_at_the_option_terminator() {
    let output = run_from(
        ["ullage", "show", "--", "--output", "json"],
        &MockClient::with_daemon_result(Ok(())),
    );

    assert_eq!(output.code, ExitCode::Usage);
    assert!(output.stderr.starts_with("error:"));
    assert!(!output.stderr.starts_with('{'));
}

#[test]
fn reveal_is_an_explicit_opt_in_for_sensitive_json_values() {
    let client = MockClient::new(snapshot_result);
    let output = run_from(
        ["ullage", "--output", "json", "--reveal", "show", "primary"],
        &client,
    );

    assert_eq!(output.code, ExitCode::Success);
    assert!(output.stdout.contains("person@example.test"));
    assert!(!output.stdout.contains("[redacted]"));
}

#[test]
fn table_preserves_dynamic_window_and_unit_identifiers() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = empty_usage();
        usage.windows.push(UsageWindow {
            window: UsageWindowKind::Other {
                id: "rolling_30d".into(),
                label: "Rolling 30 days".into(),
            },
            resets_at: None,
            measurements: vec![UsageMeasurement {
                name: "spend".into(),
                used: 4.5,
                limit: Some(20.0),
                unit: MeasurementUnit::Currency { code: "USD".into() },
            }],
        });
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete { data: usage },
            )]),
        ))
    }
    let output = run_from(
        ["ullage", "--raw", "show", "primary"],
        &MockClient::new(responder),
    );

    assert_eq!(output.code, ExitCode::Success);
    assert!(output.stdout.contains("other:rolling_30d"));
    assert!(output.stdout.contains("currency:USD"));
}

#[test]
fn help_is_successful_without_contacting_the_daemon() {
    let output = run_from(
        ["ullage", "--help"],
        &MockClient::with_daemon_result(Ok(())),
    );

    assert_eq!(output.code, ExitCode::Success);
    assert!(output.stdout.contains("Usage:"));
    assert!(output.stderr.is_empty());
}

#[test]
fn output_format_default_is_table() {
    assert_eq!(OutputFormat::default(), OutputFormat::Table);
}

#[test]
fn color_mode_default_is_auto() {
    assert_eq!(
        ullage_cli::ColorMode::default(),
        ullage_cli::ColorMode::Auto
    );
}

/// A mock daemon with enough state to walk a whole interactive login.
struct LoginClient {
    accounts: Mutex<Vec<Account>>,
    requests: Mutex<Vec<ControlCommand>>,
    /// Errors returned by `CompleteAuth`, one per attempt, before it succeeds.
    complete_failures: Mutex<Vec<ControlError>>,
    /// Errors returned by `SetAccountLabel`, one per attempt, before it succeeds.
    label_failures: Mutex<Vec<ControlError>>,
    logout_failures: Mutex<Vec<ControlError>>,
    pending_polls: Mutex<usize>,
    diagnostic: Option<&'static str>,
    authenticated_label: Option<&'static str>,
    providers: Vec<ProviderDescriptor>,
    challenge_input: Option<ullage_protocol::AuthInputRequest>,
    challenge_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Set once `CompleteAuth` has actually stored a session, so `AuthStatus`
    /// answers what happened rather than always claiming a sign-in.
    signed_in: Mutex<bool>,
}

impl LoginClient {
    fn new() -> Self {
        Self {
            accounts: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            complete_failures: Mutex::new(Vec::new()),
            label_failures: Mutex::new(Vec::new()),
            logout_failures: Mutex::new(Vec::new()),
            pending_polls: Mutex::new(0),
            diagnostic: None,
            authenticated_label: Some("person@example.test"),
            providers: vec![
                descriptor("claude", "Claude"),
                descriptor("chatgpt", "ChatGPT"),
            ],
            challenge_input: Some(ullage_protocol::AuthInputRequest::visible(
                "the full callback URL from the browser, or code#state",
            )),
            challenge_expires_at: None,
            signed_in: Mutex::new(false),
        }
    }

    fn commands(&self) -> Vec<ControlCommand> {
        self.requests.lock().unwrap().clone()
    }

    fn saw(&self, predicate: impl Fn(&ControlCommand) -> bool) -> bool {
        self.commands().iter().any(predicate)
    }
}

fn descriptor(id: &str, display_name: &str) -> ProviderDescriptor {
    ProviderDescriptor {
        id: ProviderId::new(id),
        display_name: display_name.into(),
        capabilities: vec![
            ullage_protocol::Capability::Authentication,
            ullage_protocol::Capability::AuthenticationStatus,
        ],
    }
}

impl ControlClient for LoginClient {
    fn send(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        self.requests.lock().unwrap().push(request.command.clone());
        let mut diagnostic = None;
        let result = match &request.command {
            ControlCommand::DaemonStatus => ControlResult::DaemonStatus(DaemonStatusPayload {
                shutting_down: false,
                accounts: Vec::new(),
                credential_backend: CredentialBackendId::LinuxSecretService,
            }),
            ControlCommand::ListProviders => ControlResult::Providers(self.providers.clone()),
            ControlCommand::ListAccounts => {
                ControlResult::Accounts(self.accounts.lock().unwrap().clone())
            }
            ControlCommand::AddAccount { provider, label } => {
                let mut accounts = self.accounts.lock().unwrap();
                let account = Account {
                    id: AccountId::new(format!("account-{}", accounts.len() + 1)),
                    provider: provider.clone(),
                    label: label.clone(),
                    enabled: true,
                    metrics: Vec::new(),
                };
                accounts.push(account.clone());
                ControlResult::Account(account)
            }
            ControlCommand::SetAccountLabel { account, label } => {
                if let Some(error) = self.label_failures.lock().unwrap().pop() {
                    ControlResult::Error(error)
                } else {
                    let mut accounts = self.accounts.lock().unwrap();
                    let stored = accounts
                        .iter_mut()
                        .find(|stored| &stored.id == account)
                        .expect("the account exists");
                    stored.label = label.clone();
                    ControlResult::Account(stored.clone())
                }
            }
            ControlCommand::ShowAccount { account } => {
                let accounts = self.accounts.lock().unwrap();
                let stored = accounts
                    .iter()
                    .find(|stored| &stored.id == account)
                    .expect("the account exists");
                ControlResult::Account(stored.clone())
            }
            ControlCommand::RemoveAccount { account } => {
                self.accounts
                    .lock()
                    .unwrap()
                    .retain(|stored| &stored.id != account);
                ControlResult::Ack
            }
            ControlCommand::StartAuth { .. } => ControlResult::AuthChallenge(AuthChallenge {
                flow_id: "flow-1".into(),
                method: AuthMethod::BrowserOAuth,
                verification_uri: Some("https://example.test/authorize".into()),
                user_code: None,
                expires_at: self.challenge_expires_at,
                input: self.challenge_input.clone(),
            }),
            ControlCommand::CompleteAuth { .. } => {
                match self.complete_failures.lock().unwrap().pop() {
                    Some(error) => {
                        diagnostic = request
                            .diagnostics
                            .then(|| self.diagnostic.unwrap_or("provider said no").to_owned());
                        ControlResult::Error(error)
                    }
                    None => {
                        let mut pending = self.pending_polls.lock().unwrap();
                        if *pending > 0 {
                            *pending -= 1;
                            ControlResult::AuthState(ullage_protocol::AuthState::Pending {
                                flow_id: "flow-1".into(),
                                expires_at: self.challenge_expires_at,
                            })
                        } else {
                            *self.signed_in.lock().unwrap() = true;
                            ControlResult::AuthState(ullage_protocol::AuthState::Authenticated {
                                account_label: self.authenticated_label.map(Into::into),
                                expires_at: None,
                                account_key: None,
                            })
                        }
                    }
                }
            }
            ControlCommand::Logout { .. } => {
                if let Some(error) = self.logout_failures.lock().unwrap().pop() {
                    ControlResult::Error(error)
                } else {
                    *self.signed_in.lock().unwrap() = false;
                    ControlResult::Ack
                }
            }
            ControlCommand::RetireDuplicateAccounts { .. } => ControlResult::Accounts(Vec::new()),
            ControlCommand::Probe { account_id, .. } => ControlResult::Probe(ProbePayload {
                account_id: account_id.clone(),
                usage: QueryOutcome::Complete {
                    data: empty_usage(),
                },
                metrics: Vec::new(),
            }),
            ControlCommand::AuthStatus { .. } => {
                if *self.signed_in.lock().unwrap() {
                    ControlResult::AuthState(ullage_protocol::AuthState::Authenticated {
                        account_label: self.authenticated_label.map(Into::into),
                        expires_at: None,
                        account_key: None,
                    })
                } else {
                    ControlResult::AuthState(ullage_protocol::AuthState::NotAuthenticated)
                }
            }
            other => unreachable!("unexpected control command: {other:?}"),
        };
        Ok(ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            result,
            diagnostic,
            daemon_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        })
    }
}

#[test]
fn interactive_login_creates_authenticates_and_names_an_account() {
    let client = LoginClient::new();
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new([
        "1",
        "https://platform.claude.com/oauth/code/callback?code=abc&state=flow-1",
        "",
    ]);

    let output = run_from_with(
        ["ullage", "--reveal", "auth", "login"],
        &client,
        &mut prompt,
    );

    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    assert!(output.stdout.contains("account-1"), "{}", output.stdout);
    assert!(
        output.stdout.contains("person@example.test"),
        "{}",
        output.stdout
    );
    assert_eq!(prompt.remaining(), 0);
    // The provisional label keeps `AddAccount` unique before the real one lands.
    assert!(client.saw(|command| matches!(
        command,
        ControlCommand::AddAccount { label: Some(label), .. } if label == "claude-1"
    )));
    // The pasted callback URL supplies both the code and the redirect URI.
    assert!(client.saw(|command| matches!(
        command,
        ControlCommand::CompleteAuth { request, .. }
            if request.authorization_code.as_deref()
                == Some("https://platform.claude.com/oauth/code/callback?code=abc&state=flow-1")
                && request.redirect_uri.as_deref()
                    == Some("https://platform.claude.com/oauth/code/callback")
    )));
    // The credential is checked independently of the completion result.
    assert!(client.saw(|command| matches!(command, ControlCommand::AuthStatus { .. })));
    assert!(client.saw(|command| matches!(
        command,
        ControlCommand::SetAccountLabel { label: Some(label), .. } if label == "person@example.test"
    )));
    assert!(prompt.said("https://example.test/authorize"));
}

/// The name a provider discovers is the same for both accounts, so retiring the
/// older row has to happen before the new one claims that name.
#[test]
fn interactive_login_retires_duplicates_before_it_names_the_account() {
    let client = LoginClient::new();
    // Provider 1, a new account, pasted code, then Enter to take the default
    // name, which is the identity the provider reported.
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1", "1", "code#flow-1", ""]);

    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);

    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    let commands = client.commands();
    let retired = commands
        .iter()
        .position(|command| matches!(command, ControlCommand::RetireDuplicateAccounts { .. }))
        .expect("duplicates were never retired");
    let named = commands
        .iter()
        .position(|command| matches!(command, ControlCommand::SetAccountLabel { .. }))
        .expect("the account was never named");
    assert!(retired < named, "{commands:?}");
}

#[test]
fn interactive_login_offers_existing_accounts_and_keeps_their_label() {
    let client = LoginClient::new();
    client.accounts.lock().unwrap().push(Account {
        id: AccountId::new("account-7"),
        provider: ProviderId::new("claude"),
        label: Some("work".into()),
        enabled: true,
        metrics: Vec::new(),
    });
    // Provider 1, account entry 2 (the existing one), pasted code, keep the
    // current label even though the provider returned a different identity.
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1", "2", "code#flow-1", ""]);

    let output = run_from_with(
        ["ullage", "--reveal", "auth", "login"],
        &client,
        &mut prompt,
    );

    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    assert!(!client.saw(|command| matches!(command, ControlCommand::AddAccount { .. })));
    assert!(!client.saw(|command| matches!(command, ControlCommand::SetAccountLabel { .. })));
    // A bare code carries no redirect URI to derive.
    assert!(client.saw(|command| matches!(
        command,
        ControlCommand::CompleteAuth { request, .. } if request.redirect_uri.is_none()
    )));
    assert!(prompt.said("account-7 (work)"));
}

#[test]
fn interactive_login_reports_provider_detail_and_removes_the_stub_account() {
    let client = LoginClient::new();
    client
        .complete_failures
        .lock()
        .unwrap()
        .push(ControlError::Provider(
            ProviderError::AuthenticationInvalid {
                message: "provider authentication is invalid".into(),
            },
        ));
    // Provider 1, paste a code, decline the retry.
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1", "code#flow-1", "n"]);

    let output = run_from_with(
        ["ullage", "--diagnose", "auth", "login"],
        &client,
        &mut prompt,
    );

    assert_eq!(output.code, ExitCode::AuthenticationInvalid);
    assert!(prompt.said("complete_auth"), "{:?}", prompt.transcript());
    assert!(
        output.stderr.contains("stage: complete_auth"),
        "{}",
        output.stderr
    );
    assert!(prompt.said("provider said no"), "{:?}", prompt.transcript());
    // The account created for the abandoned flow does not survive it.
    assert!(client.saw(|command| matches!(command, ControlCommand::Logout { .. })));
    assert!(client.saw(|command| matches!(command, ControlCommand::RemoveAccount { .. })));
    assert!(client.accounts.lock().unwrap().is_empty());
}

#[test]
fn interactive_login_retries_the_whole_flow_on_request() {
    let client = LoginClient::new();
    client
        .complete_failures
        .lock()
        .unwrap()
        .push(ControlError::Provider(
            ProviderError::AuthenticationInvalid {
                message: "provider authentication is invalid".into(),
            },
        ));
    // Provider 1, a bad paste, retry, a good paste, accept the default label.
    let mut prompt =
        ullage_cli::prompt::ScriptedPrompt::new(["1", "code#wrong", "y", "code#flow-1", ""]);

    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);

    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    // A retry restarts the authorization flow rather than reusing the spent one.
    assert_eq!(
        client
            .commands()
            .iter()
            .filter(|command| matches!(command, ControlCommand::StartAuth { .. }))
            .count(),
        2
    );
    // Without --diagnose the failure names the flag instead of the provider text.
    assert!(prompt.said("--diagnose"));
    assert!(!prompt.said("provider said no"));
}

#[test]
fn interactive_login_hides_a_secret_and_skips_the_paste_for_device_flows() {
    let mut client = LoginClient::new();
    client.challenge_input = Some(ullage_protocol::AuthInputRequest::secret(
        "the Cursor API key",
    ));
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1", "api-key-value", ""]);
    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);
    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    assert_eq!(prompt.secrets_asked(), 1);

    let mut client = LoginClient::new();
    client.challenge_input = None;
    // Provider 1, acknowledge the approval, accept the default label.
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1", "", ""]);
    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);
    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    assert_eq!(prompt.secrets_asked(), 0);
    assert!(client.saw(|command| matches!(
        command,
        ControlCommand::CompleteAuth { request, .. } if request.authorization_code.is_none()
    )));
}

/// A loopback browser flow also accepts the callback URL pasted by hand, for
/// browsers that cannot reach the daemon's 127.0.0.1 listener.
#[test]
fn interactive_login_accepts_a_pasted_callback_for_browser_flows() {
    let mut client = LoginClient::new();
    client.challenge_input = None;
    // Provider 1, paste the callback URL instead of pressing Enter, accept the
    // default label.
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new([
        "1",
        "http://127.0.0.1:54321/callback?code=abc&state=flow-1",
        "",
    ]);
    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);
    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    assert!(client.saw(|command| matches!(
        command,
        ControlCommand::CompleteAuth { request, .. }
            if request.authorization_code.as_deref()
                == Some("http://127.0.0.1:54321/callback?code=abc&state=flow-1")
                && request.redirect_uri.as_deref()
                    == Some("http://127.0.0.1:54321/callback")
    )));
}

#[test]
fn interactive_login_preselects_a_named_provider() {
    let client = LoginClient::new();
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["code#flow-1", ""]);

    let output = run_from_with(["ullage", "auth", "login", "chatgpt"], &client, &mut prompt);

    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    assert!(client.saw(|command| matches!(
        command,
        ControlCommand::AddAccount { provider, .. } if provider.as_str() == "chatgpt"
    )));
}

#[test]
fn interactive_login_needs_a_terminal() {
    struct Silent;

    impl ullage_cli::prompt::Prompt for Silent {
        fn tell(&mut self, _: &str) {}

        fn ask(&mut self, _: &str) -> Option<String> {
            None
        }

        fn is_interactive(&self) -> bool {
            false
        }
    }

    let client = LoginClient::new();
    let output = run_from_with(["ullage", "auth", "login"], &client, &mut Silent);

    assert_eq!(output.code, ExitCode::Usage);
    assert!(
        output.stderr.contains("not_interactive"),
        "{}",
        output.stderr
    );
    assert!(output.stderr.contains("auth complete"), "{}", output.stderr);
    assert!(client.commands().is_empty());
}

#[test]
fn interactive_login_explains_a_stopped_daemon() {
    struct Unavailable;

    impl ControlClient for Unavailable {
        fn send(&self, _: &ControlRequest) -> Result<ControlResponse, ClientError> {
            Err(ClientError::DaemonUnavailable)
        }
    }

    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(Vec::<String>::new());
    let output = run_from_with(["ullage", "auth", "login"], &Unavailable, &mut prompt);

    assert_eq!(output.code, ExitCode::DaemonUnavailable);
    assert!(
        prompt.said("daemon is not running"),
        "{:?}",
        prompt.transcript()
    );
    assert!(prompt.said("ullage daemon start"));
    assert!(output.stderr.contains("stage: daemon"), "{}", output.stderr);
    assert!(
        output.stderr.contains("hint: start the daemon"),
        "{}",
        output.stderr
    );
}

#[test]
fn interactive_login_keeps_an_account_whose_naming_is_abandoned() {
    // By this point the account holds a credential and may already have
    // replaced an older one signed in as the same person. Abandoning the name
    // leaves it unnamed, not deleted.
    let client = LoginClient::new();
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1", "code#flow-1"]);

    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);

    assert_eq!(output.code, ExitCode::Failure);
    assert!(output.stderr.contains("cancelled"), "{}", output.stderr);
    assert!(
        output.stderr.contains("stage: account_label"),
        "{}",
        output.stderr
    );
    assert!(!client.saw(|command| matches!(command, ControlCommand::RemoveAccount { .. })));
    assert_eq!(client.accounts.lock().unwrap().len(), 1);
    assert!(prompt.said("is signed in"), "{:?}", prompt.transcript());
}

#[test]
fn interactive_login_keeps_an_account_whose_renaming_fails() {
    let client = LoginClient::new();
    client
        .label_failures
        .lock()
        .unwrap()
        .push(ControlError::Storage);
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1", "code#flow-1", ""]);

    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);

    assert_eq!(output.code, ExitCode::Failure);
    assert!(output.stderr.contains("storage"), "{}", output.stderr);
    assert!(
        output.stderr.contains("stage: account_label"),
        "{}",
        output.stderr
    );
    assert!(!client.saw(|command| matches!(command, ControlCommand::RemoveAccount { .. })));
    assert_eq!(client.accounts.lock().unwrap().len(), 1);
}

#[test]
fn interactive_login_does_not_read_a_secret_when_echo_cannot_be_disabled() {
    let mut client = LoginClient::new();
    client.challenge_input = Some(ullage_protocol::AuthInputRequest::secret(
        "the Cursor API key",
    ));
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1"]);
    prompt.refuse_secret_echo();

    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);

    assert_eq!(output.code, ExitCode::Failure);
    assert!(output.stderr.contains("terminal_echo"), "{}", output.stderr);
    assert_eq!(prompt.secrets_asked(), 1);
    assert_eq!(prompt.remaining(), 0);
    assert!(!client.saw(|command| matches!(command, ControlCommand::CompleteAuth { .. })));
    assert!(client.saw(|command| matches!(command, ControlCommand::Logout { .. })));
    assert!(client.saw(|command| matches!(command, ControlCommand::RemoveAccount { .. })));
}

#[test]
fn interactive_login_keeps_polling_a_device_flow_after_rate_limiting() {
    let mut client = LoginClient::new();
    client.challenge_input = None;
    client
        .complete_failures
        .lock()
        .unwrap()
        .push(ControlError::Provider(ProviderError::RateLimited {
            message: "slow down".into(),
            retry_after_seconds: Some(1),
        }));
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1", "", ""]);

    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);

    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    assert_eq!(
        client
            .commands()
            .iter()
            .filter(|command| matches!(command, ControlCommand::CompleteAuth { .. }))
            .count(),
        2
    );
}

#[test]
fn account_list_table_uses_ascii_borders_and_alignment() {
    let output = run_from(
        ["ullage", "--color", "never", "account", "list"],
        &MockClient::new(account_list_result),
    );
    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(
        output.stdout,
        concat!(
            "+---------+----------+-------+---------+---------+\n",
            "| ACCOUNT | PROVIDER | LABEL | ENABLED | METRICS |\n",
            "+---------+----------+-------+---------+---------+\n",
            "| primary | claude   | -     | true    | -       |\n",
            "+---------+----------+-------+---------+---------+\n",
        )
    );
    assert!(!output.stdout.contains('\u{1b}'));
    assert!(!output.stderr.contains('\u{1b}'));
}

#[test]
fn show_header_aligns_account_and_usage_keys_in_one_paragraph() {
    let output = run_from(
        ["ullage", "--color", "never", "--raw", "show", "primary"],
        &MockClient::new(snapshot_result),
    );
    assert_eq!(output.code, ExitCode::Success);
    assert!(
        output.stdout.starts_with("==== claude ====\n"),
        "{}",
        output.stdout
    );
    assert!(
        !output
            .stdout
            .lines()
            .any(|line| { line.starts_with("ACCOUNT") && !line.starts_with("ACCOUNT_LABEL") }),
        "ACCOUNT key-value row should be replaced by the section header:\n{}",
        output.stdout
    );
    let status = output
        .stdout
        .lines()
        .find(|line| line.starts_with("STATUS"))
        .unwrap();
    let label = output
        .stdout
        .lines()
        .find(|line| line.starts_with("ACCOUNT_LABEL"))
        .unwrap();
    let status_value = status.find("current").unwrap();
    let label_value = label.find("[redacted]").unwrap();
    assert_eq!(
        status_value, label_value,
        "show header values should share one column:\n{}",
        output.stdout
    );
    assert!(
        output
            .stdout
            .contains("| WINDOW | MEASUREMENT | USED | LIMIT | UNIT | RESETS_AT |"),
        "{}",
        output.stdout
    );
}

#[test]
fn show_separates_multiple_accounts_with_section_headers() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut secondary = empty_usage();
        secondary.provider = ProviderId::new("cursor");
        Ok(response(
            request,
            ControlResult::Snapshots(vec![
                snapshot(
                    "primary",
                    QueryOutcome::Complete {
                        data: empty_usage(),
                    },
                ),
                SnapshotPayload {
                    account_id: "secondary".into(),
                    usage: QueryOutcome::Partial {
                        data: secondary,
                        failures: vec![PartialFailure {
                            scope: "secret-scope".into(),
                            message: "sensitive detail".into(),
                        }],
                    },
                    last_success_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
                    stale: true,
                    last_error: Some(SanitizedErrorPayload::Network),
                    last_error_at: Some(Utc.with_ymd_and_hms(2026, 8, 27, 12, 1, 0).unwrap()),
                    metrics: Vec::new(),
                },
            ]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "--raw", "show", "--all"],
        &MockClient::new(responder),
    );
    assert_eq!(output.code, ExitCode::Partial, "{}", output.stderr);
    let stdout = output.stdout.as_str();
    let primary = stdout.find("==== claude ====").expect(stdout);
    let secondary = stdout.find("==== cursor ====").expect(stdout);
    assert!(primary < secondary, "{stdout}");
    assert!(stdout.contains("==== claude ====\nSTATUS"), "{stdout}");
    assert!(stdout.contains("\n\n==== cursor ===="), "{stdout}");
    let warning = stdout.find("WARNING").expect(stdout);
    let last_error = stdout.find("LAST_ERROR").expect(stdout);
    assert!(secondary < warning, "{stdout}");
    assert!(warning < last_error, "{stdout}");
    assert!(!stdout[..secondary].contains("WARNING"), "{stdout}");
    assert!(!stdout[..secondary].contains("LAST_ERROR"), "{stdout}");
    assert!(!stdout.contains("sensitive detail"), "{stdout}");
}

#[test]
fn show_section_header_is_colored_only_when_color_is_always() {
    let colored = run_from(
        ["ullage", "--color", "always", "--raw", "show", "primary"],
        &MockClient::new(snapshot_result),
    );
    let plain = run_from(
        ["ullage", "--color", "never", "--raw", "show", "primary"],
        &MockClient::new(snapshot_result),
    );
    assert_eq!(colored.code, ExitCode::Success);
    assert_eq!(plain.code, ExitCode::Success);
    assert!(
        colored
            .stdout
            .contains("\u{1b}[1;36m==== claude ====\u{1b}[0m"),
        "{}",
        colored.stdout
    );
    assert!(!plain.stdout.contains('\u{1b}'), "{}", plain.stdout);
    assert!(
        plain.stdout.starts_with("==== claude ====\n"),
        "{}",
        plain.stdout
    );
}

#[test]
fn color_always_emits_ansi_on_table_output() {
    let output = run_from(
        ["ullage", "--color", "always", "account", "list"],
        &MockClient::new(account_list_result),
    );
    assert_eq!(output.code, ExitCode::Success);
    assert!(output.stdout.contains('\u{1b}'));
    assert!(output.stdout.contains("[redacted]") || output.stdout.contains("primary"));
    assert!(output.stdout.contains("\u{1b}[1;36m"), "{}", output.stdout);
    assert!(output.stdout.contains("\u{1b}[2m"), "{}", output.stdout);
}

#[test]
fn json_output_never_contains_ansi_even_with_color_always() {
    let client = MockClient::new(snapshot_result);
    let plain = run_from(["ullage", "--output", "json", "show", "primary"], &client);
    let colored = run_from(
        [
            "ullage", "--color", "always", "--output", "json", "show", "primary",
        ],
        &client,
    );
    let pretty = run_from(
        [
            "ullage",
            "--color",
            "always",
            "--output",
            "pretty-json",
            "show",
            "primary",
        ],
        &client,
    );
    assert_eq!(plain.stdout, colored.stdout);
    assert!(!colored.stdout.contains('\u{1b}'));
    assert!(!pretty.stdout.contains('\u{1b}'));
    assert!(pretty.stdout.contains("\n  \"result\": \"snapshots\""));
}

#[test]
fn cjk_provider_names_keep_ascii_table_borders_aligned() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Providers(vec![ProviderDescriptor {
                id: ProviderId::new("claude"),
                display_name: "中文名称".into(),
                capabilities: Vec::new(),
            }]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "provider", "list"],
        &MockClient::new(responder),
    );
    assert_eq!(output.code, ExitCode::Success);
    let lines: Vec<&str> = output.stdout.lines().collect();
    assert_eq!(lines.len(), 5, "{}", output.stdout);
    let border_cols = lines[0].chars().count();
    let data = lines[3];
    assert!(data.contains("中文名称"), "{data}");
    assert_eq!(
        data.chars().count() + 4,
        border_cols,
        "CJK double-width should match the ASCII border:\n{}",
        output.stdout
    );
}

#[test]
fn used_at_ninety_percent_is_colored_red_when_color_is_always() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = empty_usage();
        usage.windows.push(UsageWindow {
            window: UsageWindowKind::Weekly,
            resets_at: None,
            measurements: vec![UsageMeasurement {
                name: "spend".into(),
                used: 90.0,
                limit: Some(100.0),
                unit: MeasurementUnit::Credits,
            }],
        });
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete { data: usage },
            )]),
        ))
    }
    let colored = run_from(
        ["ullage", "--color", "always", "--raw", "show", "primary"],
        &MockClient::new(responder),
    );
    let plain = run_from(
        ["ullage", "--color", "never", "--raw", "show", "primary"],
        &MockClient::new(responder),
    );
    assert!(colored.stdout.contains("\u{1b}[31m"), "{}", colored.stdout);
    assert!(!plain.stdout.contains('\u{1b}'));
    assert!(plain.stdout.contains("90"));
}

#[test]
fn color_always_does_not_color_json_error_envelopes() {
    let output = run_from(
        ["ullage", "--color", "always", "--output", "json", "show"],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(output.code, ExitCode::Usage);
    assert!(!output.stdout.contains('\u{1b}'));
    assert!(!output.stderr.contains('\u{1b}'));
    assert!(output.stderr.contains("\"kind\":\"usage\""));
}

#[test]
fn interactive_login_keeps_a_stub_when_logout_fails() {
    // The sign-in itself fails here, so the stub holds nothing and is cleaned
    // up — except that signing it out fails, and a row whose credential may
    // still exist is kept rather than orphaning the secret.
    let client = LoginClient::new();
    client
        .complete_failures
        .lock()
        .unwrap()
        .push(ControlError::Provider(
            ProviderError::AuthenticationInvalid {
                message: "provider said no".into(),
            },
        ));
    client
        .logout_failures
        .lock()
        .unwrap()
        .push(ControlError::Provider(ProviderError::Network {
            message: "revoke failed".into(),
        }));
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(["1", "code#flow-1"]);

    let output = run_from_with(["ullage", "auth", "login"], &client, &mut prompt);

    assert_eq!(output.code, ExitCode::AuthenticationInvalid);
    assert!(client.saw(|command| matches!(command, ControlCommand::Logout { .. })));
    assert!(!client.saw(|command| matches!(command, ControlCommand::RemoveAccount { .. })));
    assert_eq!(client.accounts.lock().unwrap().len(), 1);
    assert!(prompt.said("auth logout"), "{:?}", prompt.transcript());
}

#[test]
fn interactive_login_rejects_an_unsolicited_diagnostic() {
    struct Leaky(LoginClient);

    impl ControlClient for Leaky {
        fn send(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError> {
            let mut response = self.0.send(request)?;
            response.diagnostic = Some("leaked provider text".into());
            Ok(response)
        }
    }

    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(Vec::<String>::new());
    let output = run_from_with(
        ["ullage", "auth", "login"],
        &Leaky(LoginClient::new()),
        &mut prompt,
    );

    assert_eq!(output.code, ExitCode::ProtocolError);
    assert!(output.stderr.contains("invalid_daemon_response"));
    assert!(!output.stderr.contains("leaked provider text"));
}

#[test]
fn a_login_without_a_provider_cannot_name_an_account() {
    let client = LoginClient::new();
    let mut prompt = ullage_cli::prompt::ScriptedPrompt::new(Vec::<String>::new());

    let output = run_from_with(
        ["ullage", "auth", "login", "--account", "account-1"],
        &client,
        &mut prompt,
    );

    assert_eq!(output.code, ExitCode::Usage);
    assert!(client.commands().is_empty());
}

#[test]
fn missing_subcommands_print_layer_help_to_stderr() {
    let commands: &[&[&str]] = &[
        &["ullage"],
        &["ullage", "account"],
        &["ullage", "auth"],
        &["ullage", "daemon"],
        &["ullage", "provider"],
    ];
    for command in commands {
        let output = run_from(*command, &MockClient::with_daemon_result(Ok(())));
        assert_eq!(output.code, ExitCode::Usage, "{command:?}");
        assert!(output.stderr.contains("Usage:"), "{}", output.stderr);
        assert!(output.stderr.contains("Commands:"), "{}", output.stderr);
        assert!(output.stdout.is_empty(), "{}", output.stdout);
    }

    let with_global = run_from(
        ["ullage", "--output", "json"],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(with_global.code, ExitCode::Usage);
    assert!(
        with_global.stderr.contains("Commands:"),
        "{}",
        with_global.stderr
    );
    assert!(!with_global.stderr.contains("requires a subcommand"));

    let nested_with_global = run_from(
        ["ullage", "account", "--reveal"],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(nested_with_global.code, ExitCode::Usage);
    assert!(
        nested_with_global.stderr.contains("Usage: ullage account"),
        "{}",
        nested_with_global.stderr
    );
    assert!(
        nested_with_global.stderr.contains("\n  add"),
        "{}",
        nested_with_global.stderr
    );
    assert!(
        !nested_with_global.stderr.contains("\n  probe"),
        "{}",
        nested_with_global.stderr
    );
}

#[test]
fn workspace_is_not_a_cli_command() {
    let client = MockClient::with_daemon_result(Ok(()));
    let output = run_from(["ullage", "workspace"], &client);

    assert_eq!(output.code, ExitCode::Usage);
    assert!(
        output
            .stderr
            .contains("unrecognized subcommand 'workspace'")
    );
    assert!(output.stdout.is_empty());
    assert!(client.requests.lock().unwrap().is_empty());
}

#[test]
fn unknown_subcommand_includes_did_you_mean() {
    let output = run_from(
        ["ullage", "acount", "list"],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(output.code, ExitCode::Usage);
    assert!(output.stderr.contains("unrecognized subcommand 'acount'"));
    assert!(output.stderr.contains("account"));
}

#[test]
fn parse_errors_surface_clap_details() {
    let client = MockClient::with_daemon_result(Ok(()));
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["ullage", "account", "lst"],
            &["unrecognized subcommand", "list"],
        ),
        (
            &["ullage", "show", "--al"],
            &["unexpected argument", "--all"],
        ),
        (&["ullage", "probe"], &["<ACCOUNT_ID>"]),
        (&["ullage", "account", "add"], &["<PROVIDER_ID>"]),
        (&["ullage", "auth", "complete", "claude"], &["--account"]),
        (
            &["ullage", "auth", "complete", "claude", "--account"],
            &["--account"],
        ),
        (
            &["ullage", "--output", "xml", "show", "primary"],
            &["--output", "xml"],
        ),
        (
            &["ullage", "--color", "sometimes", "show", "primary"],
            &["--color", "sometimes"],
        ),
        (
            &[
                "ullage",
                "auth",
                "login",
                "claude",
                "--method",
                "magic-link",
            ],
            &["--method", "magic-link"],
        ),
        (&["ullage", "show", "primary", "--all"], &["--all"]),
        (
            &["ullage", "auth", "login", "--account", "primary"],
            &["PROVIDER_ID"],
        ),
    ];

    for (command, needles) in cases {
        let output = run_from(*command, &client);
        assert_eq!(output.code, ExitCode::Usage, "{command:?}");
        for needle in *needles {
            assert!(
                output.stderr.contains(needle),
                "{command:?}: {}",
                output.stderr
            );
        }
    }
}

#[test]
fn help_and_version_stay_on_stdout_with_success() {
    let commands: &[&[&str]] = &[
        &["ullage", "--help"],
        &["ullage", "-h"],
        &["ullage", "help"],
        &["ullage", "--version"],
    ];
    for command in commands {
        let output = run_from(*command, &MockClient::with_daemon_result(Ok(())));
        assert_eq!(output.code, ExitCode::Success, "{command:?}");
        assert!(!output.stdout.is_empty(), "{command:?}");
        assert!(output.stderr.is_empty(), "{command:?}: {}", output.stderr);
    }
}

#[test]
fn json_parse_errors_include_message_without_hint() {
    let output = run_from(
        ["ullage", "--output", "json", "show"],
        &MockClient::with_daemon_result(Ok(())),
    );
    assert_eq!(output.code, ExitCode::Usage);
    assert!(output.stderr.contains("\"kind\":\"usage\""));
    assert!(output.stderr.contains("\"message\":"));
    assert!(!output.stderr.contains("\"hint\":"));
    assert!(!output.stderr.contains('\u{1b}'));
}

#[test]
fn runtime_error_hints_are_static() {
    fn registry_error(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::Registry(
                ullage_protocol::RegistryError::NotFound(ProviderId::new("missing")),
            )),
        ))
    }
    fn account_error(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::AccountNotFound {
                account_id: "missing".into(),
            }),
        ))
    }
    fn selector_error(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Error(ControlError::AccountSelectorNotFound {
                provider: ProviderId::new("chatgpt"),
                account_label: Some("work".into()),
            }),
        ))
    }
    fn unavailable(_: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Err(ClientError::DaemonUnavailable)
    }

    let table = run_from(
        ["ullage", "account", "add", "missing"],
        &MockClient::new(registry_error),
    );
    assert!(table.stderr.contains("provider_registry_error"));
    assert!(
        table
            .stderr
            .contains("hint: pass a compiled-in provider id")
    );

    let account = run_from(
        ["ullage", "show", "missing"],
        &MockClient::new(account_error),
    );
    assert!(account.stderr.contains("account_not_found"));
    assert!(account.stderr.contains("hint: pass a stable account id"));

    let selector = run_from(
        ["ullage", "probe", "primary"],
        &MockClient::new(selector_error),
    );
    assert!(selector.stderr.contains("account_selector_not_found"));
    assert!(selector.stderr.contains("hint: pass a stable account id"));

    let daemon = run_from(["ullage", "show", "primary"], &MockClient::new(unavailable));
    assert!(daemon.stderr.contains("daemon_unavailable"));
    assert!(daemon.stderr.contains("hint: start the daemon"));

    let json = run_from(
        ["ullage", "--output", "json", "show", "primary"],
        &MockClient::new(unavailable),
    );
    assert!(json.stderr.contains("\"kind\":\"daemon_unavailable\""));
    assert!(json.stderr.contains("\"hint\":"));
    assert!(!json.stderr.contains('\u{1b}'));
}

/// Usage shaped like a live Claude snapshot, anchored to the current time so
/// the relative reset and update lines stay predictable.
#[test]
fn show_defaults_to_a_readable_summary_with_a_trailing_progress_bar() {
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(summary_result),
    );

    assert_eq!(output.code, ExitCode::Success);
    let stdout = output.stdout.as_str();
    assert!(stdout.starts_with("==== claude - pro ====\n"), "{stdout}");
    assert!(
        stdout.lines().nth(1).unwrap().starts_with("5h "),
        "{stdout}"
    );

    let five_hours = line_starting_with(stdout, "5h ");
    assert!(five_hours.contains("remains 97%"), "{stdout}");
    assert!(five_hours.contains("-  3h -"), "{stdout}");
    assert!(five_hours.ends_with("[##########]"), "{stdout}");

    let opus = line_starting_with(stdout, "Weekly Opus");
    assert!(opus.contains("remains 89%"), "{stdout}");
    assert!(opus.contains("--*****"), "{stdout}");
    assert!(opus.ends_with("[-#########]"), "{stdout}");

    assert!(!stdout.contains('\u{1b}'), "{stdout}");
    for hidden in ["PROVIDER", "ACCOUNT_LABEL", "OBSERVED_AT", "EXPIRES_AT"] {
        assert!(
            !stdout.lines().any(|line| line.starts_with(hidden)),
            "{hidden} should not appear in the summary view:\n{stdout}"
        );
    }
    assert!(!stdout.contains("| WINDOW |"), "{stdout}");
}

#[test]
fn show_raw_restores_the_provider_table() {
    let output = run_from(
        ["ullage", "--color", "never", "--raw", "show", "primary"],
        &MockClient::new(summary_result),
    );

    assert_eq!(output.code, ExitCode::Success);
    let stdout = output.stdout.as_str();
    assert!(stdout.starts_with("==== claude ====\nSTATUS"), "{stdout}");
    let header = line_starting_with(stdout, "| WINDOW ");
    for column in ["MEASUREMENT", "USED", "LIMIT", "UNIT", "RESETS_AT"] {
        assert!(header.contains(column), "{stdout}");
    }
    assert!(stdout.contains("| five_hours "), "{stdout}");
    assert!(stdout.contains("| other:seven_day_opus "), "{stdout}");
    assert!(!stdout.contains('█'), "{stdout}");
    assert!(!stdout.contains("[########"), "{stdout}");
    assert!(!stdout.contains("remains"), "{stdout}");
}

#[test]
fn raw_is_a_no_op_for_json_and_pretty_json_output() {
    for format in ["json", "pretty-json"] {
        let plain = run_from(
            ["ullage", "--output", format, "show", "primary"],
            &MockClient::new(snapshot_result),
        );
        let raw = run_from(
            ["ullage", "--output", format, "--raw", "show", "primary"],
            &MockClient::new(snapshot_result),
        );
        assert_eq!(plain.stdout, raw.stdout, "{format} output changed");
        assert_eq!(plain.code, raw.code);
        assert!(!plain.stdout.contains('█'), "{}", plain.stdout);
        assert!(!plain.stdout.contains("[########"), "{}", plain.stdout);
    }
}

#[test]
fn the_summary_reports_partial_results_and_keeps_the_partial_exit_code() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Partial {
                    data: summarizable_usage(),
                    failures: vec![PartialFailure {
                        scope: "secret-scope".into(),
                        message: "sensitive detail".into(),
                    }],
                },
            )]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    assert_eq!(output.code, ExitCode::Partial);
    assert!(
        output.stdout.contains(
            "! 1 item(s) unavailable (--raw for raw data, --diagnose for error details)\n"
        ),
        "{}",
        output.stdout
    );
    assert!(
        !output.stdout.contains("sensitive detail"),
        "{}",
        output.stdout
    );
}

#[test]
fn diagnose_surfaces_partial_failure_scope_and_category() {
    let hidden = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(partial_profile_incompatible),
    );
    assert_eq!(hidden.code, ExitCode::Partial);
    assert!(hidden.stdout.contains("[redacted]") || hidden.stdout.contains("unavailable"));
    assert!(
        !hidden
            .stdout
            .contains("profile: provider protocol response is incompatible"),
        "{}",
        hidden.stdout
    );

    let shown = run_from(
        [
            "ullage",
            "--color",
            "never",
            "--diagnose",
            "show",
            "primary",
        ],
        &MockClient::new(partial_profile_incompatible),
    );
    assert_eq!(shown.code, ExitCode::Partial);
    assert!(
        shown
            .stdout
            .contains("profile: provider protocol response is incompatible"),
        "{}",
        shown.stdout
    );

    let json_hidden = run_from(
        ["ullage", "--output", "json", "show", "primary"],
        &MockClient::new(partial_profile_incompatible),
    );
    assert_eq!(json_hidden.code, ExitCode::Partial);
    assert!(
        json_hidden.stdout.contains("[redacted]"),
        "{}",
        json_hidden.stdout
    );
    assert!(!json_hidden.stdout.contains("\"scope\":\"profile\""));
    assert!(
        !json_hidden
            .stdout
            .contains("provider protocol response is incompatible")
    );

    let json_shown = run_from(
        [
            "ullage",
            "--diagnose",
            "--output",
            "json",
            "show",
            "primary",
        ],
        &MockClient::new(partial_profile_incompatible),
    );
    assert_eq!(json_shown.code, ExitCode::Partial);
    assert!(
        json_shown.stdout.contains("\"scope\":\"profile\""),
        "{}",
        json_shown.stdout
    );
    assert!(
        json_shown
            .stdout
            .contains("provider protocol response is incompatible"),
        "{}",
        json_shown.stdout
    );
}

#[test]
fn diagnose_surfaces_partial_failures_on_probe() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Probe(ProbePayload {
                account_id: "primary".into(),
                usage: QueryOutcome::Partial {
                    data: summarizable_usage(),
                    failures: vec![PartialFailure {
                        scope: "usage".into(),
                        message: "provider network request failed".into(),
                    }],
                },
                metrics: Vec::new(),
            }),
        ))
    }
    let hidden = run_from(
        ["ullage", "--color", "never", "probe", "primary"],
        &MockClient::new(responder),
    );
    assert_eq!(hidden.code, ExitCode::Partial);
    assert!(
        !hidden
            .stdout
            .contains("usage: provider network request failed"),
        "{}",
        hidden.stdout
    );

    let shown = run_from(
        [
            "ullage",
            "--color",
            "never",
            "--diagnose",
            "probe",
            "primary",
        ],
        &MockClient::new(responder),
    );
    assert_eq!(shown.code, ExitCode::Partial);
    assert!(
        shown
            .stdout
            .contains("usage: provider network request failed"),
        "{}",
        shown.stdout
    );
}

#[test]
fn diagnose_does_not_print_ansi_from_partial_failure_scope() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Partial {
                    data: summarizable_usage(),
                    failures: vec![PartialFailure {
                        scope: "profile\u{1b}[31m".into(),
                        message: "provider protocol response is incompatible".into(),
                    }],
                },
            )]),
        ))
    }
    let output = run_from(
        [
            "ullage",
            "--color",
            "never",
            "--diagnose",
            "show",
            "primary",
        ],
        &MockClient::new(responder),
    );
    assert_eq!(output.code, ExitCode::Partial);
    assert!(!output.stdout.contains('\u{1b}'), "{}", output.stdout);
    assert!(!output.stderr.contains('\u{1b}'), "{}", output.stderr);
    assert!(
        output
            .stdout
            .contains("[redacted]: provider protocol response is incompatible"),
        "{}",
        output.stdout
    );
}

#[test]
fn a_stale_snapshot_is_still_called_out_in_the_summary() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut payload = snapshot(
            "primary",
            QueryOutcome::Complete {
                data: summarizable_usage(),
            },
        );
        payload.stale = true;
        payload.last_error = Some(SanitizedErrorPayload::Network);
        payload.last_error_at = Some(Utc.with_ymd_and_hms(2026, 8, 27, 12, 1, 0).unwrap());
        Ok(response(request, ControlResult::Snapshots(vec![payload])))
    }
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    assert!(
        output.stdout.contains("! stale snapshot"),
        "{}",
        output.stdout
    );
}

#[test]
fn a_reached_limit_survives_the_hidden_status_booleans() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = summarizable_usage();
        usage.windows[0].measurements.push(UsageMeasurement {
            name: "limit_reached".into(),
            used: 1.0,
            limit: Some(1.0),
            unit: MeasurementUnit::Other {
                id: "boolean".into(),
                label: "Boolean".into(),
            },
        });
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete { data: usage },
            )]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    assert!(
        output.stdout.contains("! limit reached\n"),
        "{}",
        output.stdout
    );
    assert!(
        !output.stdout.contains("limit reached  "),
        "the boolean row itself stays in raw output:\n{}",
        output.stdout
    );
}

#[test]
fn an_expiring_subscription_shows_a_single_expiry_line() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = summarizable_usage();
        usage.subscription_expires_at = Some(Utc.with_ymd_and_hms(2026, 12, 1, 8, 30, 0).unwrap());
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete { data: usage },
            )]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    assert!(
        output.stdout.contains("\nexpires 2026-12-01\n"),
        "{}",
        output.stdout
    );
    let plain = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(summary_result),
    );
    assert!(!plain.stdout.contains("expires"), "{}", plain.stdout);
}

#[test]
fn a_low_balance_colors_only_the_progress_bar() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = summarizable_usage();
        usage.windows[0].measurements[0].used = 95.0;
        usage.windows[1].measurements[0].used = 80.0;
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete { data: usage },
            )]),
        ))
    }
    let colored = run_from(
        ["ullage", "--color", "always", "show", "primary"],
        &MockClient::new(responder),
    );
    let plain = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    assert!(
        colored.stdout.contains("\u{1b}[31m[---------#]\u{1b}[0m"),
        "{}",
        colored.stdout
    );
    assert!(
        colored.stdout.contains("\u{1b}[33m[--------##]\u{1b}[0m"),
        "{}",
        colored.stdout
    );
    assert!(!plain.stdout.contains('\u{1b}'), "{}", plain.stdout);
}

#[test]
fn hostile_provider_strings_never_reach_the_summary_renderer() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = summarizable_usage();
        usage.windows[1].window = UsageWindowKind::Other {
            id: "evil".into(),
            label: "boss\n==== claude - forged ====".into(),
        };
        usage.windows[1].measurements[0].name = "spend\r##########".into();
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete { data: usage },
            )]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    assert_eq!(output.code, ExitCode::ProtocolError);
    assert!(output.stdout.is_empty(), "{}", output.stdout);
    assert!(!output.stderr.contains("forged"), "{}", output.stderr);
    assert!(!output.stderr.contains('\r'), "{}", output.stderr);
}

#[test]
fn raw_never_widens_what_the_summary_redacts() {
    for arguments in [
        vec!["ullage", "--color", "never", "--raw", "show", "primary"],
        vec!["ullage", "--color", "never", "show", "primary"],
    ] {
        let hidden = run_from(arguments.clone(), &MockClient::new(summary_result));
        assert!(
            !hidden.stdout.contains("person@example.test"),
            "{:?}\n{}",
            arguments,
            hidden.stdout
        );

        let mut revealing = arguments.clone();
        revealing.insert(1, "--reveal");
        let revealed = run_from(revealing, &MockClient::new(summary_result));
        assert_eq!(
            revealed.stdout.contains("person@example.test"),
            arguments.contains(&"--raw"),
            "the account label is a raw-table field either way:\n{}",
            revealed.stdout
        );
    }
}

#[test]
fn a_summary_without_readable_rows_falls_back_to_the_raw_table() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = empty_usage();
        usage.windows.push(UsageWindow {
            window: UsageWindowKind::FiveHours,
            resets_at: None,
            measurements: vec![UsageMeasurement {
                name: "allowed".into(),
                used: 1.0,
                limit: Some(1.0),
                unit: MeasurementUnit::Other {
                    id: "boolean".into(),
                    label: "Boolean".into(),
                },
            }],
        });
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete { data: usage },
            )]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    let stdout = output.stdout.as_str();
    assert!(
        stdout.contains("! no summarized metrics available; showing raw data\n"),
        "{stdout}"
    );
    let header = line_starting_with(stdout, "| WINDOW ");
    assert!(header.contains("MEASUREMENT"), "{stdout}");
    assert!(stdout.contains("| allowed "), "{stdout}");
}

/// A snapshot with nothing to summarize still has to report why it is unusable.
#[test]
fn the_raw_fallback_still_carries_stale_partial_and_limit_notices() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = empty_usage();
        usage.windows.push(UsageWindow {
            window: UsageWindowKind::FiveHours,
            resets_at: None,
            measurements: vec![
                UsageMeasurement {
                    name: "allowed".into(),
                    used: 0.0,
                    limit: Some(1.0),
                    unit: MeasurementUnit::Other {
                        id: "boolean".into(),
                        label: "Boolean".into(),
                    },
                },
                UsageMeasurement {
                    name: "has_credits".into(),
                    used: 1.0,
                    limit: Some(1.0),
                    unit: MeasurementUnit::Other {
                        id: "boolean".into(),
                        label: "Boolean".into(),
                    },
                },
            ],
        });
        Ok(response(
            request,
            ControlResult::Snapshots(vec![SnapshotPayload {
                account_id: "primary".into(),
                usage: QueryOutcome::Partial {
                    data: usage,
                    failures: vec![PartialFailure {
                        scope: "secret-scope".into(),
                        message: "sensitive detail".into(),
                    }],
                },
                last_success_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
                stale: true,
                last_error: Some(SanitizedErrorPayload::Network),
                last_error_at: Some(Utc.with_ymd_and_hms(2026, 8, 27, 12, 1, 0).unwrap()),
                metrics: Vec::new(),
            }]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    assert_eq!(output.code, ExitCode::Partial);
    let stdout = output.stdout.as_str();
    assert!(
        stdout.contains("! no summarized metrics available; showing raw data\n"),
        "{stdout}"
    );
    assert!(stdout.contains("| allowed "), "{stdout}");
    assert!(stdout.contains("! stale snapshot"), "{stdout}");
    assert!(stdout.contains("! limit reached\n"), "{stdout}");
    assert!(stdout.contains("! 1 item(s) unavailable"), "{stdout}");
    assert!(stdout.contains("LAST_ERROR"), "{stdout}");
    assert!(!stdout.contains("sensitive detail"), "{stdout}");
}

#[test]
fn a_window_reporting_only_a_switched_off_flag_still_shows_that_state() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = summarizable_usage();
        usage.windows.push(UsageWindow {
            window: UsageWindowKind::Other {
                id: "on_demand".into(),
                label: "On-demand usage".into(),
            },
            resets_at: None,
            measurements: vec![UsageMeasurement {
                name: "enabled".into(),
                used: 0.0,
                limit: Some(1.0),
                unit: MeasurementUnit::Other {
                    id: "boolean".into(),
                    label: "Enabled".into(),
                },
            }],
        });
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete { data: usage },
            )]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    let status = line_starting_with(&output.stdout, "On-demand usage");
    assert!(status.contains("status"), "{}", output.stdout);
    assert!(status.contains("disabled"), "{}", output.stdout);
}

#[test]
fn probe_shares_the_summary_view_and_its_raw_escape_hatch() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Probe(ProbePayload {
                account_id: "primary".into(),
                usage: QueryOutcome::Complete {
                    data: summarizable_usage(),
                },
                metrics: Vec::new(),
            }),
        ))
    }
    let summary = run_from(
        ["ullage", "--color", "never", "probe", "primary"],
        &MockClient::new(responder),
    );
    let raw = run_from(
        ["ullage", "--color", "never", "--raw", "probe", "primary"],
        &MockClient::new(responder),
    );

    assert!(
        summary.stdout.starts_with("==== claude - pro ====\n"),
        "{}",
        summary.stdout
    );
    assert!(summary.stdout.contains("remains 97%"), "{}", summary.stdout);
    assert!(raw.stdout.starts_with("PROVIDER"), "{}", raw.stdout);
    let header = line_starting_with(&raw.stdout, "| WINDOW ");
    for column in ["MEASUREMENT", "USED", "LIMIT", "UNIT", "RESETS_AT"] {
        assert!(header.contains(column), "{}", raw.stdout);
    }
    assert!(!raw.stdout.contains('█'), "{}", raw.stdout);
}

/// Locks the exact bytes of `--raw` table output so the summary view can never
/// leak into the escape hatch.
#[test]
fn raw_table_output_is_byte_stable() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = empty_usage();
        usage.windows.push(UsageWindow {
            window: UsageWindowKind::FiveHours,
            resets_at: Some(Utc.with_ymd_and_hms(2026, 8, 27, 15, 57, 0).unwrap()),
            measurements: vec![UsageMeasurement {
                name: "included_usage".into(),
                used: 3.0,
                limit: Some(100.0),
                unit: MeasurementUnit::Percent,
            }],
        });
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot(
                "primary",
                QueryOutcome::Complete { data: usage },
            )]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "--raw", "show", "primary"],
        &MockClient::new(responder),
    );

    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(
        output.stdout,
        concat!(
            "==== claude ====\n",
            "STATUS         current\n",
            "PROVIDER       claude\n",
            "ACCOUNT_LABEL  [redacted]\n",
            "PLAN           pro\n",
            "OBSERVED_AT    2026-08-27T12:00:00+00:00\n",
            "EXPIRES_AT     -\n",
            "+------------+----------------+------+-------+---------+---------------------------+\n",
            "| WINDOW     | MEASUREMENT    | USED | LIMIT | UNIT    | RESETS_AT                 |\n",
            "+------------+----------------+------+-------+---------+---------------------------+\n",
            "| five_hours | included_usage |    3 |   100 | percent | 2026-08-27T15:57:00+00:00 |\n",
            "+------------+----------------+------+-------+---------+---------------------------+\n",
        )
    );
}
