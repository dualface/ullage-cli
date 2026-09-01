import Foundation
import Testing
@testable import UllageKit

private func fixture(_ name: String) throws -> SnapshotPayload {
    try UllageFixtures.snapshot(named: name)
}

private func usage(_ name: String) throws -> SubscriptionUsage {
    try #require(fixture(name).usage.data)
}

private func date(_ value: String) throws -> Date {
    try #require(RFC3339.date(from: value))
}

// Measurement values and metric names mirror the CLI. Duplicate-kind window
// qualification is specific to the Mac projection.
@Test func providerFixturesMatchCLIValuesAndMacWindowNaming() throws {
    let chatGPT = summarize(try usage("chatgpt"))
    #expect(chatGPT.limitReached)
    #expect(chatGPT.rows == [
        SummaryRow(
            window: "5h", metric: "Codex", value: .remains(6),
            resetsAt: try date("2026-08-31T05:00:00.123456789Z"),
            remainingRatio: 0.06, disabled: false
        ),
        SummaryRow(
            window: "5h", metric: "requests", value: .counted(used: 12, limit: 20),
            resetsAt: try date("2026-08-31T05:00:00.123456789Z"),
            remainingRatio: 0.4, disabled: false
        ),
        SummaryRow(
            window: "weekly · Codex", metric: "Codex", value: .remains(69),
            resetsAt: try date("2026-09-07T00:00:00Z"),
            remainingRatio: 0.69, disabled: false
        ),
        SummaryRow(
            window: "weekly · GPT-5.3-Codex-Spark", metric: "GPT-5.3-Codex-Spark",
            value: .remains(58), resetsAt: try date("2026-09-07T00:00:00Z"),
            remainingRatio: 0.58, disabled: false
        ),
        SummaryRow(
            window: "Credits", metric: "credit balance", value: .creditsUnlimited,
            resetsAt: nil, remainingRatio: nil, disabled: false
        ),
    ])
    #expect(chatGPT.rows[0].value.roundedPercentage == 6)
    #expect(SummaryValue.used(Double.greatestFiniteMagnitude).roundedPercentage == nil)

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
            window: "monthly", metric: "total spend",
            value: .spent(amount: 3, limit: 20, currency: Currency(code: "USD")),
            resetsAt: cursorReset, remainingRatio: 0.85, disabled: false
        ),
        SummaryRow(
            window: "monthly", metric: "usage", value: .remains(18),
            resetsAt: cursorReset, remainingRatio: 0.18, disabled: false
        ),
        SummaryRow(
            window: "monthly", metric: "on demand spend",
            value: .spent(amount: 0, limit: 50, currency: Currency(code: "USD")),
            resetsAt: cursorReset, remainingRatio: 1, disabled: true
        ),
    ])

    let grok = summarize(try usage("grok"))
    #expect(grok.rows == [
        SummaryRow(
            window: "weekly", metric: "GrokBuild", value: .remains(45),
            resetsAt: try date("2026-09-04T01:18:04.090314+00:00"),
            remainingRatio: 0.45, disabled: false
        ),
        SummaryRow(
            window: "monthly", metric: "monthly credits", value: .credits(used: 285, limit: 1000),
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

@Test func remainingTierIncludesThresholdsInTheLowerTier() {
    #expect(RemainingTier(ratio: 0.51) == .healthy)
    #expect(RemainingTier(ratio: 0.50) == .caution)
    #expect(RemainingTier(ratio: 0.26) == .caution)
    #expect(RemainingTier(ratio: 0.25) == .low)
    #expect(RemainingTier(ratio: 0.11) == .low)
    #expect(RemainingTier(ratio: 0.10) == .critical)
}

@Test func duplicateUnknownWindowKindsUseTheirRepresentativeMetrics() throws {
    let data = Data(#"""
    {
      "provider":"future","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-08-31T00:00:00Z",
      "windows":[
        {"window":{"kind":"daily"},"resets_at":null,"measurements":[
          {"name":"alpha_usage","used":10,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"daily"},"resets_at":null,"measurements":[
          {"name":"beta_usage","used":20,"limit":100,"unit":{"kind":"percent"}}
        ]}
      ]
    }
    """#.utf8)
    let usage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: data)
    #expect(summarize(usage).rows.map(\.window) == ["daily · alpha", "daily · beta"])
}

@Test func overviewUsesPoolThenRatioThenFirstRowForTheShortestWindows() throws {
    let cursorRows = overviewRows(for: try usage("cursor"))
    #expect(cursorRows.count == 1)
    #expect(cursorRows[0].metric == "usage")

    let chatGPTRows = overviewRows(for: try usage("chatgpt"))
    #expect(chatGPTRows.map(\.metric) == ["Codex"])
    #expect(chatGPTRows.map(\.window) == ["5h"])

    let grokRows = overviewRows(for: try usage("grok"))
    #expect(grokRows.count == 1)
    #expect(grokRows[0].window == "weekly")
    #expect(grokRows[0].metric == "GrokBuild")

    #expect(overviewRows(for: try usage("claude"))[0].remainingRatio == 0.97)
    #expect(grokRows[0].remainingRatio == 0.45)
    #expect(overviewRows(for: try usage("cursor"))[0].remainingRatio == 0.18)
    #expect(chatGPTRows[0].remainingRatio == 0.06)
}

@Test func overviewKeepsOnlyTheShortestKnownWindowTier() throws {
    let data = Data(#"""
    {
      "provider":"future","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-09-01T00:00:00Z",
      "windows":[
        {"window":{"kind":"daily"},"resets_at":null,"measurements":[
          {"name":"usage","used":10,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"monthly"},"resets_at":null,"measurements":[
          {"name":"usage","used":20,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"other","id":"custom","label":"Custom quota"},"resets_at":null,"measurements":[
          {"name":"usage","used":30,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"weekly"},"resets_at":null,"measurements":[
          {"name":"usage","used":40,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"five_hours"},"resets_at":null,"measurements":[
          {"name":"usage","used":50,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"other","id":"fallback","label":"   "},"resets_at":null,"measurements":[
          {"name":"usage","used":60,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"other","id":"rate_limit_status","label":"Rate limit status"},"resets_at":null,"measurements":[
          {"name":"allowed","used":0,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}},
          {"name":"limit_reached","used":1,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}}
        ]},
        {"window":{"kind":"future_status"},"resets_at":null,"measurements":[
          {"name":"limit_reached","used":0,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}}
        ]},
        {"window":{"kind":"other","id":"conflicting_status","label":"Conflicting status"},"resets_at":null,"measurements":[
          {"name":"allowed","used":1,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}},
          {"name":"limit_reached","used":1,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}}
        ]}
      ]
    }
    """#.utf8)
    let usage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: data)

    let rows = overviewRows(for: usage)
    #expect(rows.map(\.window) == ["5h"])
    #expect(rows.map(\.remainingRatio) == [0.5])
}

@Test func overviewKeepsMultipleShortestWindowsInStableOrder() throws {
    let data = Data(#"""
    {
      "provider":"test","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-09-01T00:00:00Z",
      "windows":[
        {"window":{"kind":"five_hours"},"resets_at":"2026-09-01T05:00:00Z","measurements":[
          {"name":"included_usage","used":10,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"weekly"},"resets_at":null,"measurements":[
          {"name":"usage","used":90,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"five_hours"},"resets_at":"2026-09-01T10:00:00Z","measurements":[
          {"name":"requests","used":20,"limit":100,"unit":{"kind":"count"}}
        ]}
      ]
    }
    """#.utf8)
    let usage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: data)

    let rows = overviewRows(for: usage)
    #expect(rows.map(\.window) == ["5h · usage", "5h · requests"])
    #expect(rows.map(\.metric) == ["usage", "requests"])
}

@Test func overviewItemsKeepUniqueStableIDsForDuplicateDisplayNames() throws {
    let data = Data(#"""
    {
      "provider":"test","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-09-01T00:00:00Z",
      "windows":[
        {"window":{"kind":"five_hours"},"resets_at":"2026-09-01T05:00:00Z","measurements":[
          {"name":"usage","used":10,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"five_hours"},"resets_at":"2026-09-01T10:00:00Z","measurements":[
          {"name":"usage","used":20,"limit":100,"unit":{"kind":"percent"}}
        ]}
      ]
    }
    """#.utf8)
    let usage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: data)
    let updatedData = Data(String(decoding: data, as: UTF8.self)
        .replacingOccurrences(of: "\"used\":10", with: "\"used\":30")
        .utf8)
    let updatedUsage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: updatedData)

    let items = overviewItems(for: usage)
    #expect(items.map(\.row.window) == ["5h · usage", "5h · usage"])
    #expect(Set(items.map(\.id)).count == 2)
    #expect(items.map(\.id) == overviewItems(for: updatedUsage).map(\.id))
}

@Test func overviewKeepsAllOtherAndUnknownWindowsWhenTheyAreTheOnlyTier() throws {
    let data = Data(#"""
    {
      "provider":"future","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-09-01T00:00:00Z",
      "windows":[
        {"window":{"kind":"daily"},"resets_at":null,"measurements":[
          {"name":"usage","used":10,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"other","id":"custom","label":"Custom quota"},"resets_at":null,"measurements":[
          {"name":"usage","used":20,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"other","id":"custom-2","label":"Custom quota"},"resets_at":null,"measurements":[
          {"name":"usage","used":30,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"future_status"},"resets_at":null,"measurements":[
          {"name":"limit_reached","used":1,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}}
        ]}
      ]
    }
    """#.utf8)
    let usage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: data)

    let items = overviewItems(for: usage)
    let rows = items.map(\.row)
    #expect(rows.map(\.window) == ["daily", "Custom quota", "Custom quota", "future_status"])
    #expect(rows.map(\.remainingRatio) == [0.9, 0.8, 0.7, 0])
    #expect(Set(items.map(\.id)).count == items.count)
}

@Test func menuBarFillUsesTheMinimumAcrossEnabledAccounts() throws {
    let snapshots = try [fixture("claude"), fixture("cursor"), fixture("grok")]
    let accounts = snapshots.map {
        Account(id: $0.accountId, provider: "fixture", label: nil, enabled: true)
    }
    #expect(menuBarFillRatio(accounts: accounts, snapshots: snapshots) == 0.18)

    let cursorDisabled = accounts.map {
        Account(id: $0.id, provider: $0.provider, label: $0.label, enabled: $0.id != "fixture-cursor")
    }
    #expect(menuBarFillRatio(accounts: cursorDisabled, snapshots: snapshots) == 0.45)
}

@Test func menuBarFillSkipsDisabledRows() throws {
    let data = Data(#"""
    {
      "provider":"test","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-09-01T00:00:00Z",
      "windows":[
        {"window":{"kind":"five_hours"},"resets_at":null,"measurements":[
          {"name":"enabled","used":0,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}},
          {"name":"total","used":99,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"weekly"},"resets_at":null,"measurements":[
          {"name":"total","used":60,"limit":100,"unit":{"kind":"percent"}}
        ]}
      ]
    }
    """#.utf8)
    let usage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: data)
    let snapshot = SnapshotPayload(
        accountId: "test", usage: .complete(usage), lastSuccessAt: Date(),
        stale: true, lastError: .network, lastErrorAt: Date()
    )
    let account = Account(id: "test", provider: "test", label: nil, enabled: true)
    #expect(menuBarFillRatio(accounts: [account], snapshots: [snapshot]) == nil)
}

@Test func menuBarFillIgnoresLongerWindowRatiosAndLimits() throws {
    let data = Data(#"""
    {
      "provider":"test","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-09-01T00:00:00Z",
      "windows":[
        {"window":{"kind":"five_hours"},"resets_at":null,"measurements":[
          {"name":"usage","used":10,"limit":100,"unit":{"kind":"percent"}}
        ]},
        {"window":{"kind":"weekly"},"resets_at":null,"measurements":[
          {"name":"usage","used":99,"limit":100,"unit":{"kind":"percent"}},
          {"name":"limit_reached","used":1,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}}
        ]}
      ]
    }
    """#.utf8)
    let usage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: data)
    let snapshot = SnapshotPayload(
        accountId: "test", usage: .complete(usage), lastSuccessAt: Date(),
        stale: false, lastError: nil, lastErrorAt: nil
    )
    let account = Account(id: "test", provider: "test", label: nil, enabled: true)

    #expect(menuBarFillRatio(accounts: [account], snapshots: [snapshot]) == 0.9)
}

@Test func menuBarFillReturnsZeroForReachedLimitsAndNilWithoutRows() throws {
    let limited = try fixture("chatgpt")
    let account = Account(id: limited.accountId, provider: "chatgpt", label: nil, enabled: true)
    #expect(menuBarFillRatio(accounts: [account], snapshots: [limited]) == 0)

    let unknown = SnapshotPayload(
        accountId: account.id, usage: .unknown("unavailable"), lastSuccessAt: Date(),
        stale: false, lastError: .network, lastErrorAt: Date()
    )
    #expect(menuBarFillRatio(accounts: [account], snapshots: [unknown]) == nil)
    #expect(menuBarFillRatio(accounts: [], snapshots: [limited]) == nil)
}

@Test func menuBarFillQuantizationUsesTwentyEqualSteps() {
    #expect(quantizedMenuBarFillRatio(-1) == 0)
    #expect(quantizedMenuBarFillRatio(0.0249) == 0)
    #expect(quantizedMenuBarFillRatio(0.0251) == 0.05)
    #expect(quantizedMenuBarFillRatio(0.5249) == 0.5)
    #expect(quantizedMenuBarFillRatio(0.5251) == 0.55)
    #expect(quantizedMenuBarFillRatio(2) == 1)
}
