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
            window: "Rate limit reset credits", metric: "available count",
            value: .credits(used: 1, limit: nil),
            resetsAt: nil, remainingRatio: nil, disabled: false
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
        SummaryRow(
            window: "Fable (weekly_scoped)", metric: "usage", value: .remains(78),
            resetsAt: try date("2026-09-07T00:00:00Z"),
            remainingRatio: 0.78, disabled: false
        ),
    ])

    let cursor = summarize(try usage("cursor"))
    let cursorReset = try date("2026-09-01T00:00:00Z")
    #expect(cursor.rows == [
        SummaryRow(
            window: "monthly", metric: "total spend",
            value: .spent(amount: 3, limit: 20, currency: Currency(code: "USD")),
            resetsAt: cursorReset, remainingRatio: nil, disabled: false
        ),
        SummaryRow(
            window: "monthly", metric: "auto", value: .remains(70),
            resetsAt: cursorReset, remainingRatio: 0.7, disabled: false
        ),
        SummaryRow(
            window: "monthly", metric: "api", value: .remains(77),
            resetsAt: cursorReset, remainingRatio: 0.77, disabled: false
        ),
        SummaryRow(
            window: "monthly", metric: "usage", value: .remains(18),
            resetsAt: cursorReset, remainingRatio: 0.18, disabled: false
        ),
        SummaryRow(
            window: "monthly", metric: "on demand spend",
            value: .spent(amount: 0, limit: 50, currency: Currency(code: "USD")),
            resetsAt: cursorReset, remainingRatio: nil, disabled: true
        ),
    ])

    let grok = summarize(try usage("grok"))
    #expect(grok.rows == [
        SummaryRow(
            window: "weekly", metric: "usage", value: .remains(40),
            resetsAt: try date("2026-09-04T01:18:04.090314+00:00"),
            remainingRatio: 0.4, disabled: false
        ),
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

@Test func overviewUsesTheProviderCatalogInListedOrder() throws {
    let chatGPTRows = overviewRows(for: try usage("chatgpt"))
    #expect(chatGPTRows.map(\.window) == [
        "weekly · Codex",
        "weekly · GPT-5.3-Codex-Spark",
        "Rate limit reset credits",
    ])
    #expect(chatGPTRows.map(\.metric) == ["Codex", "GPT-5.3-Codex-Spark", "available count"])
    #expect(chatGPTRows.map(\.remainingRatio) == [0.69, 0.58, nil])

    let claudeRows = overviewRows(for: try usage("claude"))
    #expect(claudeRows.map(\.window) == ["5h", "Fable (weekly_scoped)"])
    #expect(claudeRows.map(\.remainingRatio) == [0.97, 0.78])

    let cursorRows = overviewRows(for: try usage("cursor"))
    #expect(cursorRows.map(\.metric) == ["auto", "api"])
    #expect(cursorRows.map(\.window) == ["monthly", "monthly"])
    #expect(cursorRows.map(\.remainingRatio) == [0.7, 0.77])

    let grokRows = overviewRows(for: try usage("grok"))
    #expect(grokRows.map(\.metric) == ["usage", "GrokBuild"])
    #expect(grokRows.map(\.window) == ["weekly", "weekly"])
    #expect(grokRows.map(\.remainingRatio) == [0.4, 0.45])
    #expect(Set(overviewItems(for: try usage("grok")).map(\.id)).count == 2)
}

