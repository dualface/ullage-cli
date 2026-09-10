import Foundation
import Observation
import SwiftUI
import UllageKit

/// Who may change an account's stored display metric filter.
enum MetricFilterAccess {
    /// Local daemon mode: the filter can be edited and persisted through the
    /// local control channel.
    case editable(AccountMetricsWriter)
    /// Remote or mock mode: the filter is shown but cannot be changed.
    case readOnly(reason: String)

    var writer: AccountMetricsWriter? {
        guard case .editable(let writer) = self else { return nil }
        return writer
    }

    var readOnlyReason: String? {
        guard case .readOnly(let reason) = self else { return nil }
        return reason
    }
}

/// Persists an account's display metric filter through the local control
/// channel. The closure is injected so the popover does not need to know how
/// the socket is found.
@MainActor
struct AccountMetricsWriter {
    let setMetrics: @MainActor (_ account: String, _ metrics: [String]) async throws -> Account
}

/// The distinct display metrics the editor offers for an account, in the order
/// the account's data presents them.
///
/// The list is built from the unfiltered projection, so a filter that hides a
/// metric does not remove it from the editor.
func metricFilterCandidates(for usage: SubscriptionUsage) -> [String] {
    var names: [String] = []
    for row in identifiedSummaryRows(for: usage) {
        let name = row.row.metric
        guard !names.contains(where: { MetricFilter.namesAreEquivalent($0, name) }) else { continue }
        names.append(name)
    }
    return names
}

/// The editor's state, kept apart from the view so a failed write is testable
/// without a window.
@MainActor
@Observable
final class MetricFilterEditorModel {
    let accountID: String
    /// Every metric the user can choose, data metrics first, then any stored
    /// name the current data no longer reports.
    private(set) var candidates: [String]
    private(set) var selected: Set<String>
    private(set) var busy = false
    /// A visible failure message; empty while everything is fine.
    private(set) var message = ""

    private let initiallyActive: Bool
    private let initialSelection: Set<String>
    private let initialNames: [String]
    private let save: @MainActor (_ account: String, _ metrics: [String]) async throws -> Account

    init(
        accountID: String,
        filterNames: [String],
        dataMetrics: [String],
        save: @escaping @MainActor (_ account: String, _ metrics: [String]) async throws -> Account
    ) {
        self.accountID = accountID
        self.save = save
        var merged: [String] = []
        for name in dataMetrics + filterNames {
            guard !merged.contains(where: { MetricFilter.namesAreEquivalent($0, name) }) else { continue }
            merged.append(name)
        }
        candidates = merged
        let matched = merged.filter { candidate in
            filterNames.contains { MetricFilter.namesAreEquivalent($0, candidate) }
        }
        // No stored filter means every metric is shown, so every box is
        // checked; clearing a box then narrows the filter.
        let selection = filterNames.isEmpty ? Set(merged) : Set(matched)
        selected = selection
        initialSelection = selection
        initialNames = matched
        initiallyActive = !filterNames.isEmpty
    }

    /// Whether the account stores a filter at all.
    var isFilterActive: Bool { initiallyActive }

    /// The value a Save writes. An untouched selection keeps the stored
    /// filter, every metric checked means "show everything" (the inactive
    /// filter, not a filter naming every current metric), and any other
    /// selection narrows to the checked names.
    var namesToSave: [String] {
        if selected == initialSelection { return initialNames }
        if selected.count == candidates.count { return [] }
        return candidates.filter { selected.contains($0) }
    }

    func isSelected(_ name: String) -> Bool { selected.contains(name) }

    func toggle(_ name: String, isOn: Bool) {
        if isOn {
            selected.insert(name)
            message = ""
        } else if selected.count > 1 {
            selected.remove(name)
            message = ""
        } else {
            message = "Choose at least one metric, or clear the filter to show every metric."
        }
    }

    /// Saves the current selection, or an empty list when `clearing` is true.
    /// Returns whether the write succeeded; failures land in `message`.
    @discardableResult
    func saveChanges(clearing: Bool = false) async -> Bool {
        busy = true
        defer { busy = false }
        do {
            _ = try await save(accountID, clearing ? [] : namesToSave)
            message = ""
            return true
        } catch {
            message = Self.failureMessage(for: error)
            return false
        }
    }

    private static func failureMessage(for error: Error) -> String {
        if case LocalControlError.server(let kind) = error, kind == "invalid_account_metrics" {
            return "The local service rejected the metric filter."
        }
        return error.localizedDescription
    }
}

/// The account page's compact filter row: what is stored, and how to change it.
struct MetricFilterSummaryRow: View {
    let names: [String]
    let access: MetricFilterAccess
    let edit: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Label("Metric filter", systemImage: "line.3.horizontal.decrease.circle")
                    .font(.system(size: 12, weight: .semibold))
                Text(summary)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Spacer(minLength: 4)
                if access.writer != nil {
                    Button("Edit…") { edit() }
                        .font(.caption)
                }
            }
            if let reason = access.readOnlyReason {
                Text(reason)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .glassPanel()
    }

    private var summary: String {
        names.isEmpty ? "All metrics" : names.joined(separator: ", ")
    }
}

/// The sheet that lists an account's display metrics with checkmarks and
/// writes the selection through the local control channel.
struct MetricFilterEditorView: View {
    @Environment(\.dismiss) private var dismiss
    @State private var model: MetricFilterEditorModel
    private let onSaved: () -> Void

    init(
        account: Account,
        usage: SubscriptionUsage,
        writer: AccountMetricsWriter,
        onSaved: @escaping () -> Void
    ) {
        _model = State(initialValue: MetricFilterEditorModel(
            accountID: account.id,
            filterNames: account.metrics,
            dataMetrics: metricFilterCandidates(for: usage),
            save: writer.setMetrics
        ))
        self.onSaved = onSaved
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Display metrics")
                    .font(.title3.bold())
                Text("Choose the metric rows this account shows. Clearing the filter shows every metric, including ones added later.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if model.candidates.isEmpty {
                Text("This account has no display metrics yet.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 8) {
                        ForEach(model.candidates, id: \.self) { name in
                            Toggle(name, isOn: binding(for: name))
                                .toggleStyle(.checkbox)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.vertical, 2)
                }
                .frame(maxHeight: 260)
            }
            if !model.message.isEmpty {
                Text(model.message)
                    .font(.callout)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                if model.busy {
                    ProgressView().controlSize(.small)
                }
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                    .disabled(model.busy)
                Button("Clear Filter") {
                    Task {
                        if await model.saveChanges(clearing: true) {
                            onSaved()
                            dismiss()
                        }
                    }
                }
                .disabled(model.busy || model.candidates.isEmpty || !model.isFilterActive)
                Button("Save") {
                    Task {
                        if await model.saveChanges() {
                            onSaved()
                            dismiss()
                        }
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(model.busy || model.candidates.isEmpty)
            }
        }
        .padding(20)
        .frame(width: 380)
    }

    private func binding(for name: String) -> Binding<Bool> {
        Binding(
            get: { model.isSelected(name) },
            set: { model.toggle(name, isOn: $0) }
        )
    }
}
