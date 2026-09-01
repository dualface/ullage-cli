import AppKit
import Foundation
import Security
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
        XCTAssertEqual(image.accessibilityDescription, "Ullage")
    }

    @MainActor
    func testMenuBarLiquidPixelsIncreaseWithFillRatio() throws {
        let ratios = [0.0, 0.25, 0.5, 0.75, 1.0]
        let rendered = try ratios.map { try rasterizedMenuBarImage(fillRatio: $0) }
        let filledPixelCounts = rendered.map { representation in
            (0..<representation.pixelsHigh).reduce(into: 0) { count, y in
                for x in 0..<representation.pixelsWide {
                    if (representation.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.1 {
                        count += 1
                    }
                }
            }
        }
        XCTAssertEqual(filledPixelCounts, filledPixelCounts.sorted())
        XCTAssertEqual(Set(filledPixelCounts).count, ratios.count)
    }

    @MainActor
    func testMenuBarZeroFillLeavesTheVisibleCavityEmpty() throws {
        let representation = try rasterizedMenuBarImage(fillRatio: 0)
        let pixelSize = CGFloat(representation.pixelsWide)
        for y in [20.0, 40.0, 60.0, 80.0, 88.0] {
            let xPixel = Int((0.5 * pixelSize).rounded(.down))
            let yPixel = Int((y / 100 * pixelSize).rounded(.down))
            let alpha = try XCTUnwrap(representation.colorAt(x: xPixel, y: yPixel)).alphaComponent
            XCTAssertLessThanOrEqual(alpha, 0.05, "Unexpected liquid at y=\(y)")
        }
    }

    @MainActor
    func testMenuBarNoDataMarkDiffersFromEveryLiquidLevel() throws {
        let noData = try pngData(rasterizedMenuBarImage(fillRatio: nil))
        for ratio in [0.0, 0.25, 0.5, 0.75, 1.0] {
            XCTAssertNotEqual(noData, try pngData(rasterizedMenuBarImage(fillRatio: ratio)))
        }
        XCTAssertEqual(
            UllageMark.menuBarImage(fillRatio: 0.3).accessibilityDescription,
            "Ullage — 30% remaining"
        )
        XCTAssertEqual(
            UllageMark.menuBarImage(fillRatio: nil).accessibilityDescription,
            "Ullage — no data"
        )
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

    func testNormalizePairCodeInputStripsSeparatorsAndUppercases() {
        XCTAssertEqual(normalizePairCodeInput("abc-def"), "ABCDEF")
        XCTAssertEqual(normalizePairCodeInput(" ab c-de f "), "ABCDEF")
        XCTAssertEqual(normalizePairCodeInput("a1b2c3d4"), "A1B2C3")
        XCTAssertEqual(normalizePairCodeInput("***"), "")
        XCTAssertEqual(normalizePairCodeInput("ab!c@d#e$f%g"), "ABCDEF")
        XCTAssertEqual(formattedPairCode("ABCDEF"), "ABC-DEF")
        XCTAssertNil(formattedPairCode("ABCDE"))
        XCTAssertNil(formattedPairCode("ABCDEFG"))
    }

    @MainActor
    func testProgressColorsUseSemanticTierColors() {
        XCTAssertEqual(progressColor(for: .healthy), .systemGreen)
        XCTAssertEqual(progressColor(for: .caution), .systemYellow)
        XCTAssertEqual(progressColor(for: .low), .systemOrange)
        XCTAssertEqual(progressColor(for: .critical), .systemRed)
    }

    func testProgressSegmentsAllowPartialCellsFromTheRight() {
        XCTAssertEqual(progressSegmentFill(ratio: 0, index: 9), 0)
        XCTAssertEqual(progressSegmentFill(ratio: 0.05, index: 9), 0.5, accuracy: 1e-12)
        XCTAssertEqual(progressSegmentFill(ratio: 0.05, index: 8), 0)
        XCTAssertEqual(progressSegmentFill(ratio: 0.10, index: 9), 1)
        XCTAssertEqual(progressSegmentFill(ratio: 0.10, index: 8), 0)
        XCTAssertEqual(progressSegmentFill(ratio: 0.15, index: 9), 1)
        XCTAssertEqual(progressSegmentFill(ratio: 0.15, index: 8), 0.5, accuracy: 1e-12)
        XCTAssertEqual(progressSegmentFill(ratio: 1, index: 0), 1)
        XCTAssertEqual(progressSegmentFill(ratio: 1.4, index: 0), 1)
        XCTAssertEqual(progressSegmentFill(ratio: -.infinity, index: 9), 0)
        XCTAssertEqual(progressSegmentFill(ratio: .nan, index: 9), 0)
        XCTAssertEqual(progressSegmentFill(ratio: 0.5, index: -1), 0)
        XCTAssertEqual(progressSegmentFill(ratio: 0.5, index: 10), 0)
    }

    func testLoginItemRequiresApplicationBundle() {
        XCTAssertTrue(isApplicationBundleURL(URL(fileURLWithPath: "/Applications/Ullage.app")))
        XCTAssertTrue(isApplicationBundleURL(URL(fileURLWithPath: "/Applications/Ullage.APP")))
        XCTAssertFalse(isApplicationBundleURL(URL(fileURLWithPath: "/tmp/UllageMac")))
    }

    func testServerURLMatchesDaemonBindAddressClassification() {
        for host in [
            "127.0.0.1", "localhost", "[::1]", "100.64.0.1", "100.127.255.254",
            "[fd7a:115c:a1e0::1]", "10.0.0.5", "172.16.0.1", "192.168.50.10",
            "[fd00::1]",
        ] {
            XCTAssertNotNil(AppSettings.validatedServerURL("http://\(host):7878"), host)
        }
        for host in [
            "0.0.0.0", "[::]", "100.128.0.1", "172.32.0.1", "169.254.1.1",
            "[fe80::1]", "8.8.8.8", "example.com", "daemon.ts.net",
        ] {
            XCTAssertNil(AppSettings.validatedServerURL("http://\(host):7878"), host)
        }
        for value in [
            "https://127.0.0.1:7878",
            "http://user@localhost:7878",
            "http://user:secret@localhost:7878",
            "http://localhost:7878?token=secret",
            "http://localhost:7878#fragment",
            "http://127%2e0%2e0%2e1:7878",
            "http://local%00host:7878",
            "http://local\0host:7878",
        ] {
            XCTAssertNil(AppSettings.validatedServerURL(value), value)
        }
    }

    @MainActor
    func testPairingMetadataAndDeviceTokenAreStoredTogether() throws {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }
        var savedToken: String?
        let settings = AppSettings(defaults: defaults) { savedToken = $0 }
        let pairedAt = Date(timeIntervalSince1970: 1_777_777_777)
        XCTAssertNil(settings.pairedServerURL(matching: "http://192.168.50.10:7878"))

        try settings.completePairing(
            serverURL: "http://192.168.50.10:7878",
            credential: PairedDeviceCredential(
                deviceId: "ABCD2345EFGH",
                deviceName: "pro2026",
                deviceToken: "device-secret"
            ),
            pairedAt: pairedAt
        )

        XCTAssertEqual(savedToken, "device-secret")
        XCTAssertEqual(settings.serverURL.absoluteString, "http://192.168.50.10:7878")
        XCTAssertEqual(settings.pairedDeviceName, "pro2026")
        XCTAssertEqual(settings.pairedAt, pairedAt)
        XCTAssertNotNil(settings.pairedServerURL(matching: "http://192.168.50.10:7878"))
        XCTAssertNil(settings.pairedServerURL(matching: "http://10.0.0.5:7878"))
        let restored = AppSettings(defaults: defaults) { _ in }
        XCTAssertEqual(restored.pairedDeviceName, "pro2026")
        XCTAssertEqual(restored.pairedAt, pairedAt)
    }

    @MainActor
    func testPairingServerTargetKeepsRequestURLSnapshot() throws {
        var enteredURL = "http://192.168.50.10:7878"
        let target = try XCTUnwrap(PairingServerTarget(enteredURL))
        enteredURL = "http://10.0.0.5:7878"

        XCTAssertEqual(target.rawValue, "http://192.168.50.10:7878")
        XCTAssertEqual(target.url.absoluteString, "http://192.168.50.10:7878")
        XCTAssertNotEqual(target.rawValue, enteredURL)
    }

    @MainActor
    func testConnectionPairingGuardAppliesOnlyToDaemonMode() throws {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let settings = AppSettings(defaults: defaults) { _ in }
        let serverURL = "http://192.168.50.10:7878"

        XCTAssertTrue(connectionTestRequiresPairing(
            mode: .daemon,
            settings: settings,
            serverURL: serverURL
        ))
        XCTAssertFalse(connectionTestRequiresPairing(
            mode: .mock,
            settings: settings,
            serverURL: serverURL
        ))
    }

    func testKeychainRoundTripAndLegacyTokenCleanup() throws {
        let suffix = UUID().uuidString
        let deviceStore = KeychainStore(service: "dev.ullage.mac.tests.\(suffix)", account: "device")
        let legacyStore = KeychainStore(service: "dev.ullage.mac.tests.\(suffix)", account: "legacy")
        defer {
            try? deviceStore.delete()
            try? legacyStore.delete()
        }
        do {
            try legacyStore.save("legacy-secret")
            try Keychain.replaceDeviceToken(
                "device-secret",
                deviceStore: deviceStore,
                legacyStore: legacyStore
            )
            XCTAssertEqual(try deviceStore.load(), "device-secret")
            XCTAssertNil(try legacyStore.load())
            try deviceStore.save("replacement-secret")
            XCTAssertEqual(try deviceStore.load(), "replacement-secret")
            try deviceStore.delete()
            XCTAssertNil(try deviceStore.load())
        } catch KeychainError.status(let status) where status == errSecInteractionNotAllowed {
            throw XCTSkip("The login Keychain is unavailable outside the GUI session")
        }
    }

    func testPairingErrorsHaveDistinctUserMessages() {
        XCTAssertEqual(
            pairingMessage(for: .unauthorized(kind: "pair_code_invalid")),
            "pair code is invalid, expired, or already used"
        )
        XCTAssertEqual(
            pairingMessage(for: .rateLimited(retryAfter: 3, kind: "rate_limited")),
            "too many pair attempts; retry in 3s"
        )
        XCTAssertEqual(
            pairingMessage(for: .storage(kind: "storage")),
            "daemon could not store the device"
        )
        XCTAssertEqual(
            pairingMessage(for: .unexpectedStatus(400, kind: "bad_request")),
            "daemon rejected the pair request"
        )
        XCTAssertEqual(
            pairingMessage(for: .decoding(underlying: CocoaError(.fileReadCorruptFile))),
            "daemon returned an invalid pair response"
        )
    }

    @MainActor
    func testLivePairingStoresDeviceTokenAndReadsFourAccountsWhenConfigured() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let pairCodeFile = environment["ULLAGE_PAIR_CODE_FILE"],
              let serverURLFile = environment["ULLAGE_PAIR_SERVER_URL_FILE"] else {
            throw XCTSkip("Set ULLAGE_PAIR_CODE_FILE and ULLAGE_PAIR_SERVER_URL_FILE to run")
        }
        let pairCode = try String(contentsOfFile: pairCodeFile, encoding: .utf8)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let serverURLValue = try String(contentsOfFile: serverURLFile, encoding: .utf8)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let serverURL = try XCTUnwrap(AppSettings.validatedServerURL(serverURLValue))
        let credential = try await DaemonClient.pair(
            baseURL: serverURL,
            pairCode: pairCode,
            deviceName: localDeviceName()
        )
        let defaults = try XCTUnwrap(UserDefaults(suiteName: "com.ullage.mac"))
        let settings = AppSettings(defaults: defaults)
        try settings.completePairing(serverURL: serverURLValue, credential: credential)

        let client = DaemonClient(baseURL: serverURL, token: credential.deviceToken)
        async let accounts = client.accounts()
        async let usage = client.usage()
        let (accountValues, usageValues) = try await (accounts, usage)
        XCTAssertEqual(accountValues.count, 4)
        XCTAssertEqual(usageValues.count, 4)
        XCTAssertEqual(settings.pairedDeviceName, credential.deviceName)
        XCTAssertNotNil(settings.pairedAt)
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
        let sections = output.components(separatedBy: "\nACCOUNTS\n")
        XCTAssertEqual(sections.count, 2)
        XCTAssertTrue(sections[0].contains("weekly · Codex"))
        XCTAssertTrue(sections[0].contains("Rate limit reset credits"))
        XCTAssertFalse(sections[0].contains("5h | Codex"))
        XCTAssertTrue(sections[1].contains("weekly · Codex"))
        XCTAssertTrue(sections[0].contains("auto"))
        XCTAssertTrue(sections[0].contains("GrokBuild"))
        XCTAssertTrue(output.contains("badges=stale,partial,network"))
        XCTAssertTrue(output.contains("badges=rate_limited"))
        XCTAssertFalse(output.contains("private label"))
    }

    func testPopoverSizingDefaultsAndClampsHeight() {
        var sizing = PopoverSizing()
        XCTAssertEqual(sizing.contentSize, NSSize(width: 360, height: 260))
        sizing.update(preferredHeight: 480, maximumHeight: 600)
        XCTAssertEqual(sizing.contentSize, NSSize(width: 360, height: 480))
        sizing.update(preferredHeight: 700, maximumHeight: 600)
        XCTAssertEqual(sizing.contentSize, NSSize(width: 360, height: 600))
        sizing.update(preferredHeight: 120, maximumHeight: 600)
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
        XCTAssertEqual(store.connectionState, .protocolMismatch(client: "9", server: "10"))
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

@MainActor
private func rasterizedMenuBarImage(fillRatio: Double?) throws -> NSBitmapImageRep {
    let image = UllageMark.menuBarImage(fillRatio: fillRatio)
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
    image.draw(in: CGRect(origin: .zero, size: image.size), from: .zero, operation: .copy, fraction: 1)
    graphicsContext.flushGraphics()
    NSGraphicsContext.restoreGraphicsState()
    return representation
}

private func pngData(_ representation: NSBitmapImageRep) throws -> Data {
    try XCTUnwrap(representation.representation(using: .png, properties: [:]))
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
        let data = Data("{\"version\":9,\"shutting_down\":false,\"accounts\":[],\"credential_backend\":\"macos_keychain\"}".utf8)
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

    private var mismatch: DaemonError { .protocolMismatch(client: 9, server: 10) }
}
