import AppKit
import CryptoKit
import Foundation
import Security
import SwiftUI
import XCTest
@testable import UllageMac
import UllageKit

final class UllageMacTests: XCTestCase {
    @MainActor
    func testPalettesExposeTheCanonicalColors() {
        let expected: [(AppPalette, UInt32, UInt32, UInt32, UInt32, UInt32)] = [
            (.amber, 0x1B5044, 0x0B231E, 0xE9F2EC, 0xF2B34A, 0xC47F1F),
            (.oxblood, 0x4A1020, 0x24060F, 0xF3EBDD, 0xD9445F, 0x8C1A33),
            (.propellant, 0x2A2E35, 0x15171B, 0xDDE3E8, 0xFF6A2A, 0xD63A0A),
            (.copper, 0x123C40, 0x071E21, 0xEAF1EE, 0xD98A48, 0x8E4E1F),
            (.paper, 0xF4ECDC, 0xE7DCC4, 0x1E1B18, 0x2A3F5F, 0x14213A),
            (.plum, 0x3A1F4A, 0x1E0F2A, 0xF1E9F4, 0xF6B26B, 0xE3703F),
        ]

        XCTAssertEqual(AppPalette.allCases.count, 6)
        XCTAssertEqual(AppPalette.default, .oxblood)
        for (palette, groundInner, groundOuter, wall, liquidTop, liquidBottom) in expected {
            XCTAssertEqual(palette.colors.groundInner, groundInner, palette.rawValue)
            XCTAssertEqual(palette.colors.groundOuter, groundOuter, palette.rawValue)
            XCTAssertEqual(palette.colors.wall, wall, palette.rawValue)
            XCTAssertEqual(palette.colors.liquidTop, liquidTop, palette.rawValue)
            XCTAssertEqual(palette.colors.liquidBottom, liquidBottom, palette.rawValue)
        }
        XCTAssertEqual(AppPalette.allCases.filter(\.isLightGround), [.paper])
    }

    @MainActor
    func testAllPaletteIconsRenderWithDifferentPixels() throws {
        let pngs = try AppPalette.allCases.map { palette in
            let icon = try UllageMark.applicationIcon(pixelSize: 64, palette: palette)
            return try XCTUnwrap(icon.representation(using: .png, properties: [:]))
        }
        XCTAssertEqual(Set(pngs).count, AppPalette.allCases.count)
    }

    @MainActor
    func testApplicationIconPixelsStayStableAcrossPaletteExtraction() throws {
        let expected = [
            AppPalette.amber: "19f6b5e1f7daa746801cc7334b9375711d8ad34aba5e4e8d4fc286cd1d703d44",
            .oxblood: "739b0f1ccc52e04902ffcddd5ce3db542bdb285b5f82f20fbf597a0a7e699f68",
            .propellant: "0eb65f96dfe9ece23d1d8d8c4774ba368f5c995b8eedec8fb2dc8e19067e671d",
            .copper: "c2f1536b43510b8705ad8f14f78a210430369bb91f88b1f83915e6eeca347023",
            .paper: "95e8d56ca9961b0e126ec09249d8ff8e58963e4da228a682d7ebf444970a055f",
            .plum: "44a04ca85b85093eb8c41c36e0cb3e2485175e365c540cd8ecbb9395ca1d70a1",
        ]
        for palette in AppPalette.allCases {
            let icon = try UllageMark.applicationIcon(pixelSize: 64, palette: palette)
            let data = Data(
                bytes: try XCTUnwrap(icon.bitmapData),
                count: icon.bytesPerRow * icon.pixelsHigh
            )
            let digest = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
            XCTAssertEqual(digest, expected[palette], palette.rawValue)
        }
    }

    @MainActor
    func testPalettesProduceDistinctFlatSurfacesInBothAppearances() {
        for isDark in [false, true] {
            let surfaces = AppPalette.allCases.map { $0.flatSurface(isDark: isDark) }
            XCTAssertEqual(Set(surfaces).count, AppPalette.allCases.count)
        }
        for palette in AppPalette.allCases {
            let lightBase = palette.flatSurface(isDark: false).base
            let darkBase = palette.flatSurface(isDark: true).base
            XCTAssertGreaterThanOrEqual(minimumChannel(lightBase), 0xD9, palette.rawValue)
            XCTAssertLessThanOrEqual(maximumChannel(darkBase), 0x33, palette.rawValue)
        }
        XCTAssertEqual(
            AppPalette.default.flatSurface(isDark: false),
            AppPalette.oxblood.flatSurface(isDark: false)
        )
        XCTAssertEqual(
            AppPalette.default.flatSurface(isDark: true),
            AppPalette.oxblood.flatSurface(isDark: true)
        )
    }

