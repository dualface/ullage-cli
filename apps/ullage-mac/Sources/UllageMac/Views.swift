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
    let presentation: PopoverPresentation
    let openSettings: () -> Void
    let onPreferredHeightChanged: (CGFloat) -> Void
    @State private var selectedTab: SelectedTab = .overview
    @State private var chromeHeight: CGFloat = 0
    @State private var contentHeight: CGFloat = 0

    var body: some View {
        // The header floats and the cards scroll underneath it, so the glass
        // chrome has moving content to refract as well as the backdrop.
        ZStack(alignment: .top) {
            ScrollView {
                VStack(spacing: 0) {
                    Color.clear.frame(height: chromeHeight)
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
                    .padding(.horizontal, 12)
                    .padding(.bottom, 14)
                    .measuredHeight(ContentHeightPreferenceKey.self)
                }
            }
            VStack(spacing: 0) {
                if let hero {
                    PopoverHero(
                        model: hero,
                        isRefreshing: store.connectionState == .loading,
                        isPresented: presentation.isShown,
                        settings: settings,
                        refresh: store.refresh,
                        openSettings: openSettings
                    )
                    .padding(.horizontal, 12)
                    .padding(.top, 12)
                }
                TabBar(store: store, settings: settings, selected: $selectedTab)
            }
            .measuredHeight(ChromeHeightPreferenceKey.self)
        }
        .frame(width: 360)
        .modifier(PopoverSurface(pointerOffset: presentation.pointerOffset))
        .onPreferenceChange(ChromeHeightPreferenceKey.self) { height in
            chromeHeight = height
            reportHeight()
        }
        .onPreferenceChange(ContentHeightPreferenceKey.self) { height in
            contentHeight = height
            reportHeight()
        }
        .onChange(of: store.accounts.map(\.id)) { _, accountIDs in
            selectedTab = normalizedSelection(selectedTab, accountIDs: accountIDs)
        }
    }

    private func reportHeight() {
        // Whole points only: fractional measurements made the popover resize by
        // a pixel on changes that did not really alter the layout.
        onPreferredHeightChanged((chromeHeight + contentHeight + Self.pointerInset).rounded(.up))
    }

    /// The pointer is part of the panel on macOS 26, so the panel has to be
    /// that much taller than its content. `NSPopover` draws its arrow outside
    /// the content size, so nothing is added below macOS 26.
    static var pointerInset: CGFloat {
        if #available(macOS 26.0, *) { return PopoverBubble.pointerHeight }
        return 0
    }

    /// The hero mirrors the menu bar mark, so it is hidden whenever the mark
    /// itself has nothing to show.
    private var hero: HeroModel? {
        guard emptyState == nil else { return nil }
        return heroModel(
            accounts: store.accounts,
            snapshots: store.snapshots,
            pinnedMetricID: settings.menuBarMetricID,
            hiddenOverviewItemIDs: settings.hiddenOverviewItemIDs,
            shownOverviewItemIDs: settings.shownOverviewItemIDs
        )
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

private struct ChromeHeightPreferenceKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

private extension View {
    func measuredHeight<Key: PreferenceKey>(_ key: Key.Type) -> some View where Key.Value == CGFloat {
        background {
            GeometryReader { proxy in
                Color.clear.preference(key: key, value: proxy.size.height)
            }
        }
    }
}

/// Row assignment for a flow layout: greedy left to right, wrapping when the
/// next item would cross `maxWidth`. An item wider than the row gets its own.
func flowRows(widths: [CGFloat], maxWidth: CGFloat, spacing: CGFloat) -> [[Int]] {
    var rows: [[Int]] = []
    var row: [Int] = []
    var used: CGFloat = 0
    for (index, width) in widths.enumerated() {
        let needed = row.isEmpty ? width : used + spacing + width
        if !row.isEmpty, needed > maxWidth {
            rows.append(row)
            row = [index]
            used = width
        } else {
            row.append(index)
            used = needed
        }
    }
    if !row.isEmpty { rows.append(row) }
    return rows
}

/// Lays subviews out in wrapped rows at their ideal widths.
struct FlowLayout: Layout {
    var spacing: CGFloat = 4
    var lineSpacing: CGFloat = 4

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Void) -> CGSize {
        let sizes: [CGSize] = subviews.map { $0.sizeThatFits(.unspecified) }
        var total: CGFloat = 0
        for size in sizes { total += size.width }
        let maxWidth: CGFloat = proposal.width ?? total
        let widths: [CGFloat] = sizes.map(\.width)
        let rows: [[Int]] = flowRows(widths: widths, maxWidth: maxWidth, spacing: spacing)

        var width: CGFloat = 0
        var height: CGFloat = 0
        for (index, row) in rows.enumerated() {
            var rowWidth: CGFloat = 0
            var rowHeight: CGFloat = 0
            for item in row {
                rowWidth += sizes[item].width
                rowHeight = max(rowHeight, sizes[item].height)
            }
            rowWidth += spacing * CGFloat(max(row.count - 1, 0))
            width = max(width, rowWidth)
            height += rowHeight
            if index > 0 { height += lineSpacing }
        }
        return CGSize(width: min(width, maxWidth), height: height)
    }

    func placeSubviews(
        in bounds: CGRect,
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout Void
    ) {
        let sizes: [CGSize] = subviews.map { $0.sizeThatFits(.unspecified) }
        let widths: [CGFloat] = sizes.map(\.width)
        let rows: [[Int]] = flowRows(widths: widths, maxWidth: bounds.width, spacing: spacing)
        var y: CGFloat = bounds.minY
        for row in rows {
            var x: CGFloat = bounds.minX
            var rowHeight: CGFloat = 0
            for item in row { rowHeight = max(rowHeight, sizes[item].height) }
            for index in row {
                subviews[index].place(
                    at: CGPoint(x: x, y: y + (rowHeight - sizes[index].height) / 2),
                    proposal: ProposedViewSize(sizes[index])
                )
                x += sizes[index].width + spacing
            }
            y += rowHeight + lineSpacing
        }
    }
}

