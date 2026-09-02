import Foundation

private let hiddenMeasurements: Set<String> = [
    "allowed",
    "limit_reached",
    "has_credits",
    "unlimited",
    "on_demand_enabled",
    "enabled",
    "included_spend",
    "bonus_spend",
]

private let poolMeasurements: Set<String> = ["included_usage", "total", "weekly_pool"]
private let brandNames = ["codex": "Codex"]

public struct Currency: Equatable, Sendable {
    public let code: String
}

public enum SummaryValue: Equatable, Sendable {
    case remains(Double)
    case used(Double)
    case balance(amount: Double, currency: Currency)
    case spent(amount: Double, limit: Double, currency: Currency)
    case credits(used: Double, limit: Double?)
    case creditsUnlimited
    case counted(used: Double, limit: Double?)
    case disabled

    public var roundedPercentage: Int? {
        let percentage: Double
        switch self {
        case .remains(let value), .used(let value): percentage = value
        default: return nil
        }
        guard percentage.isFinite else { return nil }
        return Int(exactly: percentage.rounded())
    }
}

public struct SummaryRow: Equatable, Sendable {
    public let window: String
    public let metric: String
    public let value: SummaryValue
    public let resetsAt: Date?
    /// Fraction of quota still available, `0.0...1.0`, when the value is remaining
    /// quota. Money spent is an amount, so it stays `nil`.
    public let remainingRatio: Double?
    public let disabled: Bool
}

public struct UsageSummary: Equatable, Sendable {
    public let rows: [SummaryRow]
    public let limitReached: Bool
    public let observedAt: Date
    public let expiresAt: Date?

    public var isEmpty: Bool { rows.isEmpty }
}

public struct OverviewItem: Equatable, Identifiable, Sendable {
    public struct ID: Hashable, Sendable {
        fileprivate let windowKey: String
        fileprivate let measurementName: String
        fileprivate let occurrence: Int
    }

    public let id: ID
    public let row: SummaryRow
    fileprivate let windowIndex: Int

    /// Stable across refreshes: `accountID|windowKey|measurement|occurrence`.
    public func persistenceID(accountID: String) -> String {
        "\(accountID)|\(id.windowKey)|\(id.measurementName)|\(id.occurrence)"
    }
}

/// One summarized account-tab row with a stable identity.
public struct IdentifiedSummaryRow: Equatable, Identifiable, Sendable {
    public let id: OverviewItem.ID
    public let row: SummaryRow
    fileprivate let windowIndex: Int

    public func persistenceID(accountID: String) -> String {
        "\(accountID)|\(id.windowKey)|\(id.measurementName)|\(id.occurrence)"
    }
}

public func badgeNames(
    account: Account,
    snapshot: SnapshotPayload,
    summary: UsageSummary
) -> [String] {
    var values: [String] = []
    if snapshot.stale { values.append("stale") }
    if case .partial = snapshot.usage { values.append("partial") }
    if summary.limitReached { values.append("limit reached") }
    if !account.enabled { values.append("disabled") }
    if case .authenticationInvalid? = snapshot.lastError { values.append("auth invalid") }
    if let kind = sanitizedErrorKind(snapshot.lastError) { values.append(kind) }
    return values.reduce(into: []) { result, value in
        if !result.contains(value) { result.append(value) }
    }
}

public func sanitizedErrorKind(_ error: SanitizedErrorPayload?) -> String? {
    guard let error else { return nil }
    return switch error {
    case .authenticationInvalid: "authentication_invalid"
    case .rateLimited: "rate_limited"
    case .network: "network"
    case .protocolIncompatible: "protocol_incompatible"
    case .unsupportedCapability: "unsupported_capability"
    case .timeout: "timeout"
    case .cancelled: "cancelled"
    case .providerNotFound: "provider_not_found"
    case .storage: "storage"
    case .unknown(let kind): kind
    }
}

public func summarize(_ usage: SubscriptionUsage) -> UsageSummary {
    UsageSummary(
        rows: projectedWindows(for: usage).flatMap { $0.map(\.row) },
        limitReached: usage.windows.contains(where: windowHitItsLimit),
        observedAt: usage.observedAt,
        expiresAt: usage.subscriptionExpiresAt
    )
}

public func overviewRows(for usage: SubscriptionUsage) -> [SummaryRow] {
    overviewItems(for: usage).map(\.row)
}

