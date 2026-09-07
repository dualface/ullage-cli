//! Interactive `ullage auth login`.
//!
//! One command walks the whole flow: pick a provider, pick or create an
//! account, open the authorization URL, hand the callback value back, verify the
//! result, and name the account. Every question goes through [`Prompt`], which
//! only ever writes to an attached terminal.

use std::time::Duration;

use ullage_protocol::{
    Account, AccountId, AuthCompleteRequest, AuthState, CONTROL_PROTOCOL_VERSION, Capability,
    ControlCommand, ControlRequest, ControlResult, ProviderDescriptor, ProviderId,
};

use crate::prompt::{Prompt, SecretInput};
use crate::{
    AccountCommand, AuthCommand, AuthMethodArg, Cli, ClientError, Command, ControlClient,
    DaemonCommand, ExitCode, OutputFormat, ProbeArgs, ProviderCommand, RunOutput,
    control_error_kind, error_output_with_options, is_unsafe_control, next_request_id,
    render_result, response_matches_command, result_exit_code, to_control_command,
};

/// How many times a malformed answer is re-asked before the flow gives up.
const MAXIMUM_ATTEMPTS: usize = 5;
/// How long to wait between polls of a device-code flow.
#[cfg(not(test))]
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(1);
/// How many times a device-code flow is polled before giving up.
const DEVICE_POLL_LIMIT: usize = 60;

/// A round trip that did not produce a usable result.
enum CallFailure {
    /// The daemon could not be reached, or answered something untrustworthy.
    Transport { kind: &'static str, code: ExitCode },
    /// The daemon answered with a well-formed error.
    Control {
        kind: &'static str,
        code: ExitCode,
        detail: Option<String>,
        error: ullage_protocol::ControlError,
    },
}

impl CallFailure {
    fn kind(&self) -> &'static str {
        match self {
            Self::Transport { kind, .. } | Self::Control { kind, .. } => kind,
        }
    }

    fn code(&self) -> ExitCode {
        match self {
            Self::Transport { code, .. } | Self::Control { code, .. } => *code,
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Self::Transport { .. } => None,
            Self::Control { detail, .. } => detail.as_deref(),
        }
    }

    fn at(self, stage: LoginStage) -> LoginError {
        LoginError::Call {
            stage,
            failure: self,
        }
    }
}

/// Stable name for the step that failed, printed next to the error kind.
#[derive(Clone, Copy)]
enum LoginStage {
    Daemon,
    ProviderSelection,
    Account,
    StartAuth,
    CompleteAuth,
    Verification,
    AccountLabel,
}

impl LoginStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::ProviderSelection => "provider_selection",
            Self::Account => "account",
            Self::StartAuth => "start_auth",
            Self::CompleteAuth => "complete_auth",
            Self::Verification => "verification",
            Self::AccountLabel => "account_label",
        }
    }
}

/// Anything that ends the flow before the account is named and returned.
enum LoginError {
    /// The daemon round trip failed.
    Call {
        stage: LoginStage,
        failure: CallFailure,
    },
    /// The user pressed Ctrl-D, or answered nothing where an answer was needed.
    Aborted { stage: LoginStage },
    /// The daemon answered correctly but the answer cannot carry the flow on.
    Flow {
        stage: LoginStage,
        kind: &'static str,
        code: ExitCode,
        detail: Option<String>,
    },
    /// Secret input was required but the terminal could not hide it.
    EchoUnavailable,
}

impl LoginError {
    fn stage(&self) -> LoginStage {
        match self {
            Self::Call { stage, .. } | Self::Aborted { stage, .. } | Self::Flow { stage, .. } => {
                *stage
            }
            Self::EchoUnavailable => LoginStage::CompleteAuth,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Call { failure, .. } => failure.kind(),
            Self::Aborted { .. } => "cancelled",
            Self::Flow { kind, .. } => kind,
            Self::EchoUnavailable => "terminal_echo",
        }
    }

