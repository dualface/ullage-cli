import AppKit
import SwiftUI
import UllageKit

enum SelectedTab: Hashable {
    case overview
    case account(String)
}

struct RootView: View {
    @Bindable var store: UsageStore
    let openSettings: () -> Void
    let onPreferredHeightChanged: (CGFloat) -> Void
    @State private var selectedTab: SelectedTab = .overview

    var body: some View {
        VStack(spacing: 0) {
            TabBar(accounts: store.accounts, selected: $selectedTab)
            Divider()
            ScrollView {
                Group {
                    if let emptyState = emptyState {
                        emptyState
                    } else {
                        switch selectedTab {
                        case .overview:
                            OverviewView(store: store)
                        case .account(let id):
                            AccountView(store: store, accountID: id)
                        }
                    }
                }
                .padding(14)
                .background {
                    GeometryReader { proxy in
                        Color.clear.preference(key: ContentHeightPreferenceKey.self, value: proxy.size.height)
                    }
                }
            }
        }
        .frame(width: 360)
        .background(.regularMaterial)
        .onPreferenceChange(ContentHeightPreferenceKey.self) { contentHeight in
            onPreferredHeightChanged(45 + contentHeight)
        }
        .onChange(of: store.accounts.map(\.id)) { _, accountIDs in
            selectedTab = normalizedSelection(selectedTab, accountIDs: accountIDs)
        }
    }

    private var emptyState: AnyView? {
        switch store.connectionState {
        case .idle where store.snapshots.isEmpty:
            return AnyView(EmptyStateView(title: "Loading usage…", detail: nil, actionTitle: nil, action: nil))
        case .loading where store.snapshots.isEmpty:
            return AnyView(EmptyStateView(title: "Loading usage…", detail: nil, actionTitle: nil, action: nil))
        case .tokenNotConfigured:
            return AnyView(EmptyStateView(
                title: "Token not configured",
                detail: "Paste the output of ullage http token in Settings.",
                actionTitle: "Open Settings",
                action: openSettings
            ))
        case .unreachable:
            return AnyView(EmptyStateView(
                title: "Daemon unreachable",
                detail: "Run ullage daemon start and set http.enabled = true.",
                actionTitle: "Retry",
                action: store.refresh
            ))
        case .unauthorized:
            return AnyView(EmptyStateView(
                title: "Token rejected",
                detail: "Open Settings and paste a fresh ullage http token.",
                actionTitle: "Open Settings",
                action: openSettings
            ))
        case .forbiddenHost:
            return AnyView(EmptyStateView(
                title: "Host rejected",
                detail: "Only an http:// loopback server is accepted.",
                actionTitle: "Open Settings",
                action: openSettings
            ))
        case .noAccounts:
            return AnyView(EmptyStateView(
                title: "No accounts",
                detail: "Run ullage auth login to add an account.",
                actionTitle: "Retry",
                action: store.refresh
            ))
        case .protocolMismatch(let client, let server):
            return AnyView(EmptyStateView(
                title: "Protocol version mismatch",
                detail: "App protocol \(client), daemon protocol \(server).",
                actionTitle: nil,
                action: nil
            ))
        default:
            return nil
        }
    }
}

func normalizedSelection(_ selection: SelectedTab, accountIDs: [String]) -> SelectedTab {
    guard case .account(let selectedID) = selection, !accountIDs.contains(selectedID) else {
        return selection
    }
    return .overview
}

private struct ContentHeightPreferenceKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

private struct TabBar: View {
    let accounts: [Account]
    @Binding var selected: SelectedTab

    private var titles: [String: String] { tabTitles(for: accounts) }

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 18) {
                tab(title: "Overview", value: .overview, enabled: true)
                ForEach(accounts, id: \.id) { account in
                    tab(title: titles[account.id] ?? account.provider, value: .account(account.id), enabled: account.enabled)
                }
            }
            .padding(.horizontal, 14)
        }
        .frame(height: 44)
    }

    private func tab(title: String, value: SelectedTab, enabled: Bool) -> some View {
        Button {
            selected = value
        } label: {
            VStack(spacing: 5) {
                Text(title).font(.system(size: 12, weight: selected == value ? .semibold : .regular))
                Capsule()
                    .fill(selected == value ? Color.accentColor : Color.clear)
                    .frame(height: 2)
            }
            .foregroundStyle(enabled ? .primary : .tertiary)
        }
        .buttonStyle(.plain)
        .focusEffectDisabled()
    }
}

private struct OverviewView: View {
    let store: UsageStore

    var body: some View {
        LazyVStack(spacing: 14) {
            ForEach(store.accounts, id: \.id) { account in
                if let snapshot = store.snapshot(for: account.id), let usage = snapshot.usage.data {
                    UsageCardHeader(
                        account: account,
                        usage: usage,
                        snapshot: snapshot,
                        timestampLabel: "updated",
                        timestamp: snapshot.lastSuccessAt
                    )
                    ForEach(Array(overviewRows(for: usage).enumerated()), id: \.offset) { _, row in
                        SummaryRowView(row: row)
                    }
                    Divider()
                }
            }
        }
    }
}

private struct AccountView: View {
    let store: UsageStore
    let accountID: String