public func overviewItems(for usage: SubscriptionUsage) -> [OverviewItem] {
    let windows = projectedWindows(for: usage)
    var occurrences: [OverviewIdentityBase: Int] = [:]
    return overviewSelections(windows: windows, usage: usage)
        .compactMap { selection -> OverviewItem? in
            let window = usage.windows[selection.windowIndex]
            let selected = selection.projected
            let allowFallback = selection.usesRepresentative
            guard let row = selected?.row ?? (allowFallback ? overviewFallbackRow(for: window) : nil)
            else { return nil }
            let identity = OverviewIdentityBase(
                windowKey: overviewIdentityKey(window.window),
                measurementName: selected?.measurementName ?? row.metric
            )
            let occurrence = occurrences[identity, default: 0]
            occurrences[identity] = occurrence + 1
            return OverviewItem(
                id: OverviewItem.ID(
                    windowKey: identity.windowKey,
                    measurementName: identity.measurementName,
                    occurrence: occurrence
                ),
                row: row,
                windowIndex: selection.windowIndex
            )
        }
}

public func identifiedSummaryRows(for usage: SubscriptionUsage) -> [IdentifiedSummaryRow] {
    let windows = projectedWindows(for: usage)
    var occurrences: [OverviewIdentityBase: Int] = [:]
    return zip(usage.windows.indices, windows).flatMap { windowIndex, projected -> [IdentifiedSummaryRow] in
        let window = usage.windows[windowIndex]
        return projected.map { item in
            let identity = OverviewIdentityBase(
                windowKey: overviewIdentityKey(window.window),
                measurementName: item.measurementName
            )
            let occurrence = occurrences[identity, default: 0]
            occurrences[identity] = occurrence + 1
            return IdentifiedSummaryRow(
                id: OverviewItem.ID(
                    windowKey: identity.windowKey,
                    measurementName: identity.measurementName,
                    occurrence: occurrence
                ),
                row: item.row,
                windowIndex: windowIndex
            )
        }
    }
}

public func catalogProgressIDs(for usage: SubscriptionUsage, accountID: String) -> Set<String> {
    Set(overviewItems(for: usage).compactMap { item in
        item.row.remainingRatio == nil ? nil : item.persistenceID(accountID: accountID)
    })
}

public func overviewProgressIsVisible(
    id: String,
    catalogDefault: Bool,
    hiddenIDs: Set<String>,
    shownIDs: Set<String>
) -> Bool {
    if hiddenIDs.contains(id) { return false }
    return catalogDefault || shownIDs.contains(id)
}

public func visibleOverviewItems(
    for usage: SubscriptionUsage,
    accountID: String,
    hiddenIDs: Set<String>,
    shownIDs: Set<String> = []
) -> [OverviewItem] {
    let catalog = overviewItems(for: usage)
    let catalogIDs = Set(catalog.map { $0.persistenceID(accountID: accountID) })
    var items = catalog.filter { item in
        let id = item.persistenceID(accountID: accountID)
        guard item.row.remainingRatio != nil else { return true }
        return overviewProgressIsVisible(
            id: id,
            catalogDefault: true,
            hiddenIDs: hiddenIDs,
            shownIDs: shownIDs
        )
    }
    for row in identifiedSummaryRows(for: usage) where row.row.remainingRatio != nil {
        let id = row.persistenceID(accountID: accountID)
        if catalogIDs.contains(id) { continue }
        if overviewProgressIsVisible(
            id: id,
            catalogDefault: false,
            hiddenIDs: hiddenIDs,
            shownIDs: shownIDs
        ) {
            items.append(OverviewItem(id: row.id, row: row.row, windowIndex: row.windowIndex))
        }
    }
    return items
}

private struct OverviewIdentityBase: Hashable {
    let windowKey: String
    let measurementName: String
}

private func overviewIdentityKey(_ window: UsageWindowKind) -> String {
    switch window {
    case .fiveHours: "five_hours"
    case .weekly: "weekly"
    case .monthly: "monthly"
    case .other(let id, _): "other:" + id
    case .unknown(let kind): "unknown:" + kind
    }
}

/// Provider-specific Overview catalog. Known providers list the windows and
/// measurements to show, in display order. Unknown providers keep the
/// shortest available time-tier fallback so a new account is still visible.
private struct OverviewSelector {
    enum Window {
        case fiveHours
        case weekly
        case monthly
        case otherContaining(String)
        case any
    }

    enum Measurement {
        case representative
        case named(String)
        case nameContains(String)
        case pool
    }