    fn code(&self) -> ExitCode {
        match self {
            Self::Call { failure, .. } => failure.code(),
            Self::Aborted { .. } | Self::EchoUnavailable => ExitCode::Failure,
            Self::Flow { code, .. } => *code,
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Self::Call { failure, .. } => failure.detail(),
            Self::Aborted { .. } | Self::EchoUnavailable => None,
            Self::Flow { detail, .. } => detail.as_deref(),
        }
    }

    fn envelope_hint(&self, diagnose: bool) -> Option<String> {
        let kind_hint = match self {
            Self::Call {
                failure: CallFailure::Control { error, .. },
                ..
            } => crate::error_hint_for_control(error),
            Self::Flow { kind, .. }
            | Self::Call {
                failure: CallFailure::Transport { kind, .. },
                ..
            } => crate::error_hint(kind).map(str::to_owned),
            Self::Aborted { .. } | Self::EchoUnavailable => None,
        };
        kind_hint.or_else(|| {
            if !diagnose && self.is_retryable() {
                Some("re-run with --diagnose for the provider's own error text".into())
            } else {
                None
            }
        })
    }

    /// Whether restarting the authorization flow could plausibly succeed.
    fn is_retryable(&self) -> bool {
        match self {
            Self::Call {
                failure: CallFailure::Control { .. },
                ..
            }
            | Self::Flow { .. } => true,
            Self::Call {
                failure: CallFailure::Transport { .. },
                ..
            }
            | Self::Aborted { .. }
            | Self::EchoUnavailable => false,
        }
    }

    fn already_reported_during_auth(&self) -> bool {
        matches!(
            self.stage(),
            LoginStage::StartAuth | LoginStage::CompleteAuth | LoginStage::Verification
        ) && !matches!(self, Self::Aborted { .. })
    }
}

struct Session<'a> {
    client: &'a dyn ControlClient,
    diagnose: bool,
}

impl Session<'_> {
    fn call(&self, command: &Command) -> Result<ControlResult, CallFailure> {
        self.call_with(command, to_control_command(command))
    }

    /// Sends `control` but validates the answer against `command`, for the
    /// requests the argument-driven command surface cannot express.
    fn call_with(
        &self,
        command: &Command,
        control: ControlCommand,
    ) -> Result<ControlResult, CallFailure> {
        let request_id = next_request_id();
        let request = ControlRequest::new(&request_id, control.clone())
            .with_diagnostics(self.diagnose && control.accepts_diagnostics());
        let response = match self.client.send(&request) {
            Ok(response) => response,
            Err(ClientError::DaemonUnavailable) => {
                return Err(CallFailure::Transport {
                    kind: "daemon_unavailable",
                    code: ExitCode::DaemonUnavailable,
                });
            }
            Err(_) => return Err(Self::untrusted()),
        };
        let diagnostic_allowed = self.diagnose
            && control.accepts_diagnostics()
            && matches!(response.result, ControlResult::Error(_));
        let diagnostic_is_safe = response
            .diagnostic
            .as_deref()
            .is_none_or(|detail| !detail.chars().any(is_unsafe_control));
        if response.version != CONTROL_PROTOCOL_VERSION
            || response.request_id != request_id
            || !response_matches_command(command, &response.result)
            || (response.diagnostic.is_some() && !diagnostic_allowed)
            || !diagnostic_is_safe
        {
            return Err(Self::untrusted());
        }
        match response.result {
            ControlResult::Error(error) => Err(CallFailure::Control {
                kind: control_error_kind(&error),
                code: result_exit_code(&ControlResult::Error(error.clone())),
                detail: response.diagnostic,
                error,
            }),
            ControlResult::ProtocolMismatch { .. } => Err(CallFailure::Transport {
                kind: "protocol_mismatch",
                code: ExitCode::ProtocolError,
            }),
            result => Ok(result),
        }
    }

    fn untrusted() -> CallFailure {
        CallFailure::Transport {
            kind: "invalid_daemon_response",
            code: ExitCode::ProtocolError,
        }
    }
}

