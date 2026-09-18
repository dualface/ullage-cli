use crate::*;

#[test]
fn request_uses_the_current_protocol_version() {
    let request = ControlRequest::new("request-1", ControlCommand::ListProviders);
    let json = serde_json::to_string(&request).unwrap();
    let decoded: ControlRequest = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.version, CONTROL_PROTOCOL_VERSION);
    assert_eq!(decoded, request);
}

#[test]
fn partial_usage_preserves_data_and_failures() {
    let observed_at = chrono::DateTime::parse_from_rfc3339("2026-08-27T12:00:00Z")
        .unwrap()
        .to_utc();
    let response = ControlResponse {
        version: CONTROL_PROTOCOL_VERSION,
        request_id: "request-2".into(),
        diagnostic: None,
        daemon_version: Some("0.1.6".into()),
        result: ControlResult::Usage(QueryOutcome::Partial {
            data: SubscriptionUsage {
                provider: ProviderId::new("test"),
                account_label: None,
                plan: None,
                subscription_expires_at: None,
                observed_at,
                windows: Vec::new(),
            },
            failures: vec![ullage_core::PartialFailure {
                scope: "weekly".into(),
                message: "temporarily unavailable".into(),
            }],
        }),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "version": CONTROL_PROTOCOL_VERSION,
            "request_id": "request-2",
            "daemon_version": "0.1.6",
            "result": {
                "result": "usage",
                "payload": {
                    "outcome": "partial",
                    "data": {
                        "provider": "test",
                        "account_label": null,
                        "plan": null,
                        "subscription_expires_at": null,
                        "observed_at": "2026-08-27T12:00:00Z",
                        "windows": []
                    },
                    "failures": [{
                        "scope": "weekly",
                        "message": "temporarily unavailable"
                    }]
                }
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<ControlResponse>(json).unwrap(),
        response
    );
}

#[test]
fn daemon_commands_and_auth_challenge_round_trip() {
    let commands = [
        ControlCommand::DaemonStatus,
        ControlCommand::Probe {
            account_id: "primary".into(),
            wait: true,
        },
        ControlCommand::Show { account_id: None },
        ControlCommand::SetAccountMetrics {
            account: AccountId::new("account-1"),
            metrics: vec!["usage".into(), "Codex".into()],
        },
    ];
    for (index, command) in commands.into_iter().enumerate() {
        let request = ControlRequest::new(format!("daemon-{index}"), command);
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<ControlRequest>(&encoded).unwrap(),
            request
        );
    }

    let response = ControlResponse {
        version: CONTROL_PROTOCOL_VERSION,
        request_id: "auth-challenge".into(),
        diagnostic: None,
        daemon_version: None,
        result: ControlResult::AuthChallenge(AuthChallenge {
            flow_id: "flow-1".into(),
            method: ullage_auth::AuthMethod::DeviceCode,
            verification_uri: Some("https://example.invalid/device".into()),
            user_code: Some("code-1".into()),
            expires_at: None,
            input: None,
        }),
    };
    let encoded = serde_json::to_string(&response).unwrap();
    assert_eq!(
        serde_json::from_str::<ControlResponse>(&encoded).unwrap(),
        response
    );

    for (index, error) in [
        ControlError::Timeout,
        ControlError::Cancelled,
        ControlError::Storage,
        ControlError::InvalidAccountMetrics,
        ControlError::AccountSelectorNotFound {
            provider: ProviderId::new("test"),
            account_label: Some("secondary".into()),
        },
    ]
    .into_iter()
    .enumerate()
    {
        let response = ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: format!("daemon-error-{index}"),
            result: ControlResult::Error(error),
            diagnostic: Some("provider detail".into()),
            daemon_version: None,
        };
        let encoded = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serde_json::from_str::<ControlResponse>(&encoded).unwrap(),
            response
        );
    }
}