    let window: Window
    let measurement: Measurement
}

private struct OverviewSelection {
    let windowIndex: Int
    let projected: ProjectedRow?
    let usesRepresentative: Bool
}

private func overviewCatalog(for provider: String) -> [OverviewSelector]? {
    switch provider {
    case "chatgpt":
        [
            OverviewSelector(window: .fiveHours, measurement: .representative),
            OverviewSelector(window: .weekly, measurement: .representative),
            OverviewSelector(window: .otherContaining("reset credits"), measurement: .representative),
        ]
    case "claude":
        [
            OverviewSelector(window: .fiveHours, measurement: .representative),
            OverviewSelector(window: .otherContaining("fable"), measurement: .representative),
        ]
    case "cursor":
        [
            OverviewSelector(window: .any, measurement: .named("auto")),
            OverviewSelector(window: .any, measurement: .named("api")),
        ]
    case "grok":
        [
            OverviewSelector(window: .any, measurement: .pool),
            OverviewSelector(window: .any, measurement: .nameContains("grokbuild")),
        ]
    default:
        nil
    }
}

private func overviewSelections(
    windows: [[ProjectedRow]],
    usage: SubscriptionUsage
) -> [OverviewSelection] {
    if let catalog = overviewCatalog(for: usage.provider) {
        return catalog.flatMap { pick in
            usage.windows.enumerated().compactMap { index, window -> OverviewSelection? in
                guard overviewWindowMatches(window.window, pick.window) else { return nil }
                let rows = windows[index]
                switch pick.measurement {
                case .representative:
                    return OverviewSelection(
                        windowIndex: index,
                        projected: representativeRow(in: rows),
                        usesRepresentative: true
                    )
                case .named(let name):
                    guard let projected = rows.first(where: { $0.measurementName == name }) else {
                        return nil
                    }
                    return OverviewSelection(
                        windowIndex: index, projected: projected, usesRepresentative: false
                    )
                case .nameContains(let needle):
                    guard let projected = rows.first(where: {
                        overviewContains($0.measurementName, needle) || overviewContains($0.row.metric, needle)
                    }) else { return nil }
                    return OverviewSelection(
                        windowIndex: index, projected: projected, usesRepresentative: false
                    )
                case .pool:
                    guard let projected = rows.first(where: {
                        poolMeasurements.contains($0.measurementName) || $0.measurementName == "usage"
                    }) else { return nil }
                    return OverviewSelection(
                        windowIndex: index, projected: projected, usesRepresentative: false
                    )
                }
            }
        }
    }

    return overviewWindowIndexes(for: usage).map { index in
        OverviewSelection(
            windowIndex: index,
            projected: representativeRow(in: windows[index]),
            usesRepresentative: true
        )
    }
}

private func overviewWindowMatches(_ window: UsageWindowKind, _ selector: OverviewSelector.Window) -> Bool {
    switch selector {
    case .fiveHours:
        if case .fiveHours = window { return true }
        return false
    case .weekly:
        if case .weekly = window { return true }
        return false
    case .monthly:
        if case .monthly = window { return true }
        return false
    case .otherContaining(let needle):
        switch window {
        case .other(let id, let label):
            return overviewContains(id, needle) || overviewContains(label, needle)
        default:
            return false
        }
    case .any:
        return true
    }
}

private func overviewContains(_ haystack: String, _ needle: String) -> Bool {
    overviewNormalized(haystack).contains(overviewNormalized(needle))
}

private func overviewNormalized(_ value: String) -> String {
    value.lowercased().replacingOccurrences(of: "_", with: " ")
}

private func overviewWindowIndexes(for usage: SubscriptionUsage) -> [Int] {
    guard let shortestRank = usage.windows.lazy.map({ overviewRank($0.window) }).min() else { return [] }
    return usage.windows.enumerated().compactMap { index, window in
        overviewRank(window.window) == shortestRank ? index : nil
    }
}

