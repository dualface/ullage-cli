import AppKit
import Foundation
import Observation
import SwiftUI
import UllageKit

@MainActor
@Observable
final class LocalAccountManager {
    private let dataChanged: () -> Void
    private let clientFactory: (TimeInterval) throws -> LocalControlClient
    private let readinessWindow: Duration
    private let readinessRetryDelay: Duration
    private(set) var providers: [ProviderDescriptor] = []
    private(set) var accounts: [Account] = []
    private(set) var isLoading = false
    private(set) var message = ""

    init(
        localService: LocalServiceManager,
        dataChanged: @escaping () -> Void,
        clientFactory: ((TimeInterval) throws -> LocalControlClient)? = nil,
        readinessWindow: Duration = .seconds(5),
        readinessRetryDelay: Duration = .milliseconds(200)
    ) {
        self.dataChanged = dataChanged
        self.readinessWindow = readinessWindow
        self.readinessRetryDelay = readinessRetryDelay
        self.clientFactory = clientFactory ?? { timeout in
            guard let socketURL = localService.socketURL else {
                throw LocalControlError.unavailable("App Group container is unavailable")
            }
            return LocalControlClient(socketURL: socketURL, timeout: timeout)
        }
    }

    func client(timeout: TimeInterval = 10) throws -> LocalControlClient {
        try clientFactory(timeout)
    }

    func refresh(waitForService: Bool = false) async {
        isLoading = true
        defer { isLoading = false }
        let clock = ContinuousClock()
        let deadline = waitForService ? clock.now.advanced(by: readinessWindow) : nil
        var lastStartingError: Error?
        while true {
            if let deadline, clock.now >= deadline, let lastStartingError {
                message = lastStartingError.localizedDescription
                return
            }
            do {
                let client = try client(timeout: deadline.map {
                    max(0.001, durationSeconds(clock.now.duration(to: $0)))
                } ?? 10)
                async let availableProviders = client.providers()
                async let configuredAccounts = client.accounts()
                providers = try await availableProviders
                accounts = try await configuredAccounts
                message = ""
                return
            } catch is CancellationError {
                return
            } catch {
                guard let deadline,
                      isServiceStarting(error),
                      clock.now < deadline else {
                    message = error.localizedDescription
                    return
                }
                lastStartingError = error
                let remaining = clock.now.duration(to: deadline)
                do {
                    try await Task.sleep(for: min(readinessRetryDelay, remaining))
                } catch {
                    return
                }
            }
        }
    }

    func setEnabled(_ account: Account, enabled: Bool) async {
        do {
            _ = try await client().setAccountEnabled(account.id, enabled: enabled)
            await refresh()
            dataChanged()
        } catch {
            message = error.localizedDescription
        }
    }

    func delete(_ account: Account) async {
        let client: LocalControlClient
        do {
            client = try self.client()
            try await client.logout(
                provider: account.provider,
                account: account.id,
                accountLabel: account.label
            )
        } catch {
            message = "Account was kept because logout failed: \(error.localizedDescription)"
            return
        }
        do {
            try await client.removeAccount(account.id)
            await refresh()
            dataChanged()
        } catch {
            message = "Signed out, but the account could not be removed: \(error.localizedDescription)"
        }
    }

    func didCompleteLogin() async {
        await refresh()
        dataChanged()
    }

    private func isServiceStarting(_ error: Error) -> Bool {
        guard let error = error as? LocalControlError else { return false }
        return switch error {
        case .unavailable, .timeout: true
        default: false
        }
    }

    private func durationSeconds(_ duration: Duration) -> TimeInterval {
        let components = duration.components
        return Double(components.seconds)
            + Double(components.attoseconds) / 1_000_000_000_000_000_000
    }
}

@MainActor
@Observable
final class LoginWizardModel {
    private let manager: LocalAccountManager
    private let pollingInterval: Duration
    private(set) var account: Account?
    private(set) var challenge: AuthenticationChallenge?
    private(set) var authenticated = false
    private(set) var setupCompleted = false
    private(set) var busy = false
    private(set) var message = ""
    private var pollingTask: Task<Void, Never>?
    private var authenticatedAccountLabel: String?

    var provider = ""
    var label = ""
    var input = ""

    init(manager: LocalAccountManager, pollingInterval: Duration = .seconds(2)) {
        self.manager = manager
        self.pollingInterval = pollingInterval
    }