pub(crate) fn interactive_login(
    client: &dyn ControlClient,
    prompt: &mut dyn Prompt,
    provider_hint: Option<&str>,
    method: Option<AuthMethodArg>,
    cli: &Cli,
) -> RunOutput {
    let format = cli.output;
    let reveal = cli.reveal;
    let diagnose = crate::diagnostics_requested(cli.diagnose);
    if !prompt.is_interactive() {
        return error_output_with_options(
            ExitCode::Usage,
            "not_interactive",
            format,
            None,
            Some(
                "`ullage auth login` without --account needs a terminal; for scripts use \
                 `ullage auth login <provider> --account <id>` followed by `ullage auth complete`"
                    .into(),
            ),
        );
    }
    let session = Session { client, diagnose };
    match run(&session, prompt, provider_hint, method) {
        Ok(account) => render_result(
            ControlResult::Account(account),
            format,
            reveal,
            cli.raw,
            false,
            cli.color,
        ),
        Err(error) => {
            if !error.already_reported_during_auth() {
                report(prompt, &error, diagnose);
            }
            let mut output = error_output_with_options(
                error.code(),
                error.kind(),
                format,
                None,
                error.envelope_hint(diagnose),
            );
            if format == OutputFormat::Table {
                output
                    .stderr
                    .push_str(&format!("stage: {}\n", error.stage().as_str()));
            }
            output
        }
    }
}

fn run(
    session: &Session<'_>,
    prompt: &mut dyn Prompt,
    provider_hint: Option<&str>,
    method: Option<AuthMethodArg>,
) -> Result<Account, LoginError> {
    // A dead daemon is by far the most common first failure, so name the fix.
    if let Err(failure) = session.call(&Command::Daemon {
        command: DaemonCommand::Status,
    }) {
        if failure.kind() == "daemon_unavailable" {
            prompt.tell("The Ullage daemon is not running.");
            prompt.tell(
                "Start it with `ullage daemon install` then `ullage daemon start`, or run \
                 `ullage daemon run` in another terminal.",
            );
        }
        return Err(failure.at(LoginStage::Daemon));
    }

    let provider = select_provider(session, prompt, provider_hint)?;
    let accounts = list_accounts(session, &provider.id)?;
    let selection = select_account(prompt, &provider, &accounts)?;

    let (account_id, mut current_label, created) = match selection {
        AccountSelection::Existing(index) => {
            let account = &accounts[index];
            (account.id.as_str().to_owned(), account.label.clone(), false)
        }
        AccountSelection::New => {
            let label = provisional_label(&provider.id, &accounts);
            let account = expect_account(
                session
                    .call(&Command::Account {
                        command: AccountCommand::Add {
                            provider: provider.id.as_str().to_owned(),
                            label: Some(label),
                        },
                    })
                    .map_err(|failure| failure.at(LoginStage::Account))?,
                LoginStage::Account,
            )?;
            (account.id.as_str().to_owned(), account.label, true)
        }
    };

    let outcome = (|| {
        let authenticated =
            authenticate(session, prompt, provider.id.as_str(), &account_id, method)?;

        prompt.tell("");
        prompt.tell(&format!(
            "Authenticated {} as {}.",
            provider.display_name, account_id
        ));
        // Before naming, not after: the name a provider discovers is the same
        // for both accounts, so the older row holds it until it is gone.
        retire_duplicates(session, prompt, provider.id.as_str(), &account_id);

        let default_label = if created {
            authenticated
                .filter(|label| !label.trim().is_empty())
                .or_else(|| current_label.clone())
                .unwrap_or_else(|| provisional_label(&provider.id, &accounts))
        } else {
            current_label
                .clone()
                .filter(|label| !label.trim().is_empty())
                .or_else(|| authenticated.filter(|label| !label.trim().is_empty()))
                .unwrap_or_else(|| provisional_label(&provider.id, &accounts))
        };
        let chosen = ask_label(prompt, &default_label)?;
        if current_label.as_deref() != Some(chosen.as_str()) {
            current_label = Some(chosen);
            return set_label(session, prompt, &account_id, current_label);
        }
        expect_account(
            session
                .call(&Command::Account {
                    command: AccountCommand::Show {
                        account: account_id.clone(),
                    },
                })
                .map_err(|failure| failure.at(LoginStage::AccountLabel))?,
            LoginStage::AccountLabel,
        )
    })();
    if let (true, Err(error)) = (created, &outcome) {
        discard_account(
            session,
            prompt,
            provider.id.as_str(),
            &account_id,
            error.stage(),
        );
    }
    outcome
}