private func overviewFallbackRow(for window: UsageWindow) -> SummaryRow? {
    let availability: Double?
    let allowed = booleanMeasurement(window, named: "allowed")
    let limitReached = booleanMeasurement(window, named: "limit_reached")
    if allowed != nil || limitReached != nil {
        availability = allowed == false || limitReached == true ? 0 : 100
    } else {
        availability = nil
    }
    if let availability {
        return SummaryRow(
            window: windowDisplayName(window.window),
            metric: "availability",
            value: .remains(availability),
            resetsAt: window.resetsAt,
            remainingRatio: availability / 100,
            disabled: false
        )
    }

    guard let measurement = window.measurements.first else { return nil }
    let value = measurementValue(
        measurement,
        unlimited: booleanMeasurement(window, named: "unlimited") == true
    )
    return SummaryRow(
        window: windowDisplayName(window.window),
        metric: metricDisplayName(measurement.name),
        value: value,
        resetsAt: window.resetsAt,
        remainingRatio: remainingRatio(value),
        disabled: false
    )
}

/// One enabled account that has a countable Overview remaining ratio for the menu bar.
public struct MenuBarAccountLevel: Equatable, Sendable {
    public let accountID: String
    public let displayName: String
    public let remainingRatio: Double

    public init(accountID: String, displayName: String, remainingRatio: Double) {
        self.accountID = accountID
        self.displayName = displayName
        self.remainingRatio = remainingRatio
    }
}

/// Enabled accounts with countable Overview rows, in stable account order.
/// `limitReached` (or an empty remaining) yields `0` and still participates.
public func menuBarAccountLevels(
    accounts: [Account],
    snapshots: [SnapshotPayload],
    hiddenOverviewItemIDs: Set<String> = [],
    shownOverviewItemIDs: Set<String> = []
) -> [MenuBarAccountLevel] {
    let titles = tabTitles(for: accounts)
    let snapshotsByID = Dictionary(uniqueKeysWithValues: snapshots.map { ($0.accountId, $0) })
    return sortedAccounts(accounts.filter(\.enabled)).compactMap { account in
        guard let snapshot = snapshotsByID[account.id],
              let usage = snapshot.usage.data,
              let ratio = menuBarFillRatio(
                for: usage,
                accountID: account.id,
                hiddenIDs: hiddenOverviewItemIDs,
                shownIDs: shownOverviewItemIDs
              ) else { return nil }
        return MenuBarAccountLevel(
            accountID: account.id,
            displayName: titles[account.id] ?? providerDisplayName(account.provider),
            remainingRatio: ratio
        )
    }
}

/// One Overview row a user can pin as the menu bar liquid level.
public struct MenuBarMetricOption: Equatable, Identifiable, Sendable {
    /// Stable across refreshes: account id, window key, measurement, occurrence.
    public let id: String
    public let accountID: String
    /// "Claude · 5h" or "Cursor · monthly auto"; the account part is the tab title.
    public let title: String
    public let remainingRatio: Double
    /// When this row's own window resets, for callers that display the pin.
    public let resetsAt: Date?

    public init(
        id: String,
        accountID: String,
        title: String,
        remainingRatio: Double,
        resetsAt: Date? = nil
    ) {
        self.id = id
        self.accountID = accountID
        self.title = title
        self.remainingRatio = remainingRatio
        self.resetsAt = resetsAt
    }
}

/// Every enabled account's Overview rows that carry a remaining ratio, in
/// stable account order. Disabled rows and money rows are not offered.
public func menuBarMetricOptions(
    accounts: [Account],
    snapshots: [SnapshotPayload],
    hiddenOverviewItemIDs: Set<String> = [],
    shownOverviewItemIDs: Set<String> = []
) -> [MenuBarMetricOption] {
    let titles = tabTitles(for: accounts)
    let snapshotsByID = Dictionary(uniqueKeysWithValues: snapshots.map { ($0.accountId, $0) })
    return sortedAccounts(accounts.filter(\.enabled)).flatMap { account -> [MenuBarMetricOption] in
        guard let snapshot = snapshotsByID[account.id],
              let usage = snapshot.usage.data else { return [] }
        let accountTitle = titles[account.id] ?? providerDisplayName(account.provider)
        return visibleOverviewItems(
            for: usage,
            accountID: account.id,
            hiddenIDs: hiddenOverviewItemIDs,
            shownIDs: shownOverviewItemIDs
        ).compactMap { item in
            guard !item.row.disabled, let ratio = item.row.remainingRatio else { return nil }
            let metric = item.row.metric == "usage" ? "" : " " + item.row.metric
            return MenuBarMetricOption(
                id: item.persistenceID(accountID: account.id),
                accountID: account.id,
                title: "\(accountTitle) · \(item.row.window)\(metric)",
                remainingRatio: ratio,
                resetsAt: item.row.resetsAt
            )
        }
    }
}

