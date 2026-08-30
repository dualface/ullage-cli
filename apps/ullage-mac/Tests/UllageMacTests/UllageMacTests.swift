import Foundation
import XCTest
@testable import UllageMac
import UllageKit

final class UllageMacTests: XCTestCase {
    func testServerURLAcceptsOnlyHTTPLoopbackHosts() {
        XCTAssertNotNil(AppSettings.validatedServerURL("http://127.0.0.1:7878"))
        XCTAssertNotNil(AppSettings.validatedServerURL("http://localhost:7878"))
        XCTAssertNil(AppSettings.validatedServerURL("https://127.0.0.1:7878"))
        XCTAssertNil(AppSettings.validatedServerURL("http://example.com:7878"))
        XCTAssertNil(AppSettings.validatedServerURL("http://user@localhost:7878"))
        XCTAssertNil(AppSettings.validatedServerURL("http://localhost:7878?token=secret"))
    }

    func testMockDataSourceLoadsFourProviderFixtures() async throws {
        let source = MockDataSource()
        let accounts = try await source.accounts()
        let snapshots = try await source.usage()
        XCTAssertEqual(accounts.map(\.provider), ["chatgpt", "claude", "cursor", "grok"])
        XCTAssertEqual(snapshots.count, 4)
        XCTAssertEqual(Set(accounts.map(\.id)), Set(snapshots.map(\.accountId)))
    }

    @MainActor
    func testStoppingStorePreventsFurtherRefreshes() async throws {
        let source = CountingDataSource()
        let store = UsageStore(dataSourceFactory: { source })
        let initial = await source.requestCount
        XCTAssertEqual(initial, 0)
        store.start()
        try await Task.sleep(for: .milliseconds(100))
        store.stop()
        let before = await source.requestCount
        store.refresh()
        try await Task.sleep(for: .milliseconds(100))
        let after = await source.requestCount
        XCTAssertEqual(after, before)
        XCTAssertFalse(store.isActive)
    }
}

private actor CountingDataSource: UsageDataSource {
    private(set) var requestCount = 0

    func status() async throws -> DaemonStatusPayload {
        requestCount += 1
        let data = Data("{\"shutting_down\":false,\"accounts\":[],\"credential_backend\":\"macos_keychain\"}".utf8)
        return try UllageJSON.makeDecoder().decode(DaemonStatusPayload.self, from: data)
    }

    func accounts() async throws -> [Account] {
        requestCount += 1
        return []
    }

    func usage() async throws -> [SnapshotPayload] {
        requestCount += 1
        return []
    }

    func probe(accountId: String) async throws -> ProbePayload {
        requestCount += 1
        throw CancellationError()
    }
}