enum AccountSelection {
    New,
    Existing(usize),
}

fn select_provider(
    session: &Session<'_>,
    prompt: &mut dyn Prompt,
    hint: Option<&str>,
) -> Result<ProviderDescriptor, LoginError> {
    let ControlResult::Providers(providers) = session
        .call(&Command::Provider {
            command: ProviderCommand::List,
        })
        .map_err(|failure| failure.at(LoginStage::ProviderSelection))?
    else {
        return Err(Session::untrusted().at(LoginStage::ProviderSelection));
    };
    let candidates: Vec<ProviderDescriptor> = providers
        .into_iter()
        .filter(|provider| provider.capabilities.contains(&Capability::Authentication))
        .collect();
    if candidates.is_empty() {
        return Err(LoginError::Flow {
            stage: LoginStage::ProviderSelection,
            kind: "unsupported_capability",
            code: ExitCode::Failure,
            detail: None,
        });
    }
    if let Some(hint) = hint {
        return candidates
            .into_iter()
            .find(|provider| provider.id.as_str() == hint)
            .ok_or(LoginError::Flow {
                stage: LoginStage::ProviderSelection,
                kind: "provider_registry_error",
                code: ExitCode::Failure,
                detail: None,
            });
    }
    if let [only] = candidates.as_slice() {
        prompt.tell(&format!("Using the only provider: {}.", only.display_name));
        return Ok(only.clone());
    }
    prompt.tell("Providers:");
    let labels: Vec<String> = candidates
        .iter()
        .map(|provider| format!("{} ({})", provider.display_name, provider.id.as_str()))
        .collect();
    let index = choose(prompt, "Provider", &labels, 0).ok_or(LoginError::Aborted {
        stage: LoginStage::ProviderSelection,
    })?;
    Ok(candidates[index].clone())
}

fn list_accounts(session: &Session<'_>, provider: &ProviderId) -> Result<Vec<Account>, LoginError> {
    let ControlResult::Accounts(accounts) = session
        .call(&Command::Account {
            command: AccountCommand::List,
        })
        .map_err(|failure| failure.at(LoginStage::Account))?
    else {
        return Err(Session::untrusted().at(LoginStage::Account));
    };
    Ok(accounts
        .into_iter()
        .filter(|account| &account.provider == provider)
        .collect())
}

fn select_account(
    prompt: &mut dyn Prompt,
    provider: &ProviderDescriptor,
    accounts: &[Account],
) -> Result<AccountSelection, LoginError> {
    if accounts.is_empty() {
        return Ok(AccountSelection::New);
    }
    prompt.tell(&format!("{} accounts:", provider.display_name));
    let mut labels = vec!["Add a new account".to_owned()];
    labels.extend(accounts.iter().map(|account| {
        format!(
            "{} ({})",
            account.id.as_str(),
            account.label.as_deref().unwrap_or("no label")
        )
    }));
    let index = choose(prompt, "Account", &labels, 0).ok_or(LoginError::Aborted {
        stage: LoginStage::Account,
    })?;
    Ok(if index == 0 {
        AccountSelection::New
    } else {
        AccountSelection::Existing(index - 1)
    })
}

/// Runs the authorization flow, restarting it as long as the user wants to
/// retry. Returns the account label the provider reported, if any.
fn authenticate(
    session: &Session<'_>,
    prompt: &mut dyn Prompt,
    provider: &str,
    account: &str,
    method: Option<AuthMethodArg>,
) -> Result<Option<String>, LoginError> {
    loop {
        match authenticate_once(session, prompt, provider, account, method) {
            Ok(label) => return Ok(label),
            Err(error) => {
                report(prompt, &error, session.diagnose);
                if !error.is_retryable() || !confirm(prompt, "Start the flow again?", false) {
                    return Err(error);
                }
            }
        }
    }
}

