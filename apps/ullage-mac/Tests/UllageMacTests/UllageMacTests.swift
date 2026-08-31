import AppKit
import Foundation
import SwiftUI
import XCTest
@testable import UllageMac
import UllageKit

final class UllageMacTests: XCTestCase {
    @MainActor
    func testPalettesExposeTheCanonicalColors() {
        let expected: [(UllageMark.Palette, UInt32, UInt32, UInt32, UInt32, UInt32)] = [
            (.amber, 0x1B5044, 0x0B231E, 0xE9F2EC, 0xF2B34A, 0xC47F1F),
            (.oxblood, 0x4A1020, 0x24060F, 0xF3EBDD, 0xD9445F, 0x8C1A33),
            (.propellant, 0x2A2E35, 0x15171B, 0xDDE3E8, 0xFF6A2A, 0xD63A0A),
            (.copper, 0x123C40, 0x071E21, 0xEAF1EE, 0xD98A48, 0x8E4E1F),
            (.paper, 0xF4ECDC, 0xE7DCC4, 0x1E1B18, 0x2A3F5F, 0x14213A),
            (.plum, 0x3A1F4A, 0x1E0F2A, 0xF1E9F4, 0xF6B26B, 0xE3703F),
        ]

        XCTAssertEqual(UllageMark.Palette.allCases.count, 6)
        XCTAssertEqual(UllageMark.Palette.default, .oxblood)
        for (palette, groundInner, groundOuter, wall, liquidTop, liquidBottom) in expected {
            XCTAssertEqual(palette.groundInner, groundInner, palette.rawValue)
            XCTAssertEqual(palette.groundOuter, groundOuter, palette.rawValue)
            XCTAssertEqual(palette.wall, wall, palette.rawValue)
            XCTAssertEqual(palette.liquidTop, liquidTop, palette.rawValue)
            XCTAssertEqual(palette.liquidBottom, liquidBottom, palette.rawValue)
        }
        XCTAssertEqual(UllageMark.Palette.allCases.filter(\.isLightGround), [.paper])
    }

    @MainActor
    func testAllPaletteIconsRenderWithDifferentPixels() throws {
        let pngs = try UllageMark.Palette.allCases.map { palette in
            let icon = try UllageMark.applicationIcon(pixelSize: 64, palette: palette)
            return try XCTUnwrap(icon.representation(using: .png, properties: [:]))
        }
        XCTAssertEqual(Set(pngs).count, UllageMark.Palette.allCases.count)
    }

    @MainActor
    func testOxbloodIconContainsTheGlassLayers() throws {
        let icon = try UllageMark.applicationIcon(pixelSize: 256, palette: .oxblood)
        let glint = try iconColor(icon, markPoint: CGPoint(x: 42, y: 51.2))
        let liquid = try iconColor(icon, markPoint: CGPoint(x: 42, y: 60))
        XCTAssertGreaterThan(luminance(glint), luminance(liquid) + 0.08)

        let highlight = try iconColor(icon, markPoint: CGPoint(x: 23.2, y: 40))
        let wallCenter = try iconColor(icon, markPoint: CGPoint(x: 26, y: 40))
        XCTAssertGreaterThan(luminance(highlight), luminance(wallCenter) + 0.04)

        let innerShadow = try iconColor(icon, markPoint: CGPoint(x: 50, y: 20))
        let lowerInterior = try iconColor(icon, markPoint: CGPoint(x: 50, y: 35))
        XCTAssertGreaterThan(abs(luminance(innerShadow) - luminance(lowerInterior)), 0.02)
    }

    @MainActor
    func testMenuBarMarkIsAnEighteenPointTemplateImage() {
        let image = UllageMark.menuBarImage()
        XCTAssertEqual(image.size, NSSize(width: 18, height: 18))
        XCTAssertTrue(image.isTemplate)
    }

    @MainActor
    func testMenuBarMarkRasterizationPreservesWallLiquidAndVoidAlpha() throws {
        let image = UllageMark.menuBarImage()
        let scale = 2
        let pixelSize = Int(image.size.width) * scale
        let representation = try XCTUnwrap(NSBitmapImageRep(
            bitmapDataPlanes: nil,
            pixelsWide: pixelSize,
            pixelsHigh: pixelSize,
            bitsPerSample: 8,
            samplesPerPixel: 4,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: .deviceRGB,
            bytesPerRow: 0,
            bitsPerPixel: 0
        ))
        representation.size = image.size
        let graphicsContext = try XCTUnwrap(NSGraphicsContext(bitmapImageRep: representation))

        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = graphicsContext
        graphicsContext.cgContext.clear(CGRect(origin: .zero, size: image.size))
        image.draw(
            in: CGRect(origin: .zero, size: image.size),
            from: .zero,
            operation: .copy,
            fraction: 1
        )
        graphicsContext.flushGraphics()
        NSGraphicsContext.restoreGraphicsState()

        func alpha(atMarkPoint point: CGPoint) throws -> CGFloat {
            let x = Int((point.x / 100 * CGFloat(pixelSize)).rounded(.down))
            let y = Int((point.y / 100 * CGFloat(pixelSize)).rounded(.down))
            return try XCTUnwrap(representation.colorAt(x: x, y: y)).alphaComponent
        }

        let wallAlpha = try alpha(atMarkPoint: CGPoint(x: 12, y: 30))
        let liquidAlpha = try alpha(atMarkPoint: CGPoint(x: 50, y: 75))
        let voidAlpha = try alpha(atMarkPoint: CGPoint(x: 50, y: 30))
        XCTAssertGreaterThanOrEqual(wallAlpha, 0.95)
        XCTAssertTrue(0.35...0.55 ~= liquidAlpha)
        XCTAssertLessThanOrEqual(voidAlpha, 0.05)
    }