    private var account: Account? { store.accounts.first(where: { $0.id == accountID }) }
    private var snapshot: SnapshotPayload? { store.snapshot(for: accountID) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let account, let snapshot, let usage = snapshot.usage.data {
                UsageCardHeader(
                    account: account,
                    usage: usage,
                    snapshot: snapshot,
                    timestampLabel: "observed",
                    timestamp: usage.observedAt
                )
                let rows = summarize(usage).rows
                ForEach(groupedRows(rows), id: \.0) { window, windowRows in
                    VStack(alignment: .leading, spacing: 7) {
                        HStack {
                            Text(window).font(.headline)
                            Spacer()
                            Text(resetText(windowRows.first?.resetsAt)).font(.caption).foregroundStyle(.secondary)
                        }
                        ForEach(Array(windowRows.enumerated()), id: \.offset) { _, row in
                            SummaryRowView(row: row, showWindow: false)
                        }
                    }
                }
                HStack {
                    Button {
                        store.probe(accountID: accountID)
                    } label: {
                        if store.probingAccountIDs.contains(accountID) {
                            ProgressView().controlSize(.small)
                        } else if let seconds = store.rateLimitSeconds(accountID) {
                            Text("Retry in \(seconds)s")
                        } else {
                            Text("Probe now")
                        }
                    }
                    .disabled(store.probingAccountIDs.contains(accountID) || store.isRateLimited(accountID))
                    .focusEffectDisabled()
                    Spacer()
                    Text("last error: \(errorKind(snapshot.lastError))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    private func groupedRows(_ rows: [SummaryRow]) -> [(String, [SummaryRow])] {
        var order: [String] = []
        var groups: [String: [SummaryRow]] = [:]
        for row in rows {
            if groups[row.window] == nil { order.append(row.window) }
            groups[row.window, default: []].append(row)
        }
        return order.map { ($0, groups[$0] ?? []) }
    }
}

private struct UsageCardHeader: View {
    let account: Account
    let usage: SubscriptionUsage
    let snapshot: SnapshotPayload
    let timestampLabel: String
    let timestamp: Date

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack {
                Text(providerDisplayName(account.provider)).font(.headline)
                if let label = account.label { Text(label).foregroundStyle(.secondary) }
                Spacer()
                BadgeList(account: account, snapshot: snapshot, summary: summarize(usage))
            }
            HStack(spacing: 8) {
                if let plan = usage.plan { Text(plan) }
                Text("\(timestampLabel) \(relativeTime(timestamp))")
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
    }
}

private struct BadgeList: View {
    let account: Account
    let snapshot: SnapshotPayload
    let summary: UsageSummary

    var body: some View {
        HStack(spacing: 4) {
            ForEach(badges, id: \.self) { Badge(text: $0) }
        }
    }

    private var badges: [String] {
        var values: [String] = []
        if snapshot.stale { values.append("stale") }
        if case .partial = snapshot.usage { values.append("partial") }
        if summary.limitReached { values.append("limit reached") }
        if !account.enabled { values.append("disabled") }
        if case .authenticationInvalid? = snapshot.lastError { values.append("auth invalid") }
        if let error = snapshot.lastError { values.append(errorKind(error)) }
        return values.reduce(into: []) { result, value in
            if !result.contains(value) { result.append(value) }
        }
    }
}

private struct Badge: View {
    let text: String
    var body: some View {
        Text(text)
            .font(.system(size: 9, weight: .medium))
            .padding(.horizontal, 5)
            .padding(.vertical, 2)
            .background(.thinMaterial, in: Capsule())
            .overlay {
                Capsule().stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
            }
    }
}

private struct SummaryRowView: View {
    let row: SummaryRow
    var showWindow = true

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack {
                Text(showWindow ? row.window : row.metric)
                    .font(.system(size: 12, weight: .medium))
                if showWindow { Text(row.metric).font(.caption).foregroundStyle(.secondary) }
                Spacer()
                Text(summaryValueText(row.value) + (row.disabled ? " (off)" : ""))
                    .font(.system(size: 12, design: .monospaced))
                if showWindow { Text(resetText(row.resetsAt)).font(.caption).foregroundStyle(.secondary) }
            }
            if let ratio = row.remainingRatio {
                SegmentedProgress(ratio: ratio)
            }
        }
    }
}

private struct SegmentedProgress: View {
    let ratio: Double

    var body: some View {
        HStack(spacing: 2) {
            ForEach(0..<10, id: \.self) { index in
                Capsule()
                    .fill(index >= 10 - filledSegments ? color : Color(nsColor: .separatorColor))
                    .frame(height: 5)
            }
        }
    }

    private var filledSegments: Int { min(10, max(0, Int(ceil(ratio * 10)))) }
    private var color: Color {
        switch RemainingTier(ratio: ratio) {
        case .healthy: .green
        case .caution: .yellow
        case .low: .orange
        case .critical: .red
        }
    }
}

struct EmptyStateView: View {
    let title: String
    let detail: String?
    let actionTitle: String?
    let action: (() -> Void)?

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "gauge.with.dots.needle.33percent")
                .font(.system(size: 28))
                .foregroundStyle(.secondary)
            Text(title).font(.headline)
            if let detail { Text(detail).multilineTextAlignment(.center).foregroundStyle(.secondary) }
            if let actionTitle, let action {
                Button(actionTitle, action: action)
                    .focusEffectDisabled()
            }
        }
        .frame(maxWidth: .infinity, minHeight: 180)
    }
}

private func relativeTime(_ date: Date, now: Date = Date()) -> String {
    let seconds = Int(date.timeIntervalSince(now))
    let absolute = abs(seconds)
    let text: String
    if absolute < 60 { text = "\(absolute)s" }
    else if absolute < 3_600 { text = "\(absolute / 60)m" }
    else if absolute < 86_400 { text = "\(absolute / 3_600)h" }
    else { text = "\(absolute / 86_400)d" }
    return seconds >= 0 ? "in \(text)" : "\(text) ago"
}

private func resetText(_ date: Date?) -> String {
    date.map { "resets \(relativeTime($0))" } ?? ""
}

private func errorKind(_ error: SanitizedErrorPayload?) -> String {
    guard let error else { return "none" }
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
