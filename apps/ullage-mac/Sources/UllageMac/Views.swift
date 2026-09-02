import AppKit
import SwiftUI
import UllageKit

enum SelectedTab: Hashable {
    case overview
    case account(String)
}

struct RootView: View {
    @Bindable var store: UsageStore
    @Bindable var settings: AppSettings
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
                            OverviewView(store: store, settings: settings)
                        case .account(let id):
                            AccountView(store: store, settings: settings, accountID: id)
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
        case .deviceNotPaired:
            return AnyView(EmptyStateView(
                title: "Device not paired",
                detail: "Create a pair code with ullage device pair, then pair this Mac in Settings.",
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
                title: "Device token rejected",
                detail: "Create a fresh pair code and pair this Mac again in Settings.",
                actionTitle: "Open Settings",
                action: openSettings
            ))
        case .forbiddenHost:
            return AnyView(EmptyStateView(
                title: "Host rejected",
                detail: "Use an http:// loopback, tailnet, or private LAN address.",
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
            HStack(spacing: 0) {
                tab(title: "Overview", value: .overview, enabled: true)
                ForEach(accounts, id: \.id) { account in
                    tab(title: titles[account.id] ?? account.provider, value: .account(account.id), enabled: account.enabled)
                }
            }
            .padding(.horizontal, 5)
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
            .padding(.horizontal, 9)
            .frame(maxHeight: .infinity)
            .contentShape(Rectangle())
            .foregroundStyle(enabled ? .primary : .tertiary)
        }
        .buttonStyle(.plain)
        .focusEffectDisabled()
    }
}

private struct OverviewView: View {
    let store: UsageStore
    let settings: AppSettings

    var body: some View {
        let cards = overviewCards
        VStack(spacing: 14) {
            if cards.isEmpty, hasHiddenProgressRows {
                hiddenOverviewHint
            } else {
                ForEach(cards) { card in
                    UsageCardHeader(
                        account: card.account,
                        usage: card.usage,
                        snapshot: card.snapshot,
                        timestampLabel: "updated",
                        timestamp: card.snapshot.lastSuccessAt
                    )
                    ForEach(card.items) { item in
                        SummaryRowView(row: item.row)
                    }
                    Divider()
                }
            }
        }
    }

    private var overviewCards: [OverviewCard] {
        store.accounts.compactMap { account in
            guard let snapshot = store.snapshot(for: account.id),
                  let usage = snapshot.usage.data else { return nil }
            let items = visibleOverviewItems(
                for: usage,
                accountID: account.id,
                hiddenIDs: settings.hiddenOverviewItemIDs,
                shownIDs: settings.shownOverviewItemIDs
            )
            guard !items.isEmpty else { return nil }
            return OverviewCard(account: account, usage: usage, snapshot: snapshot, items: items)
        }
    }

    private var hasHiddenProgressRows: Bool {
        store.accounts.contains { account in
            guard let usage = store.snapshot(for: account.id)?.usage.data else { return false }
            return overviewItems(for: usage).contains { item in
                item.row.remainingRatio != nil
                    && settings.hiddenOverviewItemIDs.contains(item.persistenceID(accountID: account.id))
            }
        }
    }

