import AppKit
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

    func testSelectionFallsBackWhenTheSelectedAccountDisappears() {
        XCTAssertEqual(normalizedSelection(.account("a"), accountIDs: ["b"]), .overview)
        XCTAssertEqual(normalizedSelection(.account("a"), accountIDs: ["a", "b"]), .account("a"))
        XCTAssertEqual(normalizedSelection(.overview, accountIDs: []), .overview)
    }

    func testSummaryFormattingMatchesCLIConventions() {
        XCTAssertEqual(numberText(12), "12")
        XCTAssertEqual(numberText(12.34), "12.34")
        XCTAssertEqual(numberText(.nan), "-")
        XCTAssertEqual(percentageText(74.6), "75%")
        XCTAssertEqual(moneyText(12.5, "USD"), "$12.50")
        XCTAssertEqual(moneyText(12.5, "EUR"), "€12.50")
        XCTAssertEqual(moneyText(12.5, "GBP"), "£12.50")
        XCTAssertEqual(moneyText(12.5, "SEK"), "SEK 12.50")
    }

    func testPopoverSizingClampsAndRetainsMeasuredHeight() {
        var sizing = PopoverSizing()
        XCTAssertEqual(sizing.contentSize, NSSize(width: 360, height: 260))
        sizing.update(preferredHeight: 700)
        XCTAssertEqual(sizing.contentSize, NSSize(width: 360, height: 520))
        XCTAssertEqual(sizing.contentSize, NSSize(width: 360, height: 520))
        sizing.update(preferredHeight: 120)
        XCTAssertEqual(sizing.contentSize, NSSize(width: 360, height: 180))
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

    @MainActor
    func testRateLimitCountdownRestartsWhenPopoverReopens() async throws {
        let source = CountingDataSource(probeRetryAfter: 0.4)
        let store = UsageStore(dataSourceFactory: { source })
        store.start()
        store.probe(accountID: "fixture")
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertTrue(store.isRateLimited("fixture"))
        store.stop()
        store.start()
        XCTAssertTrue(store.isRateLimited("fixture"))
        try await Task.sleep(for: .milliseconds(1_100))
        XCTAssertFalse(store.isRateLimited("fixture"))
        store.stop()
    }

    @MainActor
    func testProtocolMismatchCarriesBothVersionsToConnectionState() async throws {
        let source = ProtocolMismatchDataSource()
        let store = UsageStore(dataSourceFactory: { source })
        store.start()
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(store.connectionState, .protocolMismatch(client: "8", server: "9"))
        store.stop()
    }
}

private actor CountingDataSource: UsageDataSource {
    private(set) var requestCount = 0
    private let probeRetryAfter: TimeInterval?

    init(probeRetryAfter: TimeInterval? = nil) {
        self.probeRetryAfter = probeRetryAfter
    }

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
        if let probeRetryAfter {
            throw DaemonError.rateLimited(retryAfter: probeRetryAfter, kind: "rate_limited")
        }
        throw CancellationError()
    }
}

private struct ProtocolMismatchDataSource: UsageDataSource {
    func status() async throws -> DaemonStatusPayload { throw mismatch }
    func accounts() async throws -> [Account] { throw mismatch }
    func usage() async throws -> [SnapshotPayload] { throw mismatch }
    func probe(accountId: String) async throws -> ProbePayload { throw mismatch }

    private var mismatch: DaemonError { .protocolMismatch(client: 8, server: 9) }
}