private struct TabBar: View {
    let store: UsageStore
    let settings: AppSettings
    @Binding var selected: SelectedTab
    @Environment(\.colorScheme) private var colorScheme
    @Namespace private var highlight

    private var titles: [String: String] { tabTitles(for: store.accounts) }

    var body: some View {
        // Wrapped rows rather than a horizontal scroller: scrolling one needs a
        // sideways gesture many mice cannot make, which left later accounts
        // unreachable. Wrapping keeps every account clickable at any count.
        FlowLayout(spacing: 4, lineSpacing: 4) {
            tab(title: "Overview", value: .overview, enabled: true, warning: nil)
            ForEach(store.accounts, id: \.id) { account in
                tab(
                    title: titles[account.id] ?? account.provider,
                    value: .account(account.id),
                    enabled: account.enabled,
                    warning: warningTier(for: account)
                )
            }
        }
        .padding(6)
        // FlowLayout sizes itself to its widest row, so without this the
        // panel would shrink to the tabs instead of matching the header.
        .frame(maxWidth: .infinity, alignment: .leading)
        .glassPanel(cornerRadius: 20)
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    @ViewBuilder
    private var selectionBackground: some View {
        if #available(macOS 26.0, *) {
            Capsule().fill(.clear).glassEffect(.clear.interactive(), in: .capsule)
        } else {
            Capsule()
                .fill(colorScheme == .dark ? Color.white.opacity(0.16) : Color.white.opacity(0.95))
                .shadow(color: .black.opacity(colorScheme == .dark ? 0.25 : 0.08), radius: 3, y: 1)
        }
    }

    /// A dot on the tab when that account has a row in its last two tiers, so
    /// the account worth opening is visible without switching tabs.
    private func warningTier(for account: Account) -> RemainingTier? {
        guard let usage = store.snapshot(for: account.id)?.usage.data else { return nil }
        let ratios = visibleOverviewItems(
            for: usage,
            accountID: account.id,
            hiddenIDs: settings.hiddenOverviewItemIDs,
            shownIDs: settings.shownOverviewItemIDs
        ).compactMap(\.row.remainingRatio)
        guard let lowest = ratios.min() else { return nil }
        let tier = RemainingTier(ratio: lowest)
        return tier == .low || tier == .critical ? tier : nil
    }