/// Levels that drive the menu bar liquid. A pinned metric that is currently
/// available becomes the single level; otherwise (no pin, or the pinned row
/// vanished) every account's representative level is returned so the floor
/// falls back to the lowest remaining.
public func menuBarLiquidLevels(
    accounts: [Account],
    snapshots: [SnapshotPayload],
    pinnedMetricID: String?,
    hiddenOverviewItemIDs: Set<String> = [],
    shownOverviewItemIDs: Set<String> = []
) -> [MenuBarAccountLevel] {
    if let pinnedMetricID,
       let option = menuBarMetricOptions(
            accounts: accounts,
            snapshots: snapshots,
            hiddenOverviewItemIDs: hiddenOverviewItemIDs,
            shownOverviewItemIDs: shownOverviewItemIDs
       ).first(where: { $0.id == pinnedMetricID }) {
        return [MenuBarAccountLevel(
            accountID: option.accountID,
            displayName: option.title,
            remainingRatio: option.remainingRatio
        )]
    }
    return menuBarAccountLevels(
        accounts: accounts,
        snapshots: snapshots,
        hiddenOverviewItemIDs: hiddenOverviewItemIDs,
        shownOverviewItemIDs: shownOverviewItemIDs
    )
}

public func menuBarFillRatio(
    accounts: [Account],
    snapshots: [SnapshotPayload],
    hiddenOverviewItemIDs: Set<String> = [],
    shownOverviewItemIDs: Set<String> = []
) -> Double? {
    let levels = menuBarAccountLevels(
        accounts: accounts,
        snapshots: snapshots,
        hiddenOverviewItemIDs: hiddenOverviewItemIDs,
        shownOverviewItemIDs: shownOverviewItemIDs
    )
    guard !levels.isEmpty else { return nil }
    return levels.map(\.remainingRatio).min()
}

public func quantizedMenuBarFillRatio(_ ratio: Double) -> Double {
    let clampedRatio = min(max(ratio.isFinite ? ratio : 0, 0), 1)
    return (clampedRatio * 20).rounded() / 20
}

private func menuBarFillRatio(
    for usage: SubscriptionUsage,
    accountID: String,
    hiddenIDs: Set<String>,
    shownIDs: Set<String>
) -> Double? {
    let items = visibleOverviewItems(
        for: usage,
        accountID: accountID,
        hiddenIDs: hiddenIDs,
        shownIDs: shownIDs
    )
    guard !items.isEmpty else { return nil }
    if items.contains(where: { windowHitItsLimit(usage.windows[$0.windowIndex]) }) {
        return 0
    }

    var minimumRatio: Double?
    for item in items {
        guard !item.row.disabled, let ratio = item.row.remainingRatio else { continue }
        minimumRatio = min(minimumRatio ?? ratio, ratio)
    }
    return minimumRatio
}

private func projectedWindows(for usage: SubscriptionUsage) -> [[ProjectedRow]] {
    let projected = usage.windows.map(summarizeWindow)
    let kindCounts = Dictionary(grouping: usage.windows.compactMap { windowKindKey($0.window) }, by: { $0 })
        .mapValues(\.count)

    return zip(usage.windows, projected).map { window, rows in
        guard let key = windowKindKey(window.window), kindCounts[key, default: 0] > 1,
              let representative = representativeRow(in: rows) else { return rows }
        let qualifiedName = windowDisplayName(window.window) + " · " + representative.row.metric
        return rows.map { projectedRow in
            ProjectedRow(
                measurementName: projectedRow.measurementName,
                row: SummaryRow(
                    window: qualifiedName,
                    metric: projectedRow.row.metric,
                    value: projectedRow.row.value,
                    resetsAt: projectedRow.row.resetsAt,
                    remainingRatio: projectedRow.row.remainingRatio,
                    disabled: projectedRow.row.disabled
                )
            )
        }
    }
}

private func representativeRow(in rows: [ProjectedRow]) -> ProjectedRow? {
    rows.first(where: { poolMeasurements.contains($0.measurementName) || $0.measurementName == "usage" })
        ?? rows.first(where: { $0.row.remainingRatio != nil })
        ?? rows.first
}

private func windowKindKey(_ window: UsageWindowKind) -> String? {
    switch window {
    case .fiveHours: "five_hours"
    case .weekly: "weekly"
    case .monthly: "monthly"
    case .unknown(let kind): "unknown:" + kind
    case .other: nil
    }
}

private struct ProjectedRow {
    let measurementName: String
    let row: SummaryRow
}