    @MainActor
    func testIconsetContainsTheStandardFilesAtTheirPixelSizes() throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
            .appendingPathComponent("AppIcon.iconset", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory.deletingLastPathComponent()) }

        try IconsetCommand.render(to: directory, palette: .plum)

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

    @MainActor
    func testIconPaletteDefaultsRejectsInvalidValuesAndRoundTrips() {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }

        XCTAssertEqual(AppSettings(defaults: defaults).iconPalette, .oxblood)
        defaults.set("bogus", forKey: "iconPalette")
        XCTAssertEqual(AppSettings(defaults: defaults).iconPalette, .oxblood)

        let settings = AppSettings(defaults: defaults)
        settings.iconPalette = .plum
        XCTAssertEqual(AppSettings(defaults: defaults).iconPalette, .plum)
    }

    @MainActor
    func testApplicationIconSetterReceivesPaletteChanges() throws {
        var images: [NSImage] = []
        let controller = ApplicationIconController { images.append($0) }
        try controller.apply(palette: .oxblood)
        try controller.apply(palette: .paper)

        XCTAssertEqual(images.count, 2)
        let pngs = try images.map { image -> Data in
            let representation = try XCTUnwrap(image.representations.first as? NSBitmapImageRep)
            return try XCTUnwrap(representation.representation(using: .png, properties: [:]))
        }
        XCTAssertNotEqual(pngs[0], pngs[1])
    }

    @MainActor
    func testSettingsPreviewChangesWithTheSelectedPalette() throws {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let settings = AppSettings(defaults: defaults)

        func snapshot(palette: UllageMark.Palette) throws -> Data {
            settings.iconPalette = palette
            let view = NSHostingView(rootView: SettingsView(
                settings: settings,
                mode: .mock,
                onSaved: {},
                onPaletteChanged: { _ in }
            ))
            view.frame = NSRect(x: 0, y: 0, width: 420, height: 340)
            view.layoutSubtreeIfNeeded()
            let representation = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: view.bounds))
            view.cacheDisplay(in: view.bounds, to: representation)
            return try XCTUnwrap(representation.representation(using: .png, properties: [:]))
        }

        XCTAssertNotEqual(try snapshot(palette: .oxblood), try snapshot(palette: .paper))
    }

    @MainActor
    func testProgressColorsUseOnlyTheHealthyPaletteColor() {
        XCTAssertEqual(rgbHex(progressColor(for: .healthy, palette: .plum)), 0xF6B26B)
        XCTAssertEqual(progressColor(for: .caution, palette: .plum), .systemYellow)
        XCTAssertEqual(progressColor(for: .low, palette: .plum), .systemOrange)
        XCTAssertEqual(progressColor(for: .critical, palette: .plum), .systemRed)
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

@MainActor
private func iconColor(
    _ icon: NSBitmapImageRep,
    markPoint: CGPoint,
    file: StaticString = #filePath,
    line: UInt = #line
) throws -> NSColor {
    let size = CGFloat(icon.pixelsWide)
    let tileOrigin = size * 100 / 1024
    let tileSide = size * 824 / 1024
    let x = Int((tileOrigin + markPoint.x / 100 * tileSide).rounded(.down))
    let y = Int((tileOrigin + markPoint.y / 100 * tileSide).rounded(.down))
    return try XCTUnwrap(icon.colorAt(x: x, y: y), file: file, line: line)
}

private func luminance(_ color: NSColor) -> CGFloat {
    let converted = color.usingColorSpace(.sRGB)!
    return 0.2126 * converted.redComponent
        + 0.7152 * converted.greenComponent
        + 0.0722 * converted.blueComponent
}

private func rgbHex(_ color: NSColor) -> UInt32 {
    let converted = color.usingColorSpace(.sRGB)!
    let red = UInt32((converted.redComponent * 255).rounded())
    let green = UInt32((converted.greenComponent * 255).rounded())
    let blue = UInt32((converted.blueComponent * 255).rounded())
    return red << 16 | green << 8 | blue
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