#[test]
fn account_and_payload_metrics_round_trip_and_default_to_empty() {
    let account = Account {
        id: AccountId::new("account-1"),
        provider: ProviderId::new("claude"),
        label: None,
        enabled: true,
        metrics: vec!["usage".into(), "Codex".into()],
    };
    let json = serde_json::to_value(&account).unwrap();
    assert_eq!(json["metrics"], serde_json::json!(["usage", "Codex"]));
    assert_eq!(serde_json::from_value::<Account>(json).unwrap(), account);

    let legacy_account: Account = serde_json::from_value(serde_json::json!({
        "id": "account-1",
        "provider": "claude",
        "label": null,
        "enabled": true
    }))
    .unwrap();
    assert!(legacy_account.metrics.is_empty());

    let observed_at = chrono::DateTime::parse_from_rfc3339("2026-08-27T12:00:00Z")
        .unwrap()
        .to_utc();
    let usage = SubscriptionUsage {
        provider: ProviderId::new("claude"),
        account_label: None,
        plan: None,
        subscription_expires_at: None,
        observed_at,
        windows: Vec::new(),
    };
    let snapshot = SnapshotPayload {
        account_id: "account-1".into(),
        usage: QueryOutcome::Complete {
            data: usage.clone(),
        },
        last_success_at: observed_at,
        stale: false,
        last_error: None,
        last_error_at: None,
        metrics: vec!["usage".into()],
    };
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json["metrics"], serde_json::json!(["usage"]));
    assert_eq!(
        serde_json::from_value::<SnapshotPayload>(json.clone()).unwrap(),
        snapshot
    );
    let mut legacy = json;
    legacy.as_object_mut().unwrap().remove("metrics");
    assert!(
        serde_json::from_value::<SnapshotPayload>(legacy)
            .unwrap()
            .metrics
            .is_empty()
    );

    let probe = ProbePayload {
        account_id: "account-1".into(),
        usage: QueryOutcome::Complete { data: usage },
        metrics: vec!["Codex".into()],
    };
    let json = serde_json::to_value(&probe).unwrap();
    assert_eq!(json["metrics"], serde_json::json!(["Codex"]));
    assert_eq!(
        serde_json::from_value::<ProbePayload>(json.clone()).unwrap(),
        probe
    );
    let mut legacy = json;
    legacy.as_object_mut().unwrap().remove("metrics");
    assert!(
        serde_json::from_value::<ProbePayload>(legacy)
            .unwrap()
            .metrics
            .is_empty()
    );
}