    func begin() async {
        guard !provider.isEmpty else {
            message = "Choose a provider."
            return
        }
        busy = true
        defer { busy = false }
        do {
            let client = try manager.client()
            let created = try await client.addAccount(
                provider: provider,
                label: label.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
            )
            account = created
            let started = try await client.startAuthentication(
                provider: created.provider,
                account: created.id
            )
            challenge = started
            message = ""
            if let address = started.verificationURI.flatMap(URL.init(string:)) {
                NSWorkspace.shared.open(address)
            }
            if started.method == .deviceCode {
                startPolling()
            }
        } catch {
            message = setupErrorMessage(error)
            await rollbackTemporaryAccount()
        }
    }

    func submit() async {
        guard let account, let challenge else { return }
        busy = true
        defer { busy = false }
        do {
            let state = try await manager.client().completeAuthentication(
                provider: account.provider,
                account: account.id,
                flowId: challenge.flowId,
                input: challenge.input == nil ? nil : input,
                redirectURI: callbackOrigin(input)
            )
            try await handle(state)
        } catch {
            message = setupErrorMessage(error)
        }
    }

    func retry() async {
        guard let account else {
            await begin()
            return
        }
        busy = true
        defer { busy = false }
        do {
            let started = try await manager.client().startAuthentication(
                provider: account.provider,
                account: account.id
            )
            input = ""
            challenge = started
            message = ""
            if let address = started.verificationURI.flatMap(URL.init(string:)) {
                NSWorkspace.shared.open(address)
            }
            if started.method == .deviceCode { startPolling() }
        } catch {
            message = error.localizedDescription
        }
    }

    func cancel() async {
        pollingTask?.cancel()
        await rollbackTemporaryAccount()
    }

    private func startPolling() {
        pollingTask?.cancel()
        let interval = pollingInterval
        pollingTask = Task { [weak self, interval] in
            var attempts = 0
            while !Task.isCancelled {
                do {
                    try await Task.sleep(for: interval)
                } catch {
                    return
                }
                guard let self, let account = self.account, let challenge = self.challenge else {
                    return
                }
                if challenge.expiresAt.map({ $0 <= Date() }) == true
                    || (challenge.expiresAt == nil && attempts >= 60) {
                    self.message = "The authorization expired. Retry to start again."
                    return
                }
                attempts += 1
                do {
                    let completed = try await self.manager.client().completeAuthentication(
                        provider: account.provider,
                        account: account.id,
                        flowId: challenge.flowId,
                        input: nil
                    )
                    if case .authenticated = completed {
                        try await self.handle(completed)
                        return
                    }
                } catch {
                    if !Task.isCancelled { self.message = self.setupErrorMessage(error) }
                }
            }
        }
    }

    private func handle(_ state: AuthenticationState) async throws {
        switch state {
        case .authenticated(let accountLabel, _):
            guard let account else { return }
            authenticated = true
            authenticatedAccountLabel = accountLabel
            await manager.didCompleteLogin()
            try await finishAuthenticatedSetup(for: account)
        case .pending:
            message = "Waiting for authorization…"
        case .notAuthenticated:
            message = "Authorization was not completed."
        case .invalid(let reason):
            message = reason
        }
    }

    func retryAuthenticatedSetup() async {
        guard authenticated, let account else { return }
        busy = true
        defer { busy = false }
        do {
            try await finishAuthenticatedSetup(for: account)
        } catch {
            message = setupErrorMessage(error)
        }
    }

    private func finishAuthenticatedSetup(for account: Account) async throws {
        let client = try manager.client()
        let verified = try await client.authenticationStatus(
            provider: account.provider,
            account: account.id
        )
        guard case .authenticated(let verifiedLabel, _) = verified else {
            throw LocalControlError.server("authentication verification did not succeed")
        }
        let discoveredLabel = authenticatedAccountLabel ?? verifiedLabel
        if label.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
           let discoveredLabel,
           !discoveredLabel.isEmpty {
            _ = try await client.setAccountLabel(account.id, label: discoveredLabel)
        }
        _ = try await client.probe(accountId: account.id, wait: true)
        setupCompleted = true
        message = "Account connected."
        await manager.didCompleteLogin()
    }

    private func setupErrorMessage(_ error: Error) -> String {
        authenticated
            ? "Signed in, but setup is incomplete: \(error.localizedDescription)"
            : error.localizedDescription
    }

    private func rollbackTemporaryAccount() async {
        guard let account, !authenticated else { return }
        let client: LocalControlClient
        do {
            client = try manager.client()
        } catch {
            message = "Temporary account could not be removed: \(error.localizedDescription)"
            return
        }
        try? await client.logout(provider: account.provider, account: account.id, accountLabel: nil)
        do {
            try await client.removeAccount(account.id)
            self.account = nil
            await manager.refresh()
        } catch {
            message = "Temporary account could not be removed: \(error.localizedDescription)"
        }
    }
}

