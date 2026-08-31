import AppKit
import Foundation
import XCTest
@testable import UllageMac
import UllageKit

final class UllageMacTests: XCTestCase {
    @MainActor
    func testMenuBarMarkIsAnEighteenPointTemplateImage() {
        let image = UllageMark.menuBarImage()
        XCTAssertEqual(image.size, NSSize(width: 18, height: 18))
        XCTAssertTrue(image.isTemplate)
    }

    @MainActor
    func testIconsetContainsTheStandardFilesAtTheirPixelSizes() throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
            .appendingPathComponent("AppIcon.iconset", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory.deletingLastPathComponent()) }

        try IconsetCommand.render(to: directory)

        let filenames = try Set(FileManager.default.contentsOfDirectory(atPath: directory.path))
        XCTAssertEqual(filenames, Set(IconsetCommand.entries.map(\.filename)))
        for entry in IconsetCommand.entries {
            let data = try Data(contentsOf: directory.appendingPathComponent(entry.filename))
            let representation = try XCTUnwrap(
                NSBitmapImageRep(data: data)
            )
            XCTAssertEqual(representation.pixelsWide, entry.pixelSize, entry.filename)
            XCTAssertEqual(representation.pixelsHigh, entry.pixelSize, entry.filename)
        }
    }

    func testLoginItemRequiresApplicationBundle() {
        XCTAssertTrue(isApplicationBundleURL(URL(fileURLWithPath: "/Applications/Ullage.app")))
        XCTAssertTrue(isApplicationBundleURL(URL(fileURLWithPath: "/Applications/Ullage.APP")))
        XCTAssertFalse(isApplicationBundleURL(URL(fileURLWithPath: "/tmp/UllageMac")))
    }

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

    func testDumpContainsBothProjectionsWithoutLabels() throws {
        let snapshots = try UllageFixtures.snapshots()
        let accounts = snapshots.compactMap { snapshot in
            snapshot.usage.data.map {
                Account(id: snapshot.accountId, provider: $0.provider, label: "private label", enabled: true)
            }
        }
        let output = dumpOutput(accounts: accounts, snapshots: snapshots)
        XCTAssertTrue(output.hasPrefix("OVERVIEW\n"))
        XCTAssertTrue(output.contains("\nACCOUNTS\n"))
        XCTAssertTrue(output.contains("weekly · Codex"))
        XCTAssertTrue(output.contains("badges=stale,partial,network"))
        XCTAssertTrue(output.contains("badges=rate_limited"))
        XCTAssertFalse(output.contains("private label"))
    }

    func testPopoverSizingDefaultsAndClampsHeight() {
        var sizing = PopoverSizing()
        XCTAssertEqual(sizing.contentSize, NSSize(width: 360, height: 260))
        sizing.update(preferredHeight: 700)
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
        let data = Data("{\"version\":8,\"shutting_down\":false,\"accounts\":[],\"credential_backend\":\"macos_keychain\"}".utf8)
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

    func probe(accountId: String, wait: Bool) async throws -> ProbeResult {
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
    func probe(accountId: String, wait: Bool) async throws -> ProbeResult { throw mismatch }

    private var mismatch: DaemonError { .protocolMismatch(client: 8, server: 9) }
}