fn authenticate_once(
    session: &Session<'_>,
    prompt: &mut dyn Prompt,
    provider: &str,
    account: &str,
    method: Option<AuthMethodArg>,
) -> Result<Option<String>, LoginError> {
    let login = Command::Auth {
        command: AuthCommand::Login {
            provider: Some(provider.to_owned()),
            account: Some(account.to_owned()),
            method,
        },
    };
    let ControlResult::AuthChallenge(challenge) = session
        .call(&login)
        .map_err(|failure| failure.at(LoginStage::StartAuth))?
    else {
        return Err(Session::untrusted().at(LoginStage::StartAuth));
    };

    prompt.tell("");
    match challenge.verification_uri.as_deref() {
        Some(uri) => {
            prompt.tell("Open this URL in your browser and approve the request:");
            prompt.tell(&format!("  {uri}"));
        }
        None => prompt.tell("Approve the request in your browser."),
    }
    if let Some(code) = challenge.user_code.as_deref() {
        prompt.tell(&format!(
            "Enter this code when the page asks for it: {code}"
        ));
    }
    if let Some(expires_at) = challenge.expires_at {
        prompt.tell(&format!("The flow expires at {}.", expires_at.to_rfc3339()));
    }

    let complete = Command::Auth {
        command: AuthCommand::Complete {
            provider: provider.to_owned(),
            account: account.to_owned(),
            flow_id: challenge.flow_id.clone(),
            redirect_uri: None,
            authorization_code_env: String::new(),
        },
    };

    let state = match challenge.input.as_ref() {
        Some(input) => {
            let question = format!("Paste {}", input.prompt);
            let value = if input.secret {
                match prompt.ask_secret(&question) {
                    SecretInput::Answer(value) => value,
                    SecretInput::Eof => {
                        return Err(LoginError::Aborted {
                            stage: LoginStage::CompleteAuth,
                        });
                    }
                    SecretInput::EchoUnavailable => return Err(LoginError::EchoUnavailable),
                }
            } else {
                prompt.ask(&question).ok_or(LoginError::Aborted {
                    stage: LoginStage::CompleteAuth,
                })?
            };
            let value = value.trim().to_owned();
            if value.is_empty() || value.chars().any(is_unsafe_control) {
                return Err(LoginError::Aborted {
                    stage: LoginStage::CompleteAuth,
                });
            }
            let redirect_uri = callback_origin(&value);
            expect_state(
                session
                    .call_with(
                        &complete,
                        ControlCommand::CompleteAuth {
                            provider: ProviderId::new(provider),
                            account: AccountId::new(account),
                            request: AuthCompleteRequest {
                                flow_id: challenge.flow_id.clone(),
                                authorization_code: Some(value),
                                redirect_uri,
                            },
                        },
                    )
                    .map_err(|failure| failure.at(LoginStage::CompleteAuth))?,
                LoginStage::CompleteAuth,
            )?
        }
        None => {
            prompt.tell("Nothing to paste back. Approve the request, then continue.");
            prompt
                .ask("Press Enter once you have approved it")
                .ok_or(LoginError::Aborted {
                    stage: LoginStage::CompleteAuth,
                })?;
            poll_device_flow(
                session,
                prompt,
                &complete,
                provider,
                account,
                &challenge.flow_id,
                challenge.expires_at,
            )?
        }
    };

    let label = match state {
        AuthState::Authenticated { account_label, .. } => account_label,
        AuthState::Invalid { reason, .. } => {
            return Err(LoginError::Flow {
                stage: LoginStage::CompleteAuth,
                kind: "authentication_invalid",
                code: ExitCode::AuthenticationInvalid,
                detail: session.diagnose.then_some(reason),
            });
        }
        AuthState::Pending { .. } => {
            return Err(LoginError::Flow {
                stage: LoginStage::CompleteAuth,
                kind: "authentication_pending",
                code: ExitCode::Failure,
                detail: None,
            });
        }
        AuthState::NotAuthenticated => {
            return Err(LoginError::Flow {
                stage: LoginStage::CompleteAuth,
                kind: "authentication_invalid",
                code: ExitCode::AuthenticationInvalid,
                detail: None,
            });
        }
    };

    verify(session, prompt, provider, account)?;
    Ok(label)
}