    private func minimumChannel(_ color: UInt32) -> UInt32 {
        min(min((color >> 16) & 0xFF, (color >> 8) & 0xFF), color & 0xFF)
    }

    private func maximumChannel(_ color: UInt32) -> UInt32 {
        max(max((color >> 16) & 0xFF, (color >> 8) & 0xFF), color & 0xFF)
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
    func testMenuBarFillLevelsProduceDistinctImages() throws {
        let ratios = [0.0, 0.25, 0.5, 0.75, 1.0]
        let pngs = try ratios.map { try pngData(rasterizedMenuBarImage(fillRatio: $0)) }
        XCTAssertEqual(Set(pngs).count, ratios.count)
    }

    @MainActor
    func testMenuBarLiquidIsSolidBelowTheSurfaceAndClearAbove() throws {
        let representation = try rasterizedMenuBarImage(fillRatio: 0.8)
        let pixelSize = CGFloat(representation.pixelsWide)
        func alpha(atMarkY y: Double) throws -> CGFloat {
            let xPixel = Int((0.5 * pixelSize).rounded(.down))
            let yPixel = Int((y / 100 * pixelSize).rounded(.down))
            return try XCTUnwrap(representation.colorAt(x: xPixel, y: yPixel)).alphaComponent
        }
        // Surface sits at y ≈ 26 for 80%; the cavity between the brim bar
        // (y ≤ 16.5) and the surface stays clear.
        XCTAssertLessThanOrEqual(try alpha(atMarkY: 20), 0.05)
        for y in [40.0, 60.0, 80.0] {
            XCTAssertGreaterThanOrEqual(try alpha(atMarkY: y), 0.3, "Expected liquid fill at y=\(y)")
        }
    }

    @MainActor
    func testMenuBarFlatSurfaceIsLevelWhereTheWaveIsNot() throws {
        let flat = try pngData(rasterizedMenuBarImage(fillRatio: 0.5, wavePhase: nil))
        let wave = try pngData(rasterizedMenuBarImage(fillRatio: 0.5, wavePhase: 0))
        XCTAssertNotEqual(flat, wave)

        // At 50% the surface sits at y = 50.75; sample a row 1.15 units above
        // it at a fine scale. A flat line leaves the row clear everywhere, while
        // the wave (amplitude 1.8, positive sine downward) crests through it
        // at x = 75.
        let flatRep = try rasterizedMenuBarImage(fillRatio: 0.5, wavePhase: nil, scale: 10)
        let waveRep = try rasterizedMenuBarImage(fillRatio: 0.5, wavePhase: 0, scale: 10)
        func alpha(_ rep: NSBitmapImageRep, x: Double, y: Double) throws -> CGFloat {
            let size = CGFloat(rep.pixelsWide)
            let xPixel = Int((x / 100 * size).rounded(.down))
            let yPixel = Int((y / 100 * size).rounded(.down))
            return try XCTUnwrap(rep.colorAt(x: xPixel, y: yPixel)).alphaComponent
        }
        for x in stride(from: 25.0, through: 75.0, by: 5) {
            XCTAssertLessThanOrEqual(try alpha(flatRep, x: x, y: 49.6), 0.05, "Flat surface at x=\(x)")
        }
        XCTAssertGreaterThanOrEqual(try alpha(waveRep, x: 75, y: 49.6), 0.3)
        XCTAssertGreaterThanOrEqual(try alpha(flatRep, x: 50, y: 52), 0.3)
    }

    @MainActor
    func testMenuBarBrimLineShowsInLiquidStatesOnly() throws {
        func alpha(_ rep: NSBitmapImageRep, y: Double) throws -> CGFloat {
            let size = CGFloat(rep.pixelsWide)
            let xPixel = Int((0.5 * size).rounded(.down))
            let yPixel = Int((y / 100 * size).rounded(.down))
            return try XCTUnwrap(rep.colorAt(x: xPixel, y: yPixel)).alphaComponent
        }
        // The brim bar spans y 12...16.5 at 0.35 alpha; sample inside it.
        let empty = try rasterizedMenuBarImage(fillRatio: 0, scale: 10)
        XCTAssertGreaterThanOrEqual(try alpha(empty, y: 14), 0.2)
        XCTAssertLessThanOrEqual(try alpha(empty, y: 14), 0.6)
        let half = try rasterizedMenuBarImage(fillRatio: 0.5, scale: 10)
        XCTAssertGreaterThanOrEqual(try alpha(half, y: 14), 0.2)
        XCTAssertLessThanOrEqual(try alpha(half, y: 14), 0.6)
        let noData = try rasterizedMenuBarImage(fillRatio: nil, scale: 10)
        XCTAssertLessThanOrEqual(try alpha(noData, y: 14), 0.05)
    }

    @MainActor
    func testMenuBarZeroFillLeavesTheVisibleCavityEmpty() throws {
        let representation = try rasterizedMenuBarImage(fillRatio: 0)
        let pixelSize = CGFloat(representation.pixelsWide)
        for y in [20.0, 40.0, 60.0, 80.0] {
            let xPixel = Int((0.5 * pixelSize).rounded(.down))
            let yPixel = Int((y / 100 * pixelSize).rounded(.down))
            let alpha = try XCTUnwrap(representation.colorAt(x: xPixel, y: yPixel)).alphaComponent
            XCTAssertLessThanOrEqual(alpha, 0.05, "Unexpected liquid fill at y=\(y)")
        }
        let phaseA = try pngData(rasterizedMenuBarImage(fillRatio: 0, wavePhase: 0))
        let phaseB = try pngData(rasterizedMenuBarImage(fillRatio: 0, wavePhase: .pi / 2))
        XCTAssertEqual(phaseA, phaseB, "Zero fill must not draw a wobbling surface")
    }

    @MainActor
    func testMenuBarAccessibilityUsesTargetRatioDuringTransitions() {
        let image = UllageMark.menuBarImage(
            fillRatio: 0.4,
            accountLabel: "Claude",
            accessibilityRatio: 0.8
        )
        XCTAssertEqual(image.accessibilityDescription, "Ullage — 80% remaining · Claude")
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
            UllageMark.menuBarImage(fillRatio: 0.3, accountLabel: "Claude").accessibilityDescription,
            "Ullage — 30% remaining · Claude"
        )
        XCTAssertEqual(
            UllageMark.menuBarImage(fillRatio: nil).accessibilityDescription,
            "Ullage — no data"
        )
    }