/// Every `ControlCommand` and `ControlResult` variant must survive a
/// round trip so a v11 edit cannot silently change the wire shape.
#[test]
fn control_wire_types_round_trip_every_variant() {
    fn round_trip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        assert_eq!(
            serde_json::from_str::<T>(&json).unwrap(),
            *value,
            "round trip changed {json}"
        );
    }

    let provider = ProviderId::new("claude");
    let account = AccountId::new("account-1");
    let commands = [
        ControlCommand::DaemonStatus,
        ControlCommand::CreatePairCode,
        ControlCommand::ListDevices,
        ControlCommand::RevokeDevice {
            device_id: "device-1".into(),
        },
        ControlCommand::ListProviders,
        ControlCommand::AddAccount {
            provider: provider.clone(),
            label: Some("work".into()),
        },
        ControlCommand::AddAccount {
            provider: provider.clone(),
            label: None,
        },
        ControlCommand::ListAccounts,
        ControlCommand::ShowAccount {
            account: account.clone(),
        },
        ControlCommand::SetAccountEnabled {
            account: account.clone(),
            enabled: false,
        },
        ControlCommand::SetAccountLabel {
            account: account.clone(),
            label: Some("renamed".into()),
        },
        ControlCommand::SetAccountMetrics {
            account: account.clone(),
            metrics: vec!["usage".into()],
        },
        ControlCommand::RemoveAccount {
            account: account.clone(),
        },
        ControlCommand::Probe {
            account_id: "account-1".into(),
            wait: true,
        },
        ControlCommand::Show {
            account_id: Some("account-1".into()),
        },
        ControlCommand::Show { account_id: None },
        ControlCommand::QueryUsage {
            provider: provider.clone(),
            query: UsageQuery {
                account_label: Some("work".into()),
            },
        },
        ControlCommand::StartAuth {
            provider: provider.clone(),
            account: account.clone(),
            request: AuthStartRequest {
                method: Some(AuthMethod::DeviceCode),
                redirect_uri: Some("http://127.0.0.1:8080/callback".into()),
            },
        },
        ControlCommand::CompleteAuth {
            provider: provider.clone(),
            account: account.clone(),
            request: AuthCompleteRequest {
                flow_id: "flow-1".into(),
                authorization_code: Some("code-1".into()),
                redirect_uri: Some("http://127.0.0.1:8080/callback".into()),
            },
        },
        ControlCommand::AuthStatus {
            provider: provider.clone(),
            account: account.clone(),
        },
        ControlCommand::Logout {
            provider: provider.clone(),
            account: account.clone(),
            request: LogoutRequest {
                account_label: Some("work".into()),
            },
        },
        ControlCommand::ListWorkspaces {
            provider: provider.clone(),
            account: account.clone(),
        },
        ControlCommand::SelectWorkspace {
            provider: provider.clone(),
            account: account.clone(),
            workspace_id: "workspace-1".into(),
        },
        ControlCommand::RetireDuplicateAccounts {
            provider: provider.clone(),
            account: account.clone(),
        },
    ];
    for command in &commands {
        round_trip(command);
        round_trip(&ControlRequest::new("request-1", command.clone()));
        round_trip(&ControlRequest::new("request-1", command.clone()).with_diagnostics(true));
    }

    let observed_at = chrono::DateTime::parse_from_rfc3339("2026-08-27T12:00:00Z")
        .unwrap()
        .to_utc();
    let usage = SubscriptionUsage {
        provider: provider.clone(),
        account_label: Some("work".into()),
        plan: Some("pro".into()),
        subscription_expires_at: Some(observed_at),
        observed_at,
        windows: vec![UsageWindow {
            window: UsageWindowKind::Weekly,
            resets_at: Some(observed_at),
            measurements: vec![UsageMeasurement {
                name: "tokens".into(),
                used: 42.0,
                limit: Some(100.0),
                unit: MeasurementUnit::Tokens,
            }],
        }],
    };
    let outcome = QueryOutcome::Partial {
        data: usage,
        failures: vec![PartialFailure {
            scope: "weekly".into(),
            message: "temporarily unavailable".into(),
        }],
    };
    let account_payload = Account {
        id: account.clone(),
        provider: provider.clone(),
        label: Some("work".into()),
        enabled: true,
        metrics: vec!["usage".into()],
    };
    let workspace = ProviderWorkspace {
        id: "workspace-1".into(),
        label: Some("workspace".into()),
    };
    let sanitized = [
        SanitizedErrorPayload::AuthenticationInvalid,
        SanitizedErrorPayload::RateLimited {
            retry_after_seconds: Some(30),
        },
        SanitizedErrorPayload::RateLimited {
            retry_after_seconds: None,
        },
        SanitizedErrorPayload::Network,
        SanitizedErrorPayload::ProtocolIncompatible,
        SanitizedErrorPayload::UnsupportedCapability,
        SanitizedErrorPayload::Timeout,
        SanitizedErrorPayload::Cancelled,
        SanitizedErrorPayload::ProviderNotFound,
        SanitizedErrorPayload::AccountNotFound,
        SanitizedErrorPayload::Storage,
    ];
    for error in &sanitized {
        round_trip(error);
    }

    let errors = [
        ControlError::Account(AccountError::NotFound(account.clone())),
        ControlError::Account(AccountError::Duplicate(account.clone())),
        ControlError::Provider(ProviderError::AuthenticationInvalid {
            message: "detail".into(),
        }),
        ControlError::Provider(ProviderError::RateLimited {
            message: "detail".into(),
            retry_after_seconds: Some(30),
        }),
        ControlError::Provider(ProviderError::Network {
            message: "detail".into(),
        }),
        ControlError::Provider(ProviderError::ProtocolIncompatible {
            message: "detail".into(),
        }),
        ControlError::Provider(ProviderError::UnsupportedCapability {
            capability: "workspace selection".into(),
        }),
        ControlError::Registry(RegistryError::Duplicate(provider.clone())),
        ControlError::Registry(RegistryError::NotFound(provider.clone())),
        ControlError::Registry(RegistryError::InstanceUnavailable(provider.clone())),
        ControlError::AccountNotFound {
            account_id: "account-1".into(),
        },
        ControlError::AccountSelectorNotFound {
            provider: provider.clone(),
            account_label: Some("work".into()),
        },
        ControlError::DeviceNotFound {
            device_id: "device-1".into(),
        },
        ControlError::InvalidAccountMetrics,
        ControlError::InvalidRequest,
        ControlError::Timeout,
        ControlError::Cancelled,
        ControlError::Storage,
        ControlError::UnsupportedCommand,
    ];
    for error in &errors {
        round_trip(error);
    }

    let mut results = vec![
        ControlResult::DaemonStatus(DaemonStatusPayload {
            shutting_down: true,
            credential_backend: CredentialBackendId::LinuxSecretService,
            accounts: vec![AccountStatusPayload {
                account_id: "account-1".into(),
                provider: provider.clone(),
                enabled: true,
                in_flight: true,
                consecutive_failures: 2,
                next_probe_at: Some(observed_at),
                has_snapshot: true,
                stale: true,
                last_error: Some(SanitizedErrorPayload::RateLimited {
                    retry_after_seconds: Some(30),
                }),
            }],
        }),
        ControlResult::PairCode(PairCodePayload {
            code: "ABC123".into(),
            expires_at: observed_at,
        }),
        ControlResult::Devices(vec![DevicePayload {
            id: "device-1".into(),
            name: "laptop".into(),
            created_at: observed_at,
            last_seen_at: observed_at,
        }]),
        ControlResult::Providers(vec![ProviderDescriptor {
            id: provider.clone(),
            display_name: "Claude".into(),
            capabilities: vec![Capability::UsageQuery, Capability::Other("x".into())],
        }]),
        ControlResult::Accounts(vec![account_payload.clone()]),
        ControlResult::Account(account_payload),
        ControlResult::Usage(outcome.clone()),
        ControlResult::Probe(ProbePayload {
            account_id: "account-1".into(),
            usage: outcome.clone(),
            metrics: vec!["usage".into()],
        }),
        ControlResult::Snapshots(vec![SnapshotPayload {
            account_id: "account-1".into(),
            usage: outcome.clone(),
            last_success_at: observed_at,
            stale: true,
            last_error: Some(SanitizedErrorPayload::Network),
            last_error_at: Some(observed_at),
            metrics: vec!["usage".into()],
        }]),
        ControlResult::AuthChallenge(AuthChallenge {
            flow_id: "flow-1".into(),
            method: AuthMethod::BrowserOAuth,
            verification_uri: Some("https://example.invalid/auth".into()),
            user_code: Some("CODE".into()),
            expires_at: Some(observed_at),
            input: Some(AuthInputRequest::visible("paste the code")),
        }),
        ControlResult::AuthChallenge(AuthChallenge {
            flow_id: "flow-2".into(),
            method: AuthMethod::DeviceCode,
            verification_uri: None,
            user_code: None,
            expires_at: None,
            input: None,
        }),
    ];
    results.extend(
        [
            AuthState::NotAuthenticated,
            AuthState::Pending {
                flow_id: "flow-1".into(),
                expires_at: Some(observed_at),
            },
            AuthState::Authenticated {
                account_label: Some("work".into()),
                account_key: Some("key-1".into()),
                expires_at: Some(observed_at),
            },
            AuthState::Invalid {
                reason: "expired".into(),
                account_key: Some("key-1".into()),
            },
        ]
        .into_iter()
        .map(ControlResult::AuthState),
    );
    results.extend([
        ControlResult::Workspaces(vec![workspace.clone()]),
        ControlResult::Workspace(workspace),
        ControlResult::Ack,
        ControlResult::ProtocolMismatch {
            supported_version: CONTROL_PROTOCOL_VERSION,
        },
    ]);
    results.extend(errors.into_iter().map(ControlResult::Error));
    for result in results {
        round_trip(&result);
        round_trip(&ControlResponse::new("request-1", result.clone()));
        round_trip(
            &ControlResponse::new("request-1", result)
                .with_diagnostic(Some("provider detail".into())),
        );
    }

    for backend in [
        CredentialBackendId::LinuxSecretService,
        CredentialBackendId::MacosKeychain,
        CredentialBackendId::WindowsCredentialManager,
        CredentialBackendId::FileFallback,
        CredentialBackendId::OtherPlatform,
    ] {
        round_trip(&backend);
    }
    for method in [
        AuthMethod::BrowserOAuth,
        AuthMethod::DeviceCode,
        AuthMethod::ApiToken,
        AuthMethod::SessionImport,
        AuthMethod::Other("custom".into()),
    ] {
        round_trip(&method);
    }
}