private func summarizeWindow(_ window: UsageWindow) -> [ProjectedRow] {
    let windowName = windowDisplayName(window.window)
    let unlimited = booleanMeasurement(window, named: "unlimited") == true
    let wholeWindowOff = booleanMeasurement(window, named: "enabled") == false
        && window.measurements.contains(where: { $0.name == "enabled" })
    let onDemandOff = booleanMeasurement(window, named: "on_demand_enabled") == false
        && window.measurements.contains(where: { $0.name == "on_demand_enabled" })

    func row(name: String, metric: String, value: SummaryValue, disabled: Bool) -> ProjectedRow {
        ProjectedRow(
            measurementName: name,
            row: SummaryRow(
                window: windowName,
                metric: metric,
                value: value,
                resetsAt: window.resetsAt,
                remainingRatio: remainingRatio(value),
                disabled: disabled
            )
        )
    }

    var rows = window.measurements
        .filter { !hiddenMeasurements.contains($0.name) }
        .map { measurement in
            row(
                name: measurement.name,
                metric: metricDisplayName(measurement.name),
                value: measurementValue(measurement, unlimited: unlimited),
                disabled: wholeWindowOff || (onDemandOff && measurement.name.hasPrefix("on_demand"))
            )
        }

    if unlimited && !rows.contains(where: { $0.row.value == .creditsUnlimited }) {
        rows.append(row(name: "credits", metric: "credits", value: .creditsUnlimited, disabled: wholeWindowOff))
    }
    if wholeWindowOff && !rows.contains(where: { $0.row.disabled }) {
        rows.append(row(name: "status", metric: "status", value: .disabled, disabled: false))
    }
    let hasOnDemandAmount = window.measurements.contains {
        $0.name.hasPrefix("on_demand") && !hiddenMeasurements.contains($0.name)
    }
    if onDemandOff && !hasOnDemandAmount {
        rows.append(row(name: "on_demand", metric: "on demand", value: .disabled, disabled: false))
    }
    return rows
}

private func windowHitItsLimit(_ window: UsageWindow) -> Bool {
    booleanMeasurement(window, named: "allowed") == false
        || booleanMeasurement(window, named: "limit_reached") == true
}

private func booleanMeasurement(_ window: UsageWindow, named name: String) -> Bool? {
    window.measurements.first(where: { $0.name == name }).map { $0.used != 0 }
}

private func measurementValue(_ measurement: UsageMeasurement, unlimited: Bool) -> SummaryValue {
    switch measurement.unit {
    case .percent:
        if measurement.limit == 100 {
            return .remains(min(max(100 - measurement.used, 0), 100))
        }
        return .used(max(measurement.used, 0))
    case .currency(let code):
        let currency = Currency(code: code)
        if let limit = measurement.limit {
            return .spent(amount: measurement.used, limit: limit, currency: currency)
        }
        return .balance(amount: measurement.used, currency: currency)
    case .credits where unlimited:
        return .creditsUnlimited
    case .credits:
        return .credits(used: measurement.used, limit: measurement.limit)
    default:
        return .counted(used: measurement.used, limit: measurement.limit)
    }
}

private func remainingRatio(_ value: SummaryValue) -> Double? {
    switch value {
    case .remains(let percent):
        return min(max(percent / 100, 0), 1)
    case .credits(let amount, .some(let limit)),
         .counted(let amount, .some(let limit)) where limit > 0:
        return min(max((limit - amount) / limit, 0), 1)
    default:
        return nil
    }
}

private func windowDisplayName(_ window: UsageWindowKind) -> String {
    return switch window {
    case .fiveHours: "5h"
    case .weekly: "weekly"
    case .monthly: "monthly"
    case .other(let id, let label): label.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? id : label
    case .unknown(let kind): kind
    }
}

private func overviewRank(_ window: UsageWindowKind) -> Int {
    return switch window {
    case .fiveHours: 0
    case .weekly: 1
    case .monthly: 2
    case .other, .unknown: 3
    }
}

private func metricDisplayName(_ originalName: String) -> String {
    if poolMeasurements.contains(originalName) { return "usage" }
    var name = originalName
    if name.hasPrefix("product:") { name.removeFirst("product:".count) }
    if name.hasSuffix("_usage") { name.removeLast("_usage".count) }
    if name.isEmpty { return "usage" }
    if let brand = brandNames[name] { return brand }
    return name.replacingOccurrences(of: "_", with: " ")
}