    @MainActor
    func testMenuBarWavePhaseChangesPixelsWithoutChangingFill() throws {
        let still = try pngData(rasterizedMenuBarImage(fillRatio: 0.5, wavePhase: 0))
        let wobbling = try pngData(rasterizedMenuBarImage(fillRatio: 0.5, wavePhase: .pi / 2))
        XCTAssertNotEqual(still, wobbling)
    }

    @MainActor
    func testMenuBarMarkRasterizationKeepsWallAndHollowCavity() throws {
        let image = UllageMark.menuBarImage(fillRatio: 0.5)
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
        let voidAlpha = try alpha(atMarkPoint: CGPoint(x: 50, y: 30))
        XCTAssertGreaterThanOrEqual(wallAlpha, 0.95)
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

    func testPairingEditingStateLocksOnlyWhenPairedAndNotUnlocked() {
        XCTAssertEqual(pairingEditingState(isPaired: false, isUnlocked: false), .unpaired)
        XCTAssertEqual(pairingEditingState(isPaired: false, isUnlocked: true), .unpaired)
        XCTAssertEqual(pairingEditingState(isPaired: true, isUnlocked: false), .locked)
        XCTAssertEqual(pairingEditingState(isPaired: true, isUnlocked: true), .unlockedForRepair)
    }

    func testFlowRowsWrapWhenTheNextItemWouldOverflow() {
        // Three 100 wide items with spacing 4 need 308 for one row.
        XCTAssertEqual(flowRows(widths: [100, 100, 100], maxWidth: 308, spacing: 4), [[0, 1, 2]])
        XCTAssertEqual(flowRows(widths: [100, 100, 100], maxWidth: 307, spacing: 4), [[0, 1], [2]])
        XCTAssertEqual(flowRows(widths: [100, 100, 100], maxWidth: 100, spacing: 4), [[0], [1], [2]])
        // An item wider than the row still gets placed rather than dropped.
        XCTAssertEqual(flowRows(widths: [400, 50], maxWidth: 100, spacing: 4), [[0], [1]])
        XCTAssertEqual(flowRows(widths: [], maxWidth: 100, spacing: 4), [])
    }

    func testWrapFriendlyTitleKeepsPunctuationOffItsOwnLine() {
        let joiner = "\u{2060}"
        let nbsp = "\u{00A0}"

        // The separator binds to the word before it, so it cannot start a line.
        XCTAssertEqual(wrapFriendlyTitle("Claude · 5h"), "Claude\(nbsp)· 5h")
        // Brackets bind inward, so neither can be stranded alone.
        XCTAssertEqual(
            wrapFriendlyTitle("Fable (weekly_scoped)"),
            "Fable (\(joiner)weekly_scoped\(joiner))"
        )
        XCTAssertEqual(wrapFriendlyTitle("ChatGPT"), "ChatGPT")
        XCTAssertEqual(wrapFriendlyTitle(""), "")
        // Ordinary spaces stay breakable so the title can still wrap.
        XCTAssertTrue(wrapFriendlyTitle("Cursor · monthly auto").hasSuffix("monthly auto"))
    }

    func testAccountLabelOnlyShowsWhenAProviderHasSeveralAccounts() {
        let solo = Account(id: "a", provider: "claude", label: "Work", enabled: true)
        let sibling = Account(id: "b", provider: "claude", label: " Personal ", enabled: true)
        let other = Account(id: "c", provider: "grok", label: "Team", enabled: true)

        XCTAssertNil(disambiguatingLabel(for: solo, in: [solo, other]))
        XCTAssertEqual(disambiguatingLabel(for: solo, in: [solo, sibling]), "Work")
        XCTAssertEqual(disambiguatingLabel(for: sibling, in: [solo, sibling]), "Personal")

        let blank = Account(id: "d", provider: "claude", label: "  ", enabled: true)
        XCTAssertNil(disambiguatingLabel(for: blank, in: [blank, solo]))
        let unlabelled = Account(id: "e", provider: "claude", label: nil, enabled: true)
        XCTAssertNil(disambiguatingLabel(for: unlabelled, in: [unlabelled, solo]))
    }

    @MainActor
    func testHeroModelTracksTheSameLevelAsTheMenuBarLiquid() throws {
        let snapshots = try UllageFixtures.snapshots()
        let accounts = snapshots.map {
            Account(id: $0.accountId, provider: $0.usage.data?.provider ?? "unknown", label: nil, enabled: true)
        }
        let levels = menuBarLiquidLevels(accounts: accounts, snapshots: snapshots, pinnedMetricID: nil)
        let floor = try XCTUnwrap(MenuBarLiquidAnimation.floorLevel(in: levels))

        let hero = try XCTUnwrap(heroModel(
            accounts: accounts,
            snapshots: snapshots,
            pinnedMetricID: nil,
            hiddenOverviewItemIDs: [],
            shownOverviewItemIDs: []
        ))
        XCTAssertEqual(hero.ratio, floor.remainingRatio)
        XCTAssertEqual(hero.title, floor.displayName)
        XCTAssertEqual(hero.caption, "Lowest remaining")
        // More than one account, so the runner-up is named and ranks no lower.
        let detail = try XCTUnwrap(hero.detail)
        XCTAssertTrue(detail.contains("next "), detail)

        let options = menuBarMetricOptions(accounts: accounts, snapshots: snapshots)
        let pinned = try XCTUnwrap(options.max { $0.remainingRatio < $1.remainingRatio })
        let pinnedHero = try XCTUnwrap(heroModel(
            accounts: accounts,
            snapshots: snapshots,
            pinnedMetricID: pinned.id,
            hiddenOverviewItemIDs: [],
            shownOverviewItemIDs: []
        ))
        XCTAssertEqual(pinnedHero.ratio, pinned.remainingRatio)
        XCTAssertEqual(pinnedHero.title, pinned.title)
        XCTAssertEqual(pinnedHero.caption, "Tracking")
        XCTAssertFalse(pinnedHero.detail?.contains("next ") ?? false)

        XCTAssertNil(heroModel(
            accounts: [],
            snapshots: [],
            pinnedMetricID: nil,
            hiddenOverviewItemIDs: [],
            shownOverviewItemIDs: []
        ))
    }

    @MainActor
    func testHeroTracksAPinnedRowThatOnlyOptedIntoOverview() throws {
        let snapshot = try UllageFixtures.snapshot(named: "claude")
        let usage = try XCTUnwrap(snapshot.usage.data)
        let account = Account(id: snapshot.accountId, provider: "claude", label: nil, enabled: true)
        let catalog = catalogProgressIDs(for: usage, accountID: account.id)
        // A progress row outside the catalog: visible only once opted in.
        let extra = try XCTUnwrap(identifiedSummaryRows(for: usage).first {
            $0.row.remainingRatio != nil && !catalog.contains($0.persistenceID(accountID: account.id))
        })
        let pinID = extra.persistenceID(accountID: account.id)
        let shown: Set<String> = [pinID]

        let hero = try XCTUnwrap(heroModel(
            accounts: [account],
            snapshots: [snapshot],
            pinnedMetricID: pinID,
            hiddenOverviewItemIDs: [],
            shownOverviewItemIDs: shown
        ))
        XCTAssertEqual(hero.caption, "Tracking")
        XCTAssertEqual(hero.ratio, extra.row.remainingRatio)

        // The header must agree with the level the menu bar liquid uses.
        let levels = menuBarLiquidLevels(
            accounts: [account],
            snapshots: [snapshot],
            pinnedMetricID: pinID,
            hiddenOverviewItemIDs: [],
            shownOverviewItemIDs: shown
        )
        XCTAssertEqual(levels.count, 1)
        XCTAssertEqual(hero.ratio, levels.first?.remainingRatio)
        XCTAssertEqual(hero.title, levels.first?.displayName)

        // Regression: dropping the opt-in made the header lose the pin and
        // silently fall back while Settings still showed it selected.
        let withoutOptIn = try XCTUnwrap(heroModel(
            accounts: [account],
            snapshots: [snapshot],
            pinnedMetricID: pinID,
            hiddenOverviewItemIDs: [],
            shownOverviewItemIDs: []
        ))
        XCTAssertEqual(withoutOptIn.caption, "Lowest remaining")
        XCTAssertNotEqual(withoutOptIn.title, hero.title)
    }

    @MainActor
    func testHeroReportsThePinnedRowsOwnReset() throws {
        let snapshots = try UllageFixtures.snapshots()
        let accounts = snapshots.map {
            Account(id: $0.accountId, provider: $0.usage.data?.provider ?? "unknown", label: nil, enabled: true)
        }
        let now = Date(timeIntervalSince1970: 0)
        let options = menuBarMetricOptions(accounts: accounts, snapshots: snapshots)
        let dated = try XCTUnwrap(options.first { ($0.resetsAt ?? now) > now })
        let hero = try XCTUnwrap(heroModel(
            accounts: accounts,
            snapshots: snapshots,
            pinnedMetricID: dated.id,
            hiddenOverviewItemIDs: [],
            shownOverviewItemIDs: [],
            now: now
        ))
        let reset = try XCTUnwrap(dated.resetsAt)
        let detail = try XCTUnwrap(hero.detail)
        XCTAssertEqual(detail, "resets \(resetRelativeText(reset, now: now))")
    }

    @MainActor
    func testMenuBarMetricPinDefaultsToNilAndRoundTrips() {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }

        XCTAssertNil(AppSettings(defaults: defaults).menuBarMetricID)
        let settings = AppSettings(defaults: defaults)
        settings.menuBarMetricID = "acct|weekly|usage|0"
        XCTAssertEqual(AppSettings(defaults: defaults).menuBarMetricID, "acct|weekly|usage|0")
        settings.menuBarMetricID = nil
        XCTAssertNil(AppSettings(defaults: defaults).menuBarMetricID)
        XCTAssertNil(defaults.object(forKey: "menuBarMetricID"))
    }