    private var hiddenOverviewHint: some View {
        VStack(spacing: 8) {
            Image(systemName: "eye.slash")
                .font(.system(size: 22))
                .foregroundStyle(.secondary)
            Text("Overview is empty")
                .font(.headline)
            Text("Show a progress row again with the eye on its account tab.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 28)
        .accessibilityElement(children: .combine)
    }
}

private struct OverviewCard: Identifiable {
    var id: String { account.id }
    let account: Account
    let usage: SubscriptionUsage
    let snapshot: SnapshotPayload
    let items: [OverviewItem]
}

private struct AccountView: View {
    let store: UsageStore
    let settings: AppSettings
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
                let identified = identifiedSummaryRows(for: usage)
                let catalogIDs = catalogProgressIDs(for: usage, accountID: accountID)
                let grouped = groupedIdentifiedRows(identified)
                ForEach(Array(grouped.enumerated()), id: \.element.0) { _, group in
                    let window = group.0
                    let windowRows = group.1
                    VStack(alignment: .leading, spacing: 7) {
                        HStack {
                            Text(window).font(.headline)
                            Spacer()
                            Text(resetText(windowRows.first?.row.resetsAt)).font(.caption).foregroundStyle(.secondary)
                        }
                        ForEach(Array(windowRows.enumerated()), id: \.offset) { _, item in
                            SummaryRowView(
                                row: item.row,
                                showWindow: false,
                                overviewToggle: overviewToggle(for: item, catalogIDs: catalogIDs)
                            )
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

    private func groupedIdentifiedRows(
        _ rows: [IdentifiedSummaryRow]
    ) -> [(String, [IdentifiedSummaryRow])] {
        var order: [String] = []
        var groups: [String: [IdentifiedSummaryRow]] = [:]
        for row in rows {
            if groups[row.row.window] == nil { order.append(row.row.window) }
            groups[row.row.window, default: []].append(row)
        }
        return order.map { ($0, groups[$0] ?? []) }
    }

    private func overviewToggle(
        for item: IdentifiedSummaryRow,
        catalogIDs: Set<String>
    ) -> OverviewRowToggle? {
        guard item.row.remainingRatio != nil else { return nil }
        let id = item.persistenceID(accountID: accountID)
        let catalogDefault = catalogIDs.contains(id)
        return OverviewRowToggle(
            visible: overviewProgressIsVisible(
                id: id,
                catalogDefault: catalogDefault,
                hiddenIDs: settings.hiddenOverviewItemIDs,
                shownIDs: settings.shownOverviewItemIDs
            ),
            setVisible: { settings.setOverviewItemVisible(id, visible: $0, catalogDefault: catalogDefault) }
        )
    }
}

private struct OverviewRowToggle {
    let visible: Bool
    let setVisible: (Bool) -> Void
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
        badgeNames(account: account, snapshot: snapshot, summary: summary)
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
    var overviewToggle: OverviewRowToggle?

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack {
                HStack(spacing: 4) {
                    Text(showWindow ? row.window : row.metric)
                        .font(.system(size: 12, weight: .medium))
                        .lineLimit(1)
                        .truncationMode(.tail)
                    if showWindow {
                        Text(row.metric)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.tail)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                if let overviewToggle {
                    Button {
                        overviewToggle.setVisible(!overviewToggle.visible)
                    } label: {
                        Image(systemName: overviewToggle.visible ? "eye" : "eye.slash")
                            .font(.system(size: 13, weight: .medium))
                            .foregroundStyle(overviewToggle.visible ? .secondary : .tertiary)
                            .frame(width: 28, height: 28)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help(overviewToggle.visible ? "Hide from Overview" : "Show in Overview")
                    .accessibilityLabel(overviewToggle.visible ? "Hide from Overview" : "Show in Overview")
                    .focusEffectDisabled()
                }
                Text(summaryValueText(row.value) + (row.disabled ? " (off)" : ""))
                    .font(.system(size: 12, design: .monospaced))
                    .fixedSize(horizontal: true, vertical: false)
                if showWindow {
                    Text(resetText(row.resetsAt))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: true, vertical: false)
                }
            }
            if let ratio = row.remainingRatio {
                SegmentedProgress(ratio: ratio)
            }
        }
    }
}

struct SegmentedProgress: View {
    let ratio: Double

    var body: some View {
        HStack(spacing: 2) {
            ForEach(0..<10, id: \.self) { index in
                ProgressSegment(
                    fill: progressSegmentFill(ratio: ratio, index: index),
                    color: color
                )
            }
        }
    }

    var color: Color {
        Color(nsColor: progressColor(for: RemainingTier(ratio: ratio)))
    }
}

/// Fraction of a 10% cell that should be filled, counting remaining quota from
/// the right so a 5% remainder paints half of the last cell.
func progressSegmentFill(ratio: Double, index: Int, segmentCount: Int = 10) -> Double {
    guard segmentCount > 0, (0..<segmentCount).contains(index) else { return 0 }
    let units = min(max(ratio.isFinite ? ratio : 0, 0), 1) * Double(segmentCount)
    let filledStart = Double(segmentCount) - units
    return min(max(Double(index + 1) - max(Double(index), filledStart), 0), 1)
}

private struct ProgressSegment: View {
    let fill: Double
    let color: Color

    var body: some View {
        GeometryReader { geo in
            Capsule()
                .fill(Color(nsColor: .separatorColor))
                .overlay(alignment: .trailing) {
                    Rectangle()
                        .fill(color)
                        .frame(width: geo.size.width * min(max(fill, 0), 1))
                }
                .clipShape(Capsule())
        }
        .frame(height: 5)
    }
}

@MainActor
func progressColor(for tier: RemainingTier) -> NSColor {
    switch tier {
    case .healthy: .systemGreen
    case .caution: .systemYellow
    case .low: .systemOrange
    case .critical: .systemRed
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
    sanitizedErrorKind(error) ?? "none"
}