/// Device-code flows have nothing to paste, so the daemon is polled until the
/// provider reports the approval.
fn poll_device_flow(
    session: &Session<'_>,
    prompt: &mut dyn Prompt,
    complete: &Command,
    provider: &str,
    account: &str,
    flow_id: &str,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<AuthState, LoginError> {
    let mut attempts = 0;
    loop {
        if challenge_expired(expires_at) || (expires_at.is_none() && attempts >= DEVICE_POLL_LIMIT)
        {
            return Err(LoginError::Flow {
                stage: LoginStage::CompleteAuth,
                kind: "timeout",
                code: ExitCode::Failure,
                detail: None,
            });
        }
        match session.call_with(
            complete,
            ControlCommand::CompleteAuth {
                provider: ProviderId::new(provider),
                account: AccountId::new(account),
                request: AuthCompleteRequest {
                    flow_id: flow_id.to_owned(),
                    authorization_code: None,
                    redirect_uri: None,
                },
            },
        ) {
            // A poll that could not reach the provider says nothing about the
            // authorization, which stays live until the flow expires. Only a
            // refusal ends the login.
            Err(CallFailure::Control {
                kind: "rate_limited" | "network_failure",
                ..
            }) => {}
            Err(failure) => return Err(failure.at(LoginStage::CompleteAuth)),
            Ok(result) => {
                let state = expect_state(result, LoginStage::CompleteAuth)?;
                if !matches!(state, AuthState::Pending { .. }) {
                    return Ok(state);
                }
            }
        }
        if attempts == 0 {
            prompt.tell("Waiting for the approval to land.");
        }
        attempts += 1;
        std::thread::sleep(DEVICE_POLL_INTERVAL);
    }
}

fn challenge_expired(expires_at: Option<chrono::DateTime<chrono::Utc>>) -> bool {
    let Some(expiry) = expires_at else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(i64::MAX);
    expiry.timestamp() <= now
}

/// Independent confirmation that the stored credential really works, rather
/// than trusting the state the completion call returned.
fn verify(
    session: &Session<'_>,
    prompt: &mut dyn Prompt,
    provider: &str,
    account: &str,
) -> Result<(), LoginError> {
    prompt.tell("Verifying the stored credential.");
    let status = Command::Auth {
        command: AuthCommand::Status {
            provider: provider.to_owned(),
            account: account.to_owned(),
        },
    };
    match expect_state(
        session
            .call(&status)
            .map_err(|failure| failure.at(LoginStage::Verification))?,
        LoginStage::Verification,
    )? {
        AuthState::Authenticated { .. } => Ok(()),
        AuthState::Invalid { reason, .. } => Err(LoginError::Flow {
            stage: LoginStage::Verification,
            kind: "authentication_invalid",
            code: ExitCode::AuthenticationInvalid,
            detail: session.diagnose.then_some(reason),
        }),
        AuthState::Pending { .. } | AuthState::NotAuthenticated => Err(LoginError::Flow {
            stage: LoginStage::Verification,
            kind: "authentication_invalid",
            code: ExitCode::AuthenticationInvalid,
            detail: None,
        }),
    }
}

fn ask_label(prompt: &mut dyn Prompt, default: &str) -> Result<String, LoginError> {
    for _ in 0..MAXIMUM_ATTEMPTS {
        let answer =
            prompt
                .ask(&format!("Account label [{default}]"))
                .ok_or(LoginError::Aborted {
                    stage: LoginStage::AccountLabel,
                })?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(default.to_owned());
        }
        if answer.chars().any(is_unsafe_control) {
            prompt.tell("Labels cannot contain control characters.");
            continue;
        }
        return Ok(answer.to_owned());
    }
    Err(LoginError::Aborted {
        stage: LoginStage::AccountLabel,
    })
}

