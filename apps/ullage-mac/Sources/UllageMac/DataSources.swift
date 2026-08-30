import Foundation
import UllageKit

protocol UsageDataSource: Sendable {
    func status() async throws -> DaemonStatusPayload
    func accounts() async throws -> [Account]
    func usage() async throws -> [SnapshotPayload]
    func probe(accountId: String) async throws -> ProbePayload
}

extension DaemonClient: UsageDataSource {}

enum DataSourceSetupError: Error {
    case tokenNotConfigured
}

struct MockDataSource: UsageDataSource {
    private let snapshots: [SnapshotPayload]
    private let accountValues: [Account]

    init() {
        snapshots = (try? UllageFixtures.snapshots()) ?? []
        accountValues = sortedAccounts(snapshots.compactMap { snapshot in
            snapshot.usage.data.map { usage in
                Account(id: snapshot.accountId, provider: usage.provider, label: usage.accountLabel, enabled: true)
            }
        })
    }

    func status() async throws -> DaemonStatusPayload {
        let data = Data("{\"shutting_down\":false,\"accounts\":[],\"credential_backend\":\"macos_keychain\"}".utf8)
        return try UllageJSON.makeDecoder().decode(DaemonStatusPayload.self, from: data)
    }

    func accounts() async throws -> [Account] {
        accountValues
    }

    func usage() async throws -> [SnapshotPayload] {
        snapshots
    }

    func probe(accountId: String) async throws -> ProbePayload {
        try await Task.sleep(for: .seconds(1))
        guard let snapshot = snapshots.first(where: { $0.accountId == accountId }) else {
            throw DaemonError.notFound(kind: "account_not_found")
        }
        return ProbePayload(accountId: snapshot.accountId, usage: snapshot.usage)
    }
}
