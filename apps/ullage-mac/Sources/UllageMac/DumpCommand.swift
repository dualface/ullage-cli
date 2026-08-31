import Foundation
import UllageKit

@MainActor
enum DumpCommand {
    static func run(mode: AppMode = .current) async -> Int32 {
        do {
            let source: any UsageDataSource
            switch mode {
            case .mock:
                source = MockDataSource()
            case .daemon:
                let settings = AppSettings()
                guard let token = try Keychain.loadToken(), !token.isEmpty else {
                    throw DataSourceSetupError.tokenNotConfigured
                }
                source = DaemonClient(baseURL: settings.serverURL, token: token)
            }

            async let accounts = source.accounts()
            async let snapshots = source.usage()
            let output = dumpOutput(accounts: try await accounts, snapshots: try await snapshots)
            print(output)
            return 0
        } catch let error as DaemonError {
            writeDumpError(error.description)
            return 1
        } catch is DataSourceSetupError {
            writeDumpError("token not configured")
            return 1
        } catch {
            // Keychain and other setup failures are not `DaemonError`s; keep
            // the underlying description so a failed dump is diagnosable.
            writeDumpError("could not load daemon usage: \(error)")
            return 1
        }
    }
}

func dumpOutput(accounts: [Account], snapshots: [SnapshotPayload]) -> String {
    let accounts = sortedAccounts(accounts)
    let snapshotsByID = snapshots.reduce(into: [String: SnapshotPayload]()) { result, snapshot in
        result[snapshot.accountId] = snapshot
    }
    var lines = ["OVERVIEW"]

    for account in accounts {
        guard let snapshot = snapshotsByID[account.id], let usage = snapshot.usage.data else { continue }
        lines.append("[\(providerDisplayName(account.provider))] \(dumpBadges(account, snapshot, usage))")
        lines.append(contentsOf: overviewRows(for: usage).map { "  " + dumpRow($0) })
    }

    lines.append("ACCOUNTS")
    for account in accounts {
        guard let snapshot = snapshotsByID[account.id], let usage = snapshot.usage.data else { continue }
        lines.append("[\(providerDisplayName(account.provider))] \(dumpBadges(account, snapshot, usage))")
        lines.append(contentsOf: summarize(usage).rows.map { "  " + dumpRow($0) })
    }
    return lines.joined(separator: "\n")
}

private func dumpRow(_ row: SummaryRow) -> String {
    let reset = row.resetsAt.map { " resets=\($0.ISO8601Format())" } ?? " resets=-"
    let disabled = row.disabled ? " off" : ""
    return "\(row.window) | \(row.metric) | \(summaryValueText(row.value))\(disabled)\(reset)"
}

private func dumpBadges(
    _ account: Account,
    _ snapshot: SnapshotPayload,
    _ usage: SubscriptionUsage
) -> String {
    let badges = badgeNames(account: account, snapshot: snapshot, summary: summarize(usage))
    return "badges=" + (badges.isEmpty ? "-" : badges.joined(separator: ","))
}

private func writeDumpError(_ message: String) {
    FileHandle.standardError.write(Data((message + "\n").utf8))
}