fn set_label(
    session: &Session<'_>,
    prompt: &mut dyn Prompt,
    account_id: &str,
    mut label: Option<String>,
) -> Result<Account, LoginError> {
    for _ in 0..MAXIMUM_ATTEMPTS {
        let command = Command::Account {
            command: AccountCommand::Label {
                account: account_id.to_owned(),
                label: label.clone(),
            },
        };
        match session.call(&command) {
            Ok(result) => return expect_account(result, LoginStage::AccountLabel),
            Err(CallFailure::Control {
                kind: "account_duplicate",
                ..
            }) => {
                prompt.tell("That label already names another account for this provider.");
                let retry = ask_label(prompt, label.as_deref().unwrap_or(account_id))?;
                label = Some(retry);
            }
            Err(failure) => return Err(failure.at(LoginStage::AccountLabel)),
        }
    }
    Err(LoginError::Aborted {
        stage: LoginStage::AccountLabel,
    })
}

/// Asks the daemon to drop any other account signed in as this same person.
/// A failure here leaves both accounts, which the user can sort out; it must
/// never fail a sign-in that worked.
fn retire_duplicates(
    session: &Session<'_>,
    prompt: &mut dyn Prompt,
    provider: &str,
    account_id: &str,
) {
    let context = Command::Account {
        command: AccountCommand::List,
    };
    // A credential can authenticate and still fail every query. Nothing is
    // deleted until this account has actually answered one, so a sign-in that
    // does not work cannot take out the working row it was meant to replace.
    let probe = Command::Probe(ProbeArgs {
        account: account_id.to_owned(),
        wait: true,
    });
    if session
        .call_with(
            &probe,
            ControlCommand::Probe {
                account_id: account_id.to_owned(),
                wait: true,
            },
        )
        .is_err()
    {
        prompt.tell(
            "Signed in, but this account did not answer a usage query, so no other account was \
             replaced. Run `ullage probe` and then `ullage account list` to check for a duplicate.",
        );
        return;
    }
    let Ok(ControlResult::Accounts(retired)) = session.call_with(
        &context,
        ControlCommand::RetireDuplicateAccounts {
            provider: ProviderId::new(provider),
            account: AccountId::new(account_id),
        },
    ) else {
        prompt.tell(
            "Signed in, but other accounts could not be checked for the same sign-in. Run \
             `ullage account list` to see whether one is now a duplicate.",
        );
        return;
    };
    for account in retired {
        prompt.tell(&format!(
            "Removed {}, which was signed in as the same account.",
            account.id.as_str()
        ));
    }
}

/// Best-effort cleanup for an account this run created but did not finish
/// naming. Logout is attempted first so credentials are not orphaned; if logout
/// fails the account is kept so the user can retry cleanup with ordinary
/// commands.
fn discard_account(
    session: &Session<'_>,
    prompt: &mut dyn Prompt,
    provider: &str,
    account_id: &str,
    stage: LoginStage,
) {
    // A completion whose reply was lost still stored a credential, so at that
    // stage alone the daemon is asked what actually happened; discarding on no
    // answer would throw away a sign-in the user completed. Failing any later
    // stage means the sign-in did land and the user walked away from the rest,
    // which is the case this account is supposed to be cleaned up for.
    if matches!(stage, LoginStage::CompleteAuth) {
        let kept = match session.call(&Command::Auth {
            command: AuthCommand::Status {
                provider: provider.to_owned(),
                account: account_id.to_owned(),
            },
        }) {
            Ok(ControlResult::AuthState(AuthState::Authenticated { .. })) => Some("is signed in"),
            Ok(ControlResult::AuthState(_)) => None,
            _ => Some("could not be checked"),
        };
        if let Some(reason) = kept {
            prompt.tell(&format!(
                "{account_id} {reason}, so it was kept. Remove it with `ullage account remove \
                 {account_id}` if that is not wanted."
            ));
            return;
        }
    }
    if session
        .call(&Command::Auth {
            command: AuthCommand::Logout {
                provider: provider.to_owned(),
                account: account_id.to_owned(),
                account_label: None,
            },
        })
        .is_err()
    {
        prompt.tell(&format!(
            "Could not clear credentials for {account_id}; run `ullage auth logout {provider} \
             --account {account_id}` then `ullage account remove {account_id}`."
        ));
        return;
    }
    let removed = session.call(&Command::Account {
        command: AccountCommand::Remove {
            account: account_id.to_owned(),
        },
    });
    if removed.is_err() {
        prompt.tell(&format!(
            "Could not remove the unfinished account {account_id}; remove it with \
             `ullage account remove {account_id}`."
        ));
    }
}

