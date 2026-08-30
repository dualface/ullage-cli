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

private func date(_ value: String) throws -> Date {
    try #require(RFC3339.date(from: value))
}

// Expected rows mirror summarize_window, measurement_value, and
// metric_display_name in crates/ullage-cli/src/summary.rs.
@Test func providerFixturesMatchCLISummaryProjection() throws {
    let chatGPT = summarize(try usage("chatgpt"))
    #expect(chatGPT.limitReached)
    #expect(chatGPT.rows == [
        SummaryRow(
            window: "5h", metric: "Codex", value: .remains(74.6),
            resetsAt: try date("2026-08-31T05:00:00.123456789Z"),
            remainingRatio: 0.746, disabled: false
        ),
        SummaryRow(
            window: "5h", metric: "requests", value: .counted(used: 12, limit: 20),
            resetsAt: try date("2026-08-31T05:00:00.123456789Z"),
            remainingRatio: 0.4, disabled: false
        ),
        SummaryRow(
            window: "Credits", metric: "credit balance", value: .creditsUnlimited,
            resetsAt: nil, remainingRatio: nil, disabled: false
        ),
    ])
    #expect(chatGPT.rows[0].value.roundedPercentage == 75)

    let claude = summarize(try usage("claude"))
    #expect(claude.rows == [
        SummaryRow(
            window: "5h", metric: "usage", value: .remains(97),
            resetsAt: try date("2026-08-31T05:00:00Z"),
            remainingRatio: 0.97, disabled: false
        ),
        SummaryRow(
            window: "Weekly Opus", metric: "usage", value: .remains(89),
            resetsAt: try date("2026-09-07T00:00:00Z"),
            remainingRatio: 0.89, disabled: false
        ),
    ])

    let cursor = summarize(try usage("cursor"))
    let cursorReset = try date("2026-09-01T00:00:00Z")
    #expect(cursor.rows == [
        SummaryRow(
            window: "Monthly", metric: "total spend",
            value: .spent(amount: 3, limit: 20, currency: Currency(code: "USD")),
            resetsAt: cursorReset, remainingRatio: 0.85, disabled: false
        ),
        SummaryRow(
            window: "Monthly", metric: "usage", value: .remains(85),
            resetsAt: cursorReset, remainingRatio: 0.85, disabled: false
        ),
        SummaryRow(
            window: "Monthly", metric: "on demand spend",
            value: .spent(amount: 0, limit: 50, currency: Currency(code: "USD")),
            resetsAt: cursorReset, remainingRatio: 1, disabled: true
        ),
    ])

    let grok = summarize(try usage("grok"))
    #expect(grok.rows == [
        SummaryRow(
            window: "Weekly", metric: "GrokBuild", value: .used(61),
            resetsAt: try date("2026-09-04T01:18:04.090314+00:00"),
            remainingRatio: nil, disabled: false
        ),
        SummaryRow(
            window: "Monthly", metric: "monthly credits", value: .credits(used: 285, limit: 1000),
            resetsAt: nil, remainingRatio: 0.715, disabled: false
        ),
        SummaryRow(
            window: "Extra Usage Credits", metric: "remaining",
            value: .balance(amount: 12.34, currency: Currency(code: "USD")),
            resetsAt: nil, remainingRatio: nil, disabled: false
        ),
        SummaryRow(
            window: "On-demand usage", metric: "status", value: .disabled,
            resetsAt: nil, remainingRatio: nil, disabled: false
        ),
    ])
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