@Test func overviewSkipsMissingCatalogItems() throws {
    let data = Data(#"""
    {
      "provider":"claude","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-09-01T00:00:00Z",
      "windows":[
        {"window":{"kind":"weekly"},"resets_at":null,"measurements":[
          {"name":"included_usage","used":10,"limit":100,"unit":{"kind":"percent"}}
        ]}
      ]
    }
    """#.utf8)
    let usage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: data)
    #expect(overviewRows(for: usage).isEmpty)
}

@Test func unknownProvidersKeepTheShortestKnownWindowTier() throws {
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

@Test func unknownProvidersKeepMultipleShortestWindowsInStableOrder() throws {
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

@Test func unknownProvidersKeepUniqueStableIDsForDuplicateDisplayNames() throws {
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

@Test func unknownProvidersKeepAllOtherAndUnknownWindowsWhenTheyAreTheOnlyTier() throws {
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
    #expect(menuBarFillRatio(accounts: accounts, snapshots: snapshots) == 0.4)

    let cursorDisabled = accounts.map {
        Account(id: $0.id, provider: $0.provider, label: $0.label, enabled: $0.id != "fixture-cursor")
    }
    #expect(menuBarFillRatio(accounts: cursorDisabled, snapshots: snapshots) == 0.4)
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

@Test func menuBarFillIgnoresLimitsOutsideTheCatalog() throws {
    let snapshot = try fixture("chatgpt")
    let account = Account(id: snapshot.accountId, provider: "chatgpt", label: nil, enabled: true)
    #expect(menuBarFillRatio(accounts: [account], snapshots: [snapshot]) == 0.58)
}

@Test func menuBarFillReturnsZeroForCatalogWindowLimitsAndNilWithoutRows() throws {
    let limitedData = Data(#"""
    {
      "provider":"chatgpt","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-09-01T00:00:00Z",
      "windows":[
        {"window":{"kind":"five_hours"},"resets_at":null,"measurements":[
          {"name":"codex_usage","used":10,"limit":100,"unit":{"kind":"percent"}},
          {"name":"limit_reached","used":1,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}}
        ]},
        {"window":{"kind":"weekly"},"resets_at":null,"measurements":[
          {"name":"codex_usage","used":20,"limit":100,"unit":{"kind":"percent"}},
          {"name":"limit_reached","used":1,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}}
        ]}
      ]
    }
    """#.utf8)
    let limitedUsage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: limitedData)
    let limited = SnapshotPayload(
        accountId: "chatgpt", usage: .complete(limitedUsage), lastSuccessAt: Date(),
        stale: false, lastError: nil, lastErrorAt: nil
    )
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

@Test func menuBarAccountLevelsFollowStableAccountOrder() throws {
    let snapshots = try [fixture("claude"), fixture("cursor"), fixture("grok")]
    let accounts = [
        Account(id: "fixture-cursor", provider: "cursor", label: nil, enabled: true),
        Account(id: "fixture-claude", provider: "claude", label: nil, enabled: true),
        Account(id: "fixture-grok", provider: "grok", label: nil, enabled: true),
    ]
    let levels = menuBarAccountLevels(accounts: accounts, snapshots: snapshots)
    #expect(levels.map(\.accountID) == ["fixture-claude", "fixture-cursor", "fixture-grok"])
    #expect(levels.map(\.remainingRatio) == [0.78, 0.7, 0.4])
    #expect(menuBarFillRatio(accounts: accounts, snapshots: snapshots) == 0.4)
}

@Test func menuBarAccountLevelsKeepZeroWhenLimitReached() throws {
    let limitedData = Data(#"""
    {
      "provider":"chatgpt","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-09-01T00:00:00Z",
      "windows":[
        {"window":{"kind":"five_hours"},"resets_at":null,"measurements":[
          {"name":"codex_usage","used":10,"limit":100,"unit":{"kind":"percent"}},
          {"name":"limit_reached","used":1,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}}
        ]},
        {"window":{"kind":"weekly"},"resets_at":null,"measurements":[
          {"name":"codex_usage","used":20,"limit":100,"unit":{"kind":"percent"}},
          {"name":"limit_reached","used":1,"limit":1,"unit":{"kind":"other","id":"boolean","label":"Boolean"}}
        ]}
      ]
    }
    """#.utf8)
    let limitedUsage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: limitedData)
    let limited = SnapshotPayload(
        accountId: "chatgpt", usage: .complete(limitedUsage), lastSuccessAt: Date(),
        stale: false, lastError: nil, lastErrorAt: nil
    )
    let cursor = try fixture("cursor")
    let accounts = [
        Account(id: "chatgpt", provider: "chatgpt", label: nil, enabled: true),
        Account(id: cursor.accountId, provider: "cursor", label: nil, enabled: true),
    ]
    let levels = menuBarAccountLevels(accounts: accounts, snapshots: [limited, cursor])
    #expect(levels.map(\.accountID) == ["chatgpt", cursor.accountId])
    #expect(levels.first?.remainingRatio == 0)
}

@Test func menuBarLiquidFloorIsTheLowestAccountInStableOrder() {
    let levels = [
        MenuBarAccountLevel(accountID: "a", displayName: "A", remainingRatio: 0.6),
        MenuBarAccountLevel(accountID: "b", displayName: "B", remainingRatio: 0.2),
        MenuBarAccountLevel(accountID: "c", displayName: "C", remainingRatio: 0.2),
    ]
    #expect(MenuBarLiquidAnimation.floorLevel(in: levels)?.accountID == "b")
    #expect(MenuBarLiquidAnimation.floorLevel(in: []) == nil)
}

@Test func menuBarLiquidCycleRatioIsFullAtTheEndsAndFloorHalfway() {
    let cycle = MenuBarLiquidAnimation.cycleDuration
    #expect(cycle == 40)
    #expect(MenuBarLiquidAnimation.cycleRatio(floor: 0.2, secondsInCycle: 0) == 1)
    #expect(abs(MenuBarLiquidAnimation.cycleRatio(floor: 0.2, secondsInCycle: cycle / 2) - 0.2) < 1e-9)
    #expect(abs(MenuBarLiquidAnimation.cycleRatio(floor: 0.2, secondsInCycle: cycle) - 1) < 1e-9)
    let quarter = MenuBarLiquidAnimation.cycleRatio(floor: 0.2, secondsInCycle: cycle / 4)
    let threeQuarters = MenuBarLiquidAnimation.cycleRatio(floor: 0.2, secondsInCycle: cycle * 3 / 4)
    #expect(abs(quarter - 0.6) < 1e-9)
    #expect(abs(quarter - threeQuarters) < 1e-9)
    #expect(MenuBarLiquidAnimation.cycleRatio(floor: 1, secondsInCycle: cycle / 2) == 1)
    #expect(MenuBarLiquidAnimation.cycleRatio(floor: -3, secondsInCycle: cycle / 2) == 0)
}

@Test func menuBarLiquidBreathesFromFullToFloorAndBack() {
    let levels = [
        MenuBarAccountLevel(accountID: "a", displayName: "A", remainingRatio: 0.8),
        MenuBarAccountLevel(accountID: "b", displayName: "B", remainingRatio: 0.2),
    ]
    var state = MenuBarLiquidAnimation.advance(
        state: MenuBarLiquidAnimationState(), levels: levels, gate: .animate, dt: 0
    )
    #expect(state.displayedRatio == 1)
    #expect(state.floorRatio == 0.2)
    #expect(state.floorAccountID == "b")
    #expect(state.floorDisplayName == "B")

    let half = MenuBarLiquidAnimation.cycleDuration / 2
    let frames = 40
    var previous = state.displayedRatio
    for _ in 0..<frames {
        state = MenuBarLiquidAnimation.advance(
            state: state, levels: levels, gate: .animate, dt: half / Double(frames)
        )
        #expect(state.displayedRatio <= previous + 1e-9)
        previous = state.displayedRatio
    }
    #expect(abs(state.displayedRatio - 0.2) < 1e-6)

    for _ in 0..<frames {
        state = MenuBarLiquidAnimation.advance(
            state: state, levels: levels, gate: .animate, dt: half / Double(frames)
        )
        #expect(state.displayedRatio >= previous - 1e-9)
        previous = state.displayedRatio
    }
    #expect(abs(state.displayedRatio - 1) < 1e-6)
    let cycle = MenuBarLiquidAnimation.cycleDuration
    #expect(min(state.secondsInCycle, cycle - state.secondsInCycle) < 1e-6)
}

@Test func menuBarLiquidFollowsANewFloorMidCycle() {
    var levels = [
        MenuBarAccountLevel(accountID: "a", displayName: "A", remainingRatio: 0.4),
    ]
    var state = MenuBarLiquidAnimation.advance(
        state: MenuBarLiquidAnimationState(),
        levels: levels,
        gate: .animate,
        dt: MenuBarLiquidAnimation.cycleDuration / 2
    )
    #expect(abs(state.displayedRatio - 0.4) < 1e-9)

    levels.append(MenuBarAccountLevel(accountID: "b", displayName: "B", remainingRatio: 0.1))
    state = MenuBarLiquidAnimation.advance(state: state, levels: levels, gate: .animate, dt: 0)
    #expect(state.floorAccountID == "b")
    #expect(abs(state.displayedRatio - 0.1) < 1e-9)
}

@Test func menuBarLiquidCatchesUpAfterLongStall() {
    let levels = [
        MenuBarAccountLevel(accountID: "a", displayName: "A", remainingRatio: 0.2),
    ]
    let cycle = MenuBarLiquidAnimation.cycleDuration
    let state = MenuBarLiquidAnimation.advance(
        state: MenuBarLiquidAnimationState(secondsInCycle: 1),
        levels: levels,
        gate: .animate,
        dt: cycle * 9 + 2
    )
    #expect(abs(state.secondsInCycle - 3) < 1e-6)
    #expect(abs(state.displayedRatio - MenuBarLiquidAnimation.cycleRatio(floor: 0.2, secondsInCycle: 3)) < 1e-9)
    #expect(state.displayedRatio >= 0.2)
    #expect(state.displayedRatio <= 1)
}

@Test func menuBarLiquidFreezeParksAtTheFloorAndResumesUpward() {
    let levels = [
        MenuBarAccountLevel(accountID: "a", displayName: "A", remainingRatio: 0.2),
        MenuBarAccountLevel(accountID: "b", displayName: "B", remainingRatio: 0.9),
    ]
    let start = MenuBarLiquidAnimationState(
        displayedRatio: 0.7,
        floorRatio: 0.2,
        floorAccountID: "a",
        floorDisplayName: "A",
        wavePhase: 1.5,
        secondsInCycle: 1
    )
    let frozen = MenuBarLiquidAnimation.advance(
        state: start, levels: levels, gate: .freeze, dt: 10
    )
    #expect(frozen.floorAccountID == "a")
    #expect(frozen.wavePhase == 1.5)
    #expect(frozen.displayedRatio == 0.2)
    #expect(frozen.secondsInCycle == MenuBarLiquidAnimation.cycleDuration / 2)

    let resumed = MenuBarLiquidAnimation.advance(
        state: frozen, levels: levels, gate: .animate, dt: 0.1
    )
    #expect(resumed.displayedRatio > 0.2)
    #expect(resumed.displayedRatio < 0.25)
    #expect(resumed.wavePhase > 1.5)
}

@Test func menuBarLiquidStopLeavesStateUntouched() {
    let levels = [
        MenuBarAccountLevel(accountID: "a", displayName: "A", remainingRatio: 0.5),
    ]
    let start = MenuBarLiquidAnimationState(
        displayedRatio: 0.5,
        floorRatio: 0.5,
        floorAccountID: "a",
        floorDisplayName: "A",
        wavePhase: 3,
        secondsInCycle: 1.2
    )
    let stopped = MenuBarLiquidAnimation.advance(
        state: start, levels: levels, gate: .stop, dt: 5
    )
    #expect(stopped == start)
    let empty = MenuBarLiquidAnimation.advance(
        state: start, levels: [], gate: .animate, dt: 5
    )
    #expect(empty == start)
}
