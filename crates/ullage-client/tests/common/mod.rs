#![allow(dead_code)]

use std::sync::Mutex;

use chrono::{TimeZone, Utc};
use ullage_cli::{ClientError, ControlClient, ServiceAction};
use ullage_protocol::{
    Account, AccountId, CONTROL_PROTOCOL_VERSION, ControlCommand, ControlRequest, ControlResponse,
    ControlResult, MeasurementUnit, PartialFailure, ProviderId, QueryOutcome, SnapshotPayload,
    SubscriptionUsage, UsageMeasurement, UsageWindow, UsageWindowKind,
};

pub type Responder = fn(&ControlRequest) -> Result<ControlResponse, ClientError>;

pub struct MockClient {
    responder: Responder,
    pub requests: Mutex<Vec<ControlCommand>>,
    daemon_result: Result<(), ClientError>,
}

impl MockClient {
    pub fn new(responder: Responder) -> Self {
        Self {
            responder,
            requests: Mutex::new(Vec::new()),
            daemon_result: Ok(()),
        }
    }

    pub fn with_daemon_result(daemon_result: Result<(), ClientError>) -> Self {
        Self {
            responder: |_| unreachable!(),
            requests: Mutex::new(Vec::new()),
            daemon_result,
        }
    }
}

impl ControlClient for MockClient {
    fn send(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        self.requests.lock().unwrap().push(request.command.clone());
        (self.responder)(request)
    }

    fn run_daemon(&self) -> Result<(), ClientError> {
        match &self.daemon_result {
            Ok(()) => Ok(()),
            Err(error) => Err(match error {
                ClientError::DaemonProcess => ClientError::DaemonProcess,
                ClientError::DaemonUnavailable => ClientError::DaemonUnavailable,
                ClientError::InvalidResponse => ClientError::InvalidResponse,
                ClientError::InvalidEndpoint => ClientError::InvalidEndpoint,
                ClientError::DaemonProcessOutput(detail) => {
                    ClientError::DaemonProcessOutput(detail.clone())
                }
                ClientError::DaemonStillRunning => ClientError::DaemonStillRunning,
            }),
        }
    }

    fn manage_daemon(&self, _: ServiceAction) -> Result<(), ClientError> {
        self.run_daemon()
    }
}

pub struct StoppedServiceClient {
    pub installed: bool,
}

impl ControlClient for StoppedServiceClient {
    fn send(&self, _: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Err(ClientError::DaemonUnavailable)
    }

    fn daemon_service_installed(&self) -> Result<bool, ClientError> {
        Ok(self.installed)
    }
}

pub fn response(request: &ControlRequest, result: ControlResult) -> ControlResponse {
    ControlResponse {
        version: CONTROL_PROTOCOL_VERSION,
        request_id: request.request_id.clone(),
        result,
        diagnostic: None,
        daemon_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
    }
}

pub fn empty_usage() -> SubscriptionUsage {
    SubscriptionUsage {
        provider: ProviderId::new("claude"),
        account_label: Some("person@example.test".into()),
        plan: Some("pro".into()),
        subscription_expires_at: None,
        observed_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
        windows: Vec::new(),
    }
}

pub fn snapshot(account_id: &str, usage: QueryOutcome<SubscriptionUsage>) -> SnapshotPayload {
    SnapshotPayload {
        account_id: account_id.into(),
        usage,
        last_success_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
        stale: false,
        last_error: None,
        last_error_at: None,
        metrics: Vec::new(),
    }
}

pub fn snapshot_result(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
    Ok(response(
        request,
        ControlResult::Snapshots(vec![snapshot(
            "primary",
            QueryOutcome::Complete {
                data: empty_usage(),
            },
        )]),
    ))
}

pub fn account_list_result(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
    Ok(response(
        request,
        ControlResult::Accounts(vec![Account {
            id: AccountId::new("primary"),
            provider: ProviderId::new("claude"),
            label: None,
            enabled: true,
            metrics: Vec::new(),
        }]),
    ))
}

