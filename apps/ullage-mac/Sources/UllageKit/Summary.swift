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
    let windows = projectedWindows(for: usage)
    return usage.windows.enumerated()
        .compactMap { index, window -> (Int, Int, SummaryRow)? in
            let rank = overviewRank(window.window)
            let rows = windows[index]
            guard let selected = rows.first(where: { poolMeasurements.contains($0.measurementName) || $0.measurementName == "usage" })
                ?? rows.first(where: { $0.row.remainingRatio != nil })
                ?? rows.first else {
                return overviewFallbackRow(for: window).map { (rank, index, $0) }
            }
            return (rank, index, selected.row)
        }
        .sorted { lhs, rhs in
            lhs.0 == rhs.0 ? lhs.1 < rhs.1 : lhs.0 < rhs.0
        }
        .map(\.2)
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
    case .spent(let amount, let limit, _),
         .credits(let amount, .some(let limit)),
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