#[test]
fn daemon_status_serializes_the_credential_backend() {
    let response = ControlResponse {
        version: CONTROL_PROTOCOL_VERSION,
        request_id: "status-1".into(),
        diagnostic: None,
        daemon_version: Some("0.1.6".into()),
        result: ControlResult::DaemonStatus(DaemonStatusPayload {
            shutting_down: false,
            accounts: Vec::new(),
            credential_backend: CredentialBackendId::FileFallback,
        }),
    };
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(
        json["result"]["payload"]["credential_backend"],
        "file_fallback"
    );
    assert_eq!(
        serde_json::from_value::<ControlResponse>(json).unwrap(),
        response
    );
}

#[test]
fn authentication_probe_and_show_commands_accept_diagnostics() {
    assert!(
        ControlCommand::AuthStatus {
            provider: ProviderId::new("claude"),
            account: AccountId::new("account-1"),
        }
        .accepts_diagnostics()
    );
    assert!(
        ControlCommand::Probe {
            account_id: "account-1".into(),
            wait: true,
        }
        .accepts_diagnostics()
    );
    assert!(
        ControlCommand::Show {
            account_id: Some("account-1".into()),
        }
        .accepts_diagnostics()
    );
    assert!(ControlCommand::Show { account_id: None }.accepts_diagnostics());
    assert!(!ControlCommand::ListProviders.accepts_diagnostics());
    assert!(
        !ControlCommand::QueryUsage {
            provider: ProviderId::new("claude"),
            query: UsageQuery::default(),
        }
        .accepts_diagnostics()
    );
}