pub fn summarizable_usage() -> SubscriptionUsage {
    let now = Utc::now();
    let mut usage = empty_usage();
    usage.observed_at = now;
    usage.windows = vec![
        UsageWindow {
            window: UsageWindowKind::FiveHours,
            resets_at: Some(now + chrono::Duration::minutes(238)),
            measurements: vec![UsageMeasurement {
                name: "included_usage".into(),
                used: 3.0,
                limit: Some(100.0),
                unit: MeasurementUnit::Percent,
            }],
        },
        UsageWindow {
            window: UsageWindowKind::Other {
                id: "seven_day_opus".into(),
                label: "Weekly Opus".into(),
            },
            resets_at: Some(now + chrono::Duration::minutes(8130)),
            measurements: vec![UsageMeasurement {
                name: "included_usage".into(),
                used: 11.0,
                limit: Some(100.0),
                unit: MeasurementUnit::Percent,
            }],
        },
    ];
    usage
}

pub fn summary_result(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
    Ok(response(
        request,
        ControlResult::Snapshots(vec![snapshot(
            "primary",
            QueryOutcome::Complete {
                data: summarizable_usage(),
            },
        )]),
    ))
}

pub fn line_starting_with<'a>(stdout: &'a str, prefix: &str) -> &'a str {
    stdout
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line starting with {prefix:?}:\n{stdout}"))
}

pub fn partial_profile_incompatible(
    request: &ControlRequest,
) -> Result<ControlResponse, ClientError> {
    Ok(response(
        request,
        ControlResult::Snapshots(vec![snapshot(
            "primary",
            QueryOutcome::Partial {
                data: summarizable_usage(),
                failures: vec![PartialFailure {
                    scope: "profile".into(),
                    message: "provider protocol response is incompatible".into(),
                }],
            },
        )]),
    ))
}

pub fn account_block<'a>(stdout: &'a str, account: &str) -> &'a str {
    let marker = format!("- {account} ====");
    let marker_end = stdout
        .find(&marker)
        .map(|offset| offset + marker.len())
        .unwrap_or_else(|| panic!("no section for {account:?}:\n{stdout}"));
    let start = stdout[..marker_end]
        .rfind("==== ")
        .expect("section header start");
    let end = stdout[marker_end..]
        .find("\n==== ")
        .map(|offset| marker_end + offset + 1)
        .unwrap_or(stdout.len());
    &stdout[start..end]
}

pub fn snapshot_with_metrics(
    account_id: &str,
    data: SubscriptionUsage,
    metrics: Vec<String>,
) -> SnapshotPayload {
    let mut snapshot = snapshot(account_id, QueryOutcome::Complete { data });
    snapshot.metrics = metrics;
    snapshot
}

/// Usage with two named metrics in different windows, plus a reached limit that
/// must survive every filter.
pub fn metric_usage() -> SubscriptionUsage {
    let mut usage = empty_usage();
    usage.windows = vec![
        UsageWindow {
            window: UsageWindowKind::FiveHours,
            resets_at: Some(Utc.with_ymd_and_hms(2026, 8, 27, 15, 57, 0).unwrap()),
            measurements: vec![
                UsageMeasurement {
                    name: "included_usage".into(),
                    used: 3.0,
                    limit: Some(100.0),
                    unit: MeasurementUnit::Percent,
                },
                UsageMeasurement {
                    name: "limit_reached".into(),
                    used: 1.0,
                    limit: Some(1.0),
                    unit: MeasurementUnit::Percent,
                },
            ],
        },
        UsageWindow {
            window: UsageWindowKind::Other {
                id: "seven_day_opus".into(),
                label: "Weekly Opus".into(),
            },
            resets_at: Some(Utc.with_ymd_and_hms(2026, 9, 5, 10, 0, 0).unwrap()),
            measurements: vec![UsageMeasurement {
                name: "codex_usage".into(),
                used: 11.0,
                limit: Some(100.0),
                unit: MeasurementUnit::Percent,
            }],
        },
    ];
    usage
}