    private func tab(
        title: String,
        value: SelectedTab,
        enabled: Bool,
        warning: RemainingTier?
    ) -> some View {
        let isSelected = selected == value
        return Button {
            selected = value
        } label: {
            HStack(spacing: 5) {
                if let warning {
                    let color = Color(nsColor: progressColor(for: warning))
                    Circle()
                        .fill(color)
                        .frame(width: 6, height: 6)
                        .shadow(color: color.opacity(0.8), radius: 4)
                }
                // Reserve the selected weight's width in both states: the
                // semibold label is up to 2.4 pt wider, and letting the pill
                // resize on selection moved the wrap point, which shifted the
                // popover vertically on every tab switch.
                Text(title)
                    .font(.system(size: 12, weight: .semibold))
                    .lineLimit(1)
                    .opacity(0)
                    .overlay {
                        Text(title)
                            .font(.system(size: 12, weight: isSelected ? .semibold : .regular))
                            .lineLimit(1)
                            .fixedSize()
                            // A font weight is not something SwiftUI can
                            // interpolate, so inside the pill's animation it
                            // cross-fades the two renderings of the label and
                            // the text reads as flickering while it redraws.
                            // The weight changes at once; only the pill moves.
                            .transaction { $0.animation = nil }
                    }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .background {
                if isSelected {
                    selectionBackground
                        .matchedGeometryEffect(id: "tab", in: highlight)
                }
            }
            .contentShape(Capsule())
            .foregroundStyle(enabled ? .primary : .tertiary)
        }
        .buttonStyle(.plain)
        .focusEffectDisabled()
        .animation(.snappy(duration: 0.25), value: selected)
    }
}

private struct OverviewView: View {
    let store: UsageStore
    let settings: AppSettings