fn report(prompt: &mut dyn Prompt, error: &LoginError, diagnose: bool) {
    if matches!(error, LoginError::Aborted { .. }) {
        return;
    }
    prompt.tell(&format!(
        "Authentication failed at {}: {}",
        error.stage().as_str(),
        error.kind()
    ));
    match error.detail() {
        Some(detail) => prompt.tell(&format!("Provider detail: {detail}")),
        None if !diagnose && error.is_retryable() => {
            prompt.tell("Re-run with --diagnose to see the provider's own error text.");
        }
        None => {}
    }
}

fn expect_account(result: ControlResult, stage: LoginStage) -> Result<Account, LoginError> {
    match result {
        ControlResult::Account(account) => Ok(account),
        _ => Err(Session::untrusted().at(stage)),
    }
}

fn expect_state(result: ControlResult, stage: LoginStage) -> Result<AuthState, LoginError> {
    match result {
        ControlResult::AuthState(state) => Ok(state),
        _ => Err(Session::untrusted().at(stage)),
    }
}

/// The first `<provider>-<n>` that no existing account of this provider uses.
fn provisional_label(provider: &ProviderId, accounts: &[Account]) -> String {
    for index in 1.. {
        let candidate = format!("{}-{index}", provider.as_str());
        if !accounts
            .iter()
            .any(|account| account.label.as_deref() == Some(candidate.as_str()))
        {
            return candidate;
        }
    }
    unreachable!("the candidate sequence is unbounded")
}

/// The redirect URI a pasted callback URL implies: scheme, authority and path,
/// with the query and fragment removed. `None` when the value is a bare code.
fn callback_origin(value: &str) -> Option<String> {
    let rest = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))?;
    let scheme = if value.starts_with("https://") {
        "https://"
    } else {
        "http://"
    };
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let origin = &rest[..end];
    (!origin.is_empty()).then(|| format!("{scheme}{origin}"))
}

fn choose(
    prompt: &mut dyn Prompt,
    question: &str,
    options: &[String],
    default_index: usize,
) -> Option<usize> {
    for (index, option) in options.iter().enumerate() {
        prompt.tell(&format!("  {}) {option}", index + 1));
    }
    for _ in 0..MAXIMUM_ATTEMPTS {
        let answer = prompt.ask(&format!("{question} [{}]", default_index + 1))?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Some(default_index);
        }
        if let Ok(choice) = answer.parse::<usize>() {
            if (1..=options.len()).contains(&choice) {
                return Some(choice - 1);
            }
        }
        prompt.tell(&format!("Enter a number between 1 and {}.", options.len()));
    }
    None
}

fn confirm(prompt: &mut dyn Prompt, question: &str, default: bool) -> bool {
    let hint = if default { "Y/n" } else { "y/N" };
    let Some(answer) = prompt.ask(&format!("{question} [{hint}]")) else {
        return false;
    };
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::challenge_expired;
    use chrono::{TimeDelta, Utc};

    #[test]
    fn challenge_expiry_uses_the_provider_deadline_when_present() {
        assert!(!challenge_expired(None));
        assert!(challenge_expired(Some(Utc::now() - TimeDelta::seconds(1))));
        assert!(!challenge_expired(Some(
            Utc::now() + TimeDelta::seconds(600)
        )));
    }
}