    @MainActor
    func testHiddenOverviewItemIDsDefaultEmptyAndRoundTrip() {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }

        XCTAssertEqual(AppSettings(defaults: defaults).hiddenOverviewItemIDs, [])
        let settings = AppSettings(defaults: defaults)
        settings.setOverviewItemVisible("acct|weekly|usage|0", visible: false)
        XCTAssertEqual(
            AppSettings(defaults: defaults).hiddenOverviewItemIDs,
            ["acct|weekly|usage|0"]
        )
        settings.setOverviewItemVisible("acct|weekly|usage|0", visible: true)
        XCTAssertEqual(AppSettings(defaults: defaults).hiddenOverviewItemIDs, [])
        XCTAssertEqual(AppSettings(defaults: defaults).shownOverviewItemIDs, [])

        settings.setOverviewItemVisible("acct|5h|requests|0", visible: true, catalogDefault: false)
        XCTAssertEqual(AppSettings(defaults: defaults).shownOverviewItemIDs, ["acct|5h|requests|0"])
        settings.setOverviewItemVisible("acct|5h|requests|0", visible: false, catalogDefault: false)
        XCTAssertEqual(AppSettings(defaults: defaults).shownOverviewItemIDs, [])
        XCTAssertEqual(AppSettings(defaults: defaults).hiddenOverviewItemIDs, [])
    }

    @MainActor
    func testMockDumpIgnoresHiddenOverviewPreferences() {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let settings = AppSettings(defaults: defaults)
        settings.setOverviewItemVisible("acct|weekly|usage|0", visible: false)
        settings.setOverviewItemVisible("acct|5h|requests|0", visible: true, catalogDefault: false)

        XCTAssertEqual(dumpOverviewVisibility(mode: .mock, settings: settings), DumpOverviewVisibility(hidden: [], shown: []))
        XCTAssertEqual(
            dumpOverviewVisibility(mode: .daemon, settings: settings),
            DumpOverviewVisibility(hidden: ["acct|weekly|usage|0"], shown: ["acct|5h|requests|0"])
        )
    }

    @MainActor
    func testAnimateLiquidSettingDefaultsOnAndRoundTrips() {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }

        XCTAssertTrue(AppSettings(defaults: defaults).animatesMenuBarLiquid)
        let settings = AppSettings(defaults: defaults)
        settings.animatesMenuBarLiquid = false
        XCTAssertFalse(AppSettings(defaults: defaults).animatesMenuBarLiquid)
        settings.animatesMenuBarLiquid = true
        XCTAssertTrue(AppSettings(defaults: defaults).animatesMenuBarLiquid)
    }

    func testBackdropBreathStaysBoundedAndCloses() {
        // The loop closes: the last frame lands where the first one starts.
        for turns in [1.0, 2.0, 3.0] {
            XCTAssertEqual(
                backdropBreath(progress: 0, turns: turns, seed: 0.4),
                backdropBreath(progress: 1, turns: turns, seed: 0.4),
                accuracy: 1e-9
            )
        }

        // Nothing leaves the range the view scales and dims against.
        for step in 0...200 {
            let breath = backdropBreath(progress: Double(step) / 200, turns: 2, seed: 1.7)
            XCTAssertLessThanOrEqual(abs(breath), 1 + 1e-9)
        }

        // Seeds put the shapes out of step rather than pulsing them as one.
        let first = backdropBreath(progress: 0.25, turns: 1, seed: 0)
        let second = backdropBreath(progress: 0.25, turns: 1, seed: 3.4)
        XCTAssertGreaterThan(abs(first - second), 0.5)

        // The clock folds into the loop, and folds the same way either side of
        // the reference date.
        let reference = Date(timeIntervalSinceReferenceDate: 0)
        XCTAssertEqual(backdropProgress(at: reference, loop: 60), 0, accuracy: 1e-9)
        XCTAssertEqual(
            backdropProgress(at: reference.addingTimeInterval(75), loop: 60),
            0.25,
            accuracy: 1e-9
        )
        XCTAssertEqual(
            backdropProgress(at: reference.addingTimeInterval(-15), loop: 60),
            0.75,
            accuracy: 1e-9
        )
        for offset in stride(from: -300.0, through: 300.0, by: 7.5) {
            let progress = backdropProgress(at: reference.addingTimeInterval(offset), loop: 60)
            XCTAssertGreaterThanOrEqual(progress, 0)
            XCTAssertLessThan(progress, 1)
        }
    }

    @MainActor
    func testLiquidGlassSettingDefaultsOnRoundTripsAndGatesTheGlass() {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }

        XCTAssertTrue(AppSettings(defaults: defaults).usesLiquidGlass)
        let settings = AppSettings(defaults: defaults)
        settings.usesLiquidGlass = false
        XCTAssertFalse(AppSettings(defaults: defaults).usesLiquidGlass)
        // Off is the flat presentation on every version.
        XCTAssertFalse(liquidGlassIsEnabled(settings: settings))

        settings.usesLiquidGlass = true
        XCTAssertTrue(AppSettings(defaults: defaults).usesLiquidGlass)
        if #available(macOS 26.0, *) {
            XCTAssertTrue(liquidGlassIsEnabled(settings: settings))
        } else {
            // Wanting glass is not enough where the system has none to give.
            XCTAssertFalse(liquidGlassIsEnabled(settings: settings))
        }
    }

    @MainActor
    func testIconPaletteDefaultsRejectsInvalidValuesAndAllChoicesRoundTrip() {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }

        XCTAssertEqual(AppSettings(defaults: defaults).iconPalette, .oxblood)
        defaults.set("bogus", forKey: "iconPalette")
        XCTAssertEqual(AppSettings(defaults: defaults).iconPalette, .oxblood)

        var callbacks: [AppPalette] = []
        let settings = AppSettings(defaults: defaults)
        for palette in AppPalette.allCases {
            selectIconPalette(
                palette,
                settings: settings,
                onPaletteChanged: { callbacks.append($0) }
            )
            XCTAssertEqual(defaults.string(forKey: "iconPalette"), palette.rawValue)
            XCTAssertEqual(AppSettings(defaults: defaults).iconPalette, palette)
        }
        XCTAssertEqual(callbacks, AppPalette.allCases)
    }

    @MainActor
    func testApplicationIconSetterReceivesAllPaletteChanges() throws {
        var images: [NSImage] = []
        let controller = ApplicationIconController { images.append($0) }
        for palette in AppPalette.allCases {
            try controller.apply(palette: palette)
        }

        XCTAssertEqual(images.count, AppPalette.allCases.count)
        let pngs = try images.map { image -> Data in
            let representation = try XCTUnwrap(image.representations.first as? NSBitmapImageRep)
            return try XCTUnwrap(representation.representation(using: .png, properties: [:]))
        }
        XCTAssertEqual(Set(pngs).count, AppPalette.allCases.count)
    }

    @MainActor
    func testSettingsIconPreviewsCoverAllPaletteChoices() throws {
        let previews = try AppPalette.allCases.map { palette in
            try applicationIconImage(palette: palette, pixelSize: 128, pointSize: 56)
        }
        XCTAssertEqual(
            previews.map(\.size),
            Array(repeating: NSSize(width: 56, height: 56), count: AppPalette.allCases.count)
        )
        let pngs = try previews.map { image -> Data in
            let representation = try XCTUnwrap(image.representations.first as? NSBitmapImageRep)
            return try XCTUnwrap(representation.representation(using: .png, properties: [:]))
        }
        XCTAssertEqual(Set(pngs).count, AppPalette.allCases.count)
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

    /// The panel is a list of short sections, not a scrolling preferences
    /// window: it has to stay well inside a laptop screen. The ceiling moves
    /// only when a section is genuinely added — the Appearance section put it
    /// up by roughly a section's worth — never to make sprawl fit.
    @MainActor
    func testSettingsPanelContentHeightIsCompact() {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let settings = AppSettings(defaults: defaults)
        let controller = SettingsPanelController(
            settings: settings,
            store: UsageStore(dataSourceFactory: { MockDataSource() }),
            mode: .mock,
            onSaved: {},
            onPaletteChanged: { _ in }
        )
        let height = controller.window?.contentRect(forFrameRect: controller.window!.frame).height
            ?? 0
        XCTAssertGreaterThan(height, 100)
        XCTAssertLessThan(height, 600)
    }

    @MainActor
    func testSettingsPanelUsesStandardTitleBar() throws {
        let suiteName = "UllageMacTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let controller = SettingsPanelController(
            settings: AppSettings(defaults: defaults),
            store: UsageStore(dataSourceFactory: { MockDataSource() }),
            mode: .mock,
            onSaved: {},
            onPaletteChanged: { _ in }
        )
        let window = try XCTUnwrap(controller.window)

        XCTAssertEqual(window.title, "Ullage Settings")
        XCTAssertTrue(window.styleMask.contains(.titled))
        XCTAssertTrue(window.styleMask.contains(.closable))
        XCTAssertFalse(window.styleMask.contains(.fullSizeContentView))
        XCTAssertEqual(window.titleVisibility, .visible)
        XCTAssertFalse(window.titlebarAppearsTransparent)
        XCTAssertGreaterThan(window.frame.height, window.contentLayoutRect.height)
        XCTAssertNotNil(window.standardWindowButton(.closeButton))
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

    func testResetTimeUsesHoursAndMinutesInsideTwoDays() {
        let now = Date(timeIntervalSinceReferenceDate: 0)
        func text(_ seconds: Double) -> String {
            resetRelativeText(now.addingTimeInterval(seconds), now: now)
        }
        // Under an hour keeps the units that still mean something.
        XCTAssertEqual(text(45), "in 45s")
        XCTAssertEqual(text(45 * 60), "in 45m")
        // From an hour to two days, hours and minutes.
        XCTAssertEqual(text(3_600), "in 1h")
        XCTAssertEqual(text(3.5 * 3_600), "in 3h30m")
        XCTAssertEqual(text(25 * 3_600 + 60), "in 25h1m")
        XCTAssertEqual(text(47 * 3_600 + 59 * 60), "in 47h59m")
        // Seconds round into the minute, and a full minute into the hour.
        XCTAssertEqual(text(3_600 + 29), "in 1h")
        XCTAssertEqual(text(3_600 + 31), "in 1h1m")
        XCTAssertEqual(text(3_600 + 59 * 60 + 45), "in 2h")
        // Two days and past it, whole days again.
        XCTAssertEqual(text(48 * 3_600), "in 2d")
        XCTAssertEqual(text(10 * 86_400), "in 10d")
        // The past reads the same way round.
        XCTAssertEqual(text(-3.5 * 3_600), "3h30m ago")
        XCTAssertEqual(text(-10 * 86_400), "10d ago")
    }

    func testSpentQuotaSaysSoInsteadOfShowingZeroPercent() {
        XCTAssertEqual(summaryValueText(.remains(73)), "remains 73%")
        // Rounds to 1%, so the number still carries it.
        XCTAssertEqual(summaryValueText(.remains(0.6)), "remains 1%")
        // Would round to 0% while quota is left: say how small it is instead.
        XCTAssertEqual(summaryValueText(.remains(0.4)), "remains <1%")
        XCTAssertEqual(summaryValueText(.remains(0)), "used up")
        XCTAssertEqual(summaryValueText(.remains(-5)), "used up")
        XCTAssertEqual(summaryValueText(.remains(.nan)), "remains -")
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
        XCTAssertTrue(sections[0].contains("5h | Codex"))
        XCTAssertTrue(sections[1].contains("weekly · Codex"))
        XCTAssertTrue(sections[0].contains("auto"))
        XCTAssertTrue(sections[0].contains("GrokBuild"))
        XCTAssertTrue(output.contains("badges=stale,partial,network"))
        XCTAssertTrue(output.contains("badges=rate_limited"))
        XCTAssertFalse(output.contains("private label"))
    }

    @MainActor
    func testVesselSurfaceKeepsItsHeightAndOnlyTheWaveMoves() {
        let flat = LiquidVessel.surfacePath(surfaceY: 50, wavePhase: nil).boundingRect
        XCTAssertEqual(flat.minY, 50, accuracy: 0.001)
        XCTAssertEqual(flat.height, 0, accuracy: 0.001)

        let phaseA = LiquidVessel.surfacePath(surfaceY: 50, wavePhase: 0).boundingRect
        let phaseB = LiquidVessel.surfacePath(surfaceY: 50, wavePhase: .pi / 2).boundingRect
        for bounds in [phaseA, phaseB] {
            XCTAssertEqual(bounds.midY, 50, accuracy: 0.05)
            XCTAssertEqual(bounds.height, LiquidVessel.amplitude * 2, accuracy: 0.05)
            XCTAssertEqual(bounds.minX, 0)
            XCTAssertEqual(bounds.maxX, 100)
        }
        XCTAssertNotEqual(
            LiquidVessel.surfacePath(surfaceY: 50, wavePhase: 0).description,
            LiquidVessel.surfacePath(surfaceY: 50, wavePhase: .pi / 2).description
        )

        let phase = PopoverHero.wavePhase(at: Date(timeIntervalSinceReferenceDate: 10))
        XCTAssertEqual(phase, (10 * MenuBarLiquidAnimation.waveRadiansPerSecond).truncatingRemainder(dividingBy: 2 * .pi), accuracy: 1e-9)
        XCTAssertGreaterThanOrEqual(phase, 0)
        XCTAssertLessThan(phase, 2 * .pi)
    }

    func testPopoverBubbleAimsItsPointerAndKeepsItOffTheCorners() {
        let rect = CGRect(x: 0, y: 0, width: 360, height: 400)
        // Just under the shoulder, where the pointer is still several points
        // wide but the slab below it has not started yet.
        let probeY = rect.minY + 7

        let centred = PopoverBubble(pointerOffset: 0).path(in: rect)
        XCTAssertTrue(centred.contains(CGPoint(x: rect.midX, y: probeY)))
        XCTAssertFalse(centred.contains(CGPoint(x: rect.midX + 30, y: probeY)))

        // The offset follows the status item until the corner curve, then stops.
        let limit = rect.width / 2 - PopoverBubble.defaultCornerRadius - PopoverBubble.pointerWidth / 2
        let pushed = PopoverBubble(pointerOffset: 1_000).path(in: rect)
        XCTAssertTrue(pushed.contains(CGPoint(x: rect.midX + limit, y: probeY)))
        XCTAssertFalse(pushed.contains(CGPoint(x: rect.midX, y: probeY)))
        XCTAssertFalse(pushed.contains(CGPoint(x: rect.maxX - 2, y: probeY)))

        // The slab still fills the full width below the pointer.
        let belowPointer = rect.minY + PopoverBubble.pointerHeight + 40
        XCTAssertTrue(centred.contains(CGPoint(x: rect.minX + 2, y: belowPointer)))
        XCTAssertTrue(centred.contains(CGPoint(x: rect.maxX - 2, y: belowPointer)))
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
    func testRateLimitCountdownSurvivesStoreRestart() async throws {
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
private func rasterizedMenuBarImage(
    fillRatio: Double?,
    wavePhase: Double? = 0,
    scale: Int = 2
) throws -> NSBitmapImageRep {
    let image = UllageMark.menuBarImage(fillRatio: fillRatio, wavePhase: wavePhase)
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