    var body: some View {
        let cards = overviewCards
        VStack(spacing: 10) {
            if cards.isEmpty, hasHiddenProgressRows {
                hiddenOverviewHint
            } else {
                ForEach(cards) { card in
                    VStack(spacing: 12) {
                        UsageCardHeader(
                            account: card.account,
                            accounts: store.accounts,
                            usage: card.usage,
                            snapshot: card.snapshot,
                            timestampLabel: "updated",
                            timestamp: card.snapshot.lastSuccessAt
                        )
                        ForEach(card.items) { item in
                            SummaryRowView(row: item.row)
                        }
                    }
                    .padding(.horizontal, 14)
                    .padding(.vertical, 12)
                    .glassPanel()
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
        .glassPanel()
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
        VStack(alignment: .leading, spacing: 10) {
            if let account, let snapshot, let usage = snapshot.usage.data {
                UsageCardHeader(
                    account: account,
                    accounts: store.accounts,
                    usage: usage,
                    snapshot: snapshot,
                    timestampLabel: "observed",
                    timestamp: usage.observedAt
                )
                .padding(.horizontal, 2)
                let identified = identifiedSummaryRows(for: usage)
                let catalogIDs = catalogProgressIDs(for: usage, accountID: accountID)
                let grouped = groupedIdentifiedRows(identified)
                ForEach(Array(grouped.enumerated()), id: \.element.0) { _, group in
                    let window = group.0
                    let windowRows = group.1
                    VStack(alignment: .leading, spacing: 10) {
                        HStack {
                            Text(window).font(.system(size: 13, weight: .bold))
                            Spacer()
                            Text(resetText(windowRows.first?.row.resetsAt))
                                .font(.caption)
                                .monospacedDigit()
                                .foregroundStyle(.secondary)
                        }
                        ForEach(Array(windowRows.enumerated()), id: \.offset) { _, item in
                            SummaryRowView(
                                row: item.row,
                                showWindow: false,
                                overviewToggle: overviewToggle(for: item, catalogIDs: catalogIDs)
                            )
                        }
                    }
                    .padding(.horizontal, 14)
                    .padding(.vertical, 12)
                    .glassPanel()
                }
                HStack {
                    ProbeButton(
                        isProbing: store.probingAccountIDs.contains(accountID),
                        retrySeconds: store.rateLimitSeconds(accountID),
                        action: { store.probe(accountID: accountID) }
                    )
                    .disabled(store.probingAccountIDs.contains(accountID) || store.isRateLimited(accountID))
                    Spacer()
                    Text("last error: \(errorKind(snapshot.lastError))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 2)
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

private struct ProbeButton: View {
    let isProbing: Bool
    let retrySeconds: Int?
    let action: () -> Void
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                if isProbing {
                    ProgressView().controlSize(.small)
                } else {
                    Image(systemName: "arrow.trianglehead.2.clockwise")
                        .font(.system(size: 11, weight: .semibold))
                }
                if let retrySeconds {
                    Text("Retry in \(retrySeconds)s").monospacedDigit()
                } else if !isProbing {
                    Text("Probe now")
                }
            }
            .font(.system(size: 12, weight: .semibold))
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .modifier(ProbeBackground(accent: Self.accent, isEnabled: isEnabled))
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .focusEffectDisabled()
    }

    private static let accent = Color(red: 0.85, green: 0.27, blue: 0.37)
}

private struct ProbeBackground: ViewModifier {
    let accent: Color
    let isEnabled: Bool

    @ViewBuilder
    func body(content: Content) -> some View {
        if #available(macOS 26.0, *) {
            content.glassEffect(
                .regular.tint(accent.opacity(isEnabled ? 0.5 : 0.15)).interactive(isEnabled),
                in: .capsule
            )
        } else {
            content
                .background { Capsule().fill(accent.opacity(isEnabled ? 0.18 : 0.08)) }
                .overlay {
                    Capsule().strokeBorder(accent.opacity(isEnabled ? 0.35 : 0.15), lineWidth: 1)
                }
                .shadow(color: accent.opacity(isEnabled ? 0.35 : 0), radius: 8)
        }
    }
}

private struct OverviewRowToggle {
    let visible: Bool
    let setVisible: (Bool) -> Void
}

/// An account's label only disambiguates when its provider has more than one
/// account, matching how tab titles are built.
func disambiguatingLabel(for account: Account, in accounts: [Account]) -> String? {
    guard accounts.filter({ $0.provider == account.provider }).count > 1 else { return nil }
    let trimmed = account.label?.trimmingCharacters(in: .whitespacesAndNewlines)
    guard let trimmed, !trimmed.isEmpty else { return nil }
    return trimmed
}

private struct UsageCardHeader: View {
    let account: Account
    let accounts: [Account]
    let usage: SubscriptionUsage
    let snapshot: SnapshotPayload
    let timestampLabel: String
    let timestamp: Date

    var body: some View {
        // Badges get their own row: at 360 pt they squeeze into slivers when
        // they share a line with the name, plan and timestamp.
        let badges = badgeNames(account: account, snapshot: snapshot, summary: summarize(usage))
        return VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                ProviderBadge(provider: account.provider)
                Text(providerDisplayName(account.provider))
                    .font(.system(size: 13, weight: .semibold))
                    .lineLimit(1)
                    .fixedSize()
                if let label = disambiguatingLabel(for: account, in: accounts) {
                    Text(label)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                if let plan = usage.plan {
                    PlanChip(text: plan)
                }
                Spacer(minLength: 4)
                Text("\(timestampLabel) \(relativeTimeText(timestamp))")
                    .font(.system(size: 11))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                    .fixedSize()
            }
            if !badges.isEmpty {
                HStack(spacing: 4) {
                    ForEach(badges, id: \.self) { Badge(text: $0) }
                    Spacer(minLength: 0)
                }
            }
        }
    }
}

private struct PlanChip: View {
    let text: String

    var body: some View {
        // Providers spell plans differently ("Pro", "max_5x", "Ultra"); the
        // chip normalizes them so a row of accounts reads as one vocabulary.
        Text(text.lowercased())
            .font(.system(size: 10, weight: .semibold))
            .kerning(0.4)
            .foregroundStyle(Self.accent)
            .padding(.horizontal, 7)
            .padding(.vertical, 2)
            .background { Capsule().fill(Self.accent.opacity(0.16)) }
            .fixedSize()
    }

    private static let accent = Color(red: 0.94, green: 0.42, blue: 0.51)
}

private struct Badge: View {
    let text: String
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        Text(text)
            .font(.system(size: 9, weight: .medium))
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background {
                Capsule().fill(colorScheme == .dark ? Color.white.opacity(0.10) : Color.white.opacity(0.75))
            }
            .overlay {
                Capsule().strokeBorder(
                    colorScheme == .dark ? Color.white.opacity(0.14) : Color.black.opacity(0.08),
                    lineWidth: 1
                )
            }
            .fixedSize()
    }
}

private struct SummaryRowView: View {
    let row: SummaryRow
    var showWindow = true
    var overviewToggle: OverviewRowToggle?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                HStack(spacing: 4) {
                    Text(showWindow ? row.window : row.metric)
                        .font(.system(size: 12, weight: .semibold))
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
                            .frame(width: 26, height: 22)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help(overviewToggle.visible ? "Hide from Overview" : "Show in Overview")
                    .accessibilityLabel(overviewToggle.visible ? "Hide from Overview" : "Show in Overview")
                    .focusEffectDisabled()
                }
                Text(summaryValueText(row.value) + (row.disabled ? " (off)" : ""))
                    .font(.system(size: 12, weight: .semibold))
                    .monospacedDigit()
                    .foregroundStyle(valueColor)
                    .contentTransition(.numericText())
                    .fixedSize(horizontal: true, vertical: false)
                if showWindow {
                    Text(resetText(row.resetsAt))
                        .font(.system(size: 11))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: true, vertical: false)
                }
            }
            if let ratio = row.remainingRatio {
                // No animation: the row is rebuilt whenever the popover is,
                // and a ratio that slides toward its value only reads as ten
                // cells shifting on their own. The bar states the number it
                // has right now.
                SegmentedProgress(ratio: ratio)
            }
        }
    }