#[test]
fn diagnostics_are_opt_in_and_omitted_by_default() {
    let request = ControlRequest::new("request-3", ControlCommand::ListProviders);
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(encoded["diagnostics"], false);
    let decoded: ControlRequest = serde_json::from_value(serde_json::json!({
        "version": CONTROL_PROTOCOL_VERSION,
        "request_id": "legacy",
        "command": { "command": "list_providers" }
    }))
    .unwrap();
    assert!(!decoded.diagnostics);

    let response = ControlResponse::new("request-3", ControlResult::Ack);
    let encoded = serde_json::to_value(&response).unwrap();
    assert!(encoded.get("diagnostic").is_none());
    let with_detail = response.with_diagnostic(Some("provider detail".into()));
    assert_eq!(
        serde_json::to_value(&with_detail).unwrap()["diagnostic"],
        "provider detail"
    );
}

#[test]
fn daemon_version_defaults_to_absent_for_older_daemons() {
    let decoded: ControlResponse = serde_json::from_value(serde_json::json!({
        "version": CONTROL_PROTOCOL_VERSION,
        "request_id": "legacy",
        "result": { "result": "ack" }
    }))
    .unwrap();
    assert_eq!(decoded.daemon_version, None);

    let response = ControlResponse::new("request-4", ControlResult::Ack);
    assert_eq!(
        response.daemon_version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        serde_json::to_value(&response).unwrap()["daemon_version"],
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn registry_not_found_is_a_control_error() {
    let error = ControlError::from(RegistryError::NotFound(ProviderId::new("missing")));
    let json = serde_json::to_string(&error).unwrap();

    assert_eq!(serde_json::from_str::<ControlError>(&json).unwrap(), error);
}