struct LocalAccountsSection: View {
    @Bindable var manager: LocalAccountManager
    @State private var showsWizard = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Accounts").font(.headline)
                Spacer()
                Button("Add Account…") { showsWizard = true }
                    .disabled(manager.providers.isEmpty)
            }
            if manager.isLoading {
                ProgressView().controlSize(.small)
            } else if manager.accounts.isEmpty {
                Text("No local accounts yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(manager.accounts, id: \.id) { account in
                    HStack {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(account.label?.nilIfEmpty ?? account.provider.capitalized)
                            Text(account.provider).font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                        Toggle("Enabled", isOn: Binding(
                            get: { account.enabled },
                            set: { enabled in Task { await manager.setEnabled(account, enabled: enabled) } }
                        ))
                        .labelsHidden()
                        Button("Delete", role: .destructive) {
                            Task { await manager.delete(account) }
                        }
                    }
                }
            }
            if !manager.message.isEmpty {
                Text(manager.message).font(.caption).foregroundStyle(.secondary)
            }
            if manager.providers.isEmpty, !manager.isLoading {
                Button("Retry Local Service") {
                    Task { await manager.refresh(waitForService: true) }
                }
            }
        }
        .sheet(isPresented: $showsWizard, onDismiss: { Task { await manager.refresh() } }) {
            LoginWizardView(manager: manager)
        }
    }
}

struct LoginWizardView: View {
    @Environment(\.dismiss) private var dismiss
    @State private var model: LoginWizardModel
    let providers: [ProviderDescriptor]

    init(manager: LocalAccountManager) {
        _model = State(initialValue: LoginWizardModel(manager: manager))
        providers = manager.providers
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add Local Account").font(.title2.bold())
            if model.account == nil {
                Picker("Provider", selection: $model.provider) {
                    Text("Choose…").tag("")
                    ForEach(providers) { provider in
                        Text(provider.displayName).tag(provider.id)
                    }
                }
                TextField("Account name (optional)", text: $model.label)
            } else if let challenge = model.challenge {
                challengeView(challenge)
            }
            if !model.message.isEmpty {
                Text(model.message).font(.callout).foregroundStyle(.secondary)
            }
            HStack {
                Button(model.authenticated ? "Close" : "Cancel") {
                    Task { await model.cancel(); dismiss() }
                }
                Spacer()
                if model.setupCompleted {
                    Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
                } else if model.authenticated {
                    Button("Retry Setup") { Task { await model.retryAuthenticatedSetup() } }
                        .disabled(model.busy)
                } else if model.account == nil {
                    Button("Continue") { Task { await model.begin() } }
                        .disabled(model.busy || model.provider.isEmpty)
                        .keyboardShortcut(.defaultAction)
                } else {
                    Button("Retry") { Task { await model.retry() } }.disabled(model.busy)
                    if model.challenge?.method != .deviceCode {
                        Button("Connect") { Task { await model.submit() } }
                            .disabled(model.busy || (model.challenge?.input != nil && model.input.isEmpty))
                            .keyboardShortcut(.defaultAction)
                    }
                }
                if model.busy { ProgressView().controlSize(.small) }
            }
        }
        .padding(20)
        .frame(width: 440)
        .onDisappear { Task { await model.cancel() } }
    }

    @ViewBuilder
    private func challengeView(_ challenge: AuthenticationChallenge) -> some View {
        Text("Complete sign-in in your browser.")
        if let code = challenge.userCode {
            Text(code).font(.title.monospaced().bold()).textSelection(.enabled)
        }
        if let input = challenge.input {
            Text(input.prompt).font(.caption).foregroundStyle(.secondary)
            if input.secret {
                SecureField(input.prompt, text: $model.input)
            } else {
                TextField(input.prompt, text: $model.input)
            }
        } else {
            Text("This window will update automatically.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        if let expiresAt = challenge.expiresAt {
            Text("Expires \(expiresAt.formatted(date: .omitted, time: .shortened))")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}

func callbackOrigin(_ value: String) -> String? {
    guard let components = URLComponents(string: value),
          let scheme = components.scheme?.lowercased(),
          scheme == "http" || scheme == "https",
          let host = components.host,
          !host.isEmpty else { return nil }
    var origin = URLComponents()
    origin.scheme = scheme
    origin.host = host
    origin.port = components.port
    return origin.string
}