    private var valueColor: Color {
        guard let ratio = row.remainingRatio, !row.disabled else { return .primary }
        return Color(nsColor: progressColor(for: RemainingTier(ratio: ratio)))
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
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        GeometryReader { geo in
            Capsule()
                .fill(colorScheme == .dark ? Color.white.opacity(0.14) : Color.black.opacity(0.10))
                .overlay(alignment: .trailing) {
                    Rectangle()
                        .fill(color)
                        .frame(width: geo.size.width * min(max(fill, 0), 1))
                }
                .clipShape(Capsule())
                .shadow(color: color.opacity(fill > 0 ? 0.55 : 0), radius: 3)
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
        VStack(spacing: 12) {
            LiquidVessel(ratio: 0, size: 56)
            Text(title).font(.headline)
            if let detail {
                Text(detail)
                    .font(.callout)
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
            }
            if let actionTitle, let action {
                Button(actionTitle, action: action)
                    .focusEffectDisabled()
                    .padding(.top, 2)
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 26)
        .frame(maxWidth: .infinity, minHeight: 180)
        .glassPanel()
        .padding(.top, 12)
    }
}

func relativeTimeText(_ date: Date, now: Date = Date()) -> String {
    let seconds = Int(date.timeIntervalSince(now))
    let absolute = abs(seconds)
    let text: String
    if absolute < 60 { text = "\(absolute)s" }
    else if absolute < 3_600 { text = "\(absolute / 60)m" }
    else if absolute < 86_400 { text = "\(absolute / 3_600)h" }
    else { text = "\(absolute / 86_400)d" }
    return seconds >= 0 ? "in \(text)" : "\(text) ago"
}

/// How long until a window resets. Between an hour and two days it is given as
/// hours and minutes: "in 1d" hides whether the wait is 25 hours or 47, and
/// that is the difference between waiting a limit out and planning around it.
/// Past two days the minutes say nothing, so days take over, and under an hour
/// minutes and seconds stay as they are.
func resetRelativeText(_ date: Date, now: Date = Date()) -> String {
    let seconds = date.timeIntervalSince(now)
    let absolute = abs(seconds)
    guard absolute >= 3_600, absolute < 172_800 else { return relativeTimeText(date, now: now) }
    // Round to the minute first, so 59.7 minutes carries into the hour instead
    // of printing as "0h60m".
    let totalMinutes = Int((absolute / 60).rounded())
    let hours = totalMinutes / 60
    let minutes = totalMinutes % 60
    let text = minutes == 0 ? "\(hours)h" : "\(hours)h\(minutes)m"
    return seconds >= 0 ? "in \(text)" : "\(text) ago"
}

private func resetText(_ date: Date?) -> String {
    date.map { "resets \(resetRelativeText($0))" } ?? ""
}

private func errorKind(_ error: SanitizedErrorPayload?) -> String {
    sanitizedErrorKind(error) ?? "none"
}
