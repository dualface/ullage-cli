import Foundation
import Testing
@testable import UllageKit

private func fixture(_ name: String) throws -> SnapshotPayload {
    let url = try #require(Bundle.module.url(forResource: name, withExtension: "json", subdirectory: "Fixtures"))
    return try UllageJSON.makeDecoder().decode(SnapshotPayload.self, from: Data(contentsOf: url))
}

private func usage(_ name: String) throws -> SubscriptionUsage {
    try #require(fixture(name).usage.data)
}

// Expected rows mirror summarize_window, measurement_value, and
// metric_display_name in crates/ullage-cli/src/summary.rs.
@Test func providerFixturesMatchCLISummaryProjection() throws {
    let chatGPT = summarize(try usage("chatgpt"))
    #expect(chatGPT.limitReached)
    #expect(chatGPT.rows.map(\.metric) == ["Codex", "requests", "credit balance"])
    #expect(chatGPT.rows[0].value == .remains(74.6))
    #expect(chatGPT.rows[1].value == .counted(used: 12, limit: 20))
    #expect(chatGPT.rows[1].remainingRatio == 0.4)
    #expect(chatGPT.rows[2].value == .creditsUnlimited)
    #expect(chatGPT.rows[0].resetsAt != nil)

    let claude = summarize(try usage("claude"))
    #expect(claude.rows.map(\.window) == ["5h", "Weekly Opus"])
    #expect(claude.rows.map(\.value) == [.remains(97), .remains(89)])

    let cursor = summarize(try usage("cursor"))
    #expect(cursor.rows.map(\.metric) == ["total spend", "usage", "on demand spend"])
    #expect(cursor.rows[0].value == .spent(amount: 3, limit: 20, currency: Currency(code: "USD")))
    #expect(cursor.rows[0].remainingRatio == 0.85)
    #expect(cursor.rows[2].disabled)

    let grok = summarize(try usage("grok"))
    #expect(grok.rows[0].value == .used(61))
    #expect(grok.rows[1].value == .credits(used: 285, limit: 1000))
    #expect(grok.rows[1].remainingRatio == 0.715)
    #expect(grok.rows[2].value == .balance(amount: 12.34, currency: Currency(code: "USD")))
    #expect(grok.rows[3].value == .disabled)
}

@Test func overviewUsesPoolThenRatioThenFirstRowAndFiltersOtherWindows() throws {
    let cursorRows = overviewRows(for: try usage("cursor"))
    #expect(cursorRows.count == 1)
    #expect(cursorRows[0].metric == "usage")

    let chatGPTRows = overviewRows(for: try usage("chatgpt"))
    #expect(chatGPTRows.map(\.metric) == ["Codex"])

    let grokRows = overviewRows(for: try usage("grok"))
    #expect(grokRows.count == 2)
    #expect(grokRows[0].window == "Weekly")
    #expect(grokRows[0].metric == "GrokBuild")
    #expect(grokRows[1].window == "Monthly")
    #expect(!grokRows.contains(where: { $0.window == "Extra Usage Credits" }))
}

@Test func hiddenMeasurementsNeverBecomeRows() throws {
    let allRows = try ["chatgpt", "claude", "cursor", "grok"]
        .flatMap { summarize(try usage($0)).rows }
    let forbidden = Set([
        "allowed", "limit reached", "has credits", "unlimited", "on demand enabled",
        "enabled", "included spend", "bonus spend",
    ])
    #expect(allRows.allSatisfy { !forbidden.contains($0.metric) })
}
