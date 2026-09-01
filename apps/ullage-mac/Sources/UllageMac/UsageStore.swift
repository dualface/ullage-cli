import AppKit
import Foundation
import Observation
import UllageKit

enum ConnectionState: Equatable {
    case idle
    case loading
    case connected
    case deviceNotPaired
    case unreachable
    case unauthorized
    case forbiddenHost
    case protocolMismatch(client: String, server: String)
    case noAccounts
}

@Observable
@MainActor
final class UsageStore {
    private(set) var accounts: [Account] = []
    private(set) var snapshots: [SnapshotPayload] = []
    private(set) var connectionState: ConnectionState = .idle
    private(set) var lastRefreshedAt: Date?
    private(set) var probingAccountIDs: Set<String> = []
    private(set) var rateLimitDeadlines: [String: Date] = [:]
    private(set) var currentTime = Date()

    private let dataSourceFactory: @MainActor () throws -> any UsageDataSource
    private var dataSource: (any UsageDataSource)?
    private var refreshTask: Task<Void, Never>?
    private var probeTasks: [String: Task<Void, Never>] = [:]
    private var timer: DispatchSourceTimer?
    private var countdownTimer: DispatchSourceTimer?
    private var wakeObserver: NSObjectProtocol?
    private(set) var isActive = false

    init(dataSourceFactory: @escaping @MainActor () throws -> any UsageDataSource) {
        self.dataSourceFactory = dataSourceFactory
    }

    func start() {
        guard !isActive else { return }
        isActive = true
        currentTime = Date()
        rateLimitDeadlines = rateLimitDeadlines.filter { $0.value > currentTime }
        if !rateLimitDeadlines.isEmpty { startCountdownTimerIfNeeded() }
        wakeObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didWakeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor in self?.refresh() }
        }
        scheduleTimer()
        refresh()
    }

    func stop() {
        isActive = false
        refreshTask?.cancel()
        refreshTask = nil
        probeTasks.values.forEach { $0.cancel() }
        probeTasks.removeAll()
        probingAccountIDs.removeAll()
        timer?.setEventHandler {}
        timer?.cancel()
        timer = nil
        countdownTimer?.setEventHandler {}
        countdownTimer?.cancel()
        countdownTimer = nil
        if let wakeObserver {
            NSWorkspace.shared.notificationCenter.removeObserver(wakeObserver)
            self.wakeObserver = nil
        }
    }

    func refresh() {
        guard isActive else { return }
        refreshTask?.cancel()
        refreshTask = Task { [weak self] in
            guard let self else { return }
            do {
                let source = try self.source()
                self.connectionState = .loading
                async let accounts = source.accounts()
                async let snapshots = source.usage()
                let result = try await (accounts, snapshots)
                try Task.checkCancellation()
                guard self.isActive else { return }
                self.accounts = sortedAccounts(result.0)
                self.snapshots = result.1
                self.lastRefreshedAt = Date()
                self.connectionState = self.accounts.isEmpty ? .noAccounts : .connected
            } catch is CancellationError {
                return
            } catch {
                guard self.isActive else { return }
                self.connectionState = Self.connectionState(for: error)
            }
        }
    }

    func probe(accountID: String) {
        guard isActive, !probingAccountIDs.contains(accountID), !isRateLimited(accountID) else { return }
        probeTasks[accountID] = Task { [weak self] in
            await self?.performProbe(accountID: accountID)
        }
    }

    func invalidateDataSource() {
        refreshTask?.cancel()
        dataSource = nil
        if isActive { refresh() }
    }

    private func performProbe(accountID: String) async {
        do {
            let source = try source()
            probingAccountIDs.insert(accountID)
            defer {
                probingAccountIDs.remove(accountID)
                probeTasks[accountID] = nil
            }
            let result = try await source.probe(accountId: accountID, wait: true)
            try Task.checkCancellation()
            guard isActive else { return }
            guard case .completed(let payload) = result else { return }
            if let index = snapshots.firstIndex(where: { $0.accountId == accountID }) {
                let old = snapshots[index]
                snapshots[index] = SnapshotPayload.replacingUsage(in: old, with: payload.usage)
            }
            lastRefreshedAt = Date()
        } catch let DaemonError.rateLimited(retryAfter, _) {
            rateLimitDeadlines[accountID] = Date().addingTimeInterval(retryAfter ?? 30)
            startCountdownTimerIfNeeded()
        } catch is CancellationError {
            return
        } catch {
            connectionState = Self.connectionState(for: error)
        }
    }

    func snapshot(for accountID: String) -> SnapshotPayload? {
        snapshots.first(where: { $0.accountId == accountID })
    }

    func isRateLimited(_ accountID: String, now: Date? = nil) -> Bool {
        let now = now ?? currentTime
        guard let deadline = rateLimitDeadlines[accountID] else { return false }
        return deadline > now
    }

    func rateLimitSeconds(_ accountID: String, now: Date? = nil) -> Int? {
        let now = now ?? currentTime
        guard let deadline = rateLimitDeadlines[accountID], deadline > now else { return nil }
        return max(1, Int(ceil(deadline.timeIntervalSince(now))))
    }

    private func source() throws -> any UsageDataSource {
        if let dataSource { return dataSource }
        let created = try dataSourceFactory()
        dataSource = created
        return created
    }

    private func scheduleTimer() {
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 30, repeating: 30, leeway: .seconds(5))
        timer.setEventHandler { [weak self] in self?.refresh() }
        timer.resume()
        self.timer = timer
    }

    private func startCountdownTimerIfNeeded() {
        guard isActive, countdownTimer == nil else { return }
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 1, repeating: 1)
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            self.currentTime = Date()
            self.rateLimitDeadlines = self.rateLimitDeadlines.filter { $0.value > self.currentTime }
            if self.rateLimitDeadlines.isEmpty {
                self.countdownTimer?.setEventHandler {}
                self.countdownTimer?.cancel()
                self.countdownTimer = nil
            }
        }
        timer.resume()
        countdownTimer = timer
    }

    private static func connectionState(for error: Error) -> ConnectionState {
        if error is DataSourceSetupError { return .deviceNotPaired }
        guard let error = error as? DaemonError else { return .unreachable }
        return switch error {
        case .unauthorized, .authenticationInvalid: .unauthorized
        case .forbiddenHost: .forbiddenHost
        case .protocolMismatch(let client, let server):
            .protocolMismatch(client: String(client), server: String(server))
        default: .unreachable
        }
    }
}

private extension SnapshotPayload {
    static func replacingUsage(
        in snapshot: SnapshotPayload,
        with usage: QueryOutcome<SubscriptionUsage>
    ) -> SnapshotPayload {
        SnapshotPayload(
            accountId: snapshot.accountId,
            usage: usage,
            lastSuccessAt: Date(),
            stale: false,
            lastError: nil,
            lastErrorAt: nil
        )
    }
}
