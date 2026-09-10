import Foundation
import XCTest
@testable import UllageMac
import UllageKit

final class MetricFilterEditorTests: XCTestCase {
    @MainActor
    func testEditorChecksEveryMetricWhenNoFilterIsStored() {
        let model = MetricFilterEditorModel(
            accountID: "account-1",
            filterNames: [],
            dataMetrics: ["usage", "Codex"],
            save: { account, metrics in
                Account(id: account, provider: "chatgpt", label: nil, enabled: true, metrics: metrics)
            }
        )
        XCTAssertFalse(model.isFilterActive)
        XCTAssertTrue(model.isSelected("usage"))
        XCTAssertTrue(model.isSelected("Codex"))
        XCTAssertEqual(model.namesToSave, [])

        model.toggle("usage", isOn: false)
        XCTAssertEqual(model.namesToSave, ["Codex"])
    }

    @MainActor
    func testEditorMergesStoredNamesWithTheDataInOrder() {
        let model = MetricFilterEditorModel(
            accountID: "account-1",
            filterNames: ["Codex", "retired metric"],
            dataMetrics: ["usage", "codex", "requests"],
            save: { account, metrics in
                Account(id: account, provider: "chatgpt", label: nil, enabled: true, metrics: metrics)
            }
        )
        XCTAssertEqual(model.candidates, ["usage", "codex", "requests", "retired metric"])
        XCTAssertTrue(model.isFilterActive)
        XCTAssertTrue(model.isSelected("codex"))
        XCTAssertFalse(model.isSelected("Codex"))
        XCTAssertTrue(model.isSelected("retired metric"))
        XCTAssertFalse(model.isSelected("requests"))
        XCTAssertEqual(model.namesToSave, ["codex", "retired metric"])

        model.toggle("requests", isOn: true)
        XCTAssertEqual(model.namesToSave, ["codex", "requests", "retired metric"])
        model.toggle("usage", isOn: true)
        XCTAssertEqual(model.namesToSave, [])
    }

    @MainActor
    func testEditorWritesSubsetsAndClearsThroughTheWriter() async {
        final class Recorder {
            var writes: [(String, [String])] = []
        }
        let recorder = Recorder()
        let model = MetricFilterEditorModel(
            accountID: "account-1",
            filterNames: [],
            dataMetrics: ["usage", "Codex"],
            save: { account, metrics in
                recorder.writes.append((account, metrics))
                return Account(id: account, provider: "chatgpt", label: nil, enabled: true, metrics: metrics)
            }
        )

        let firstSave = await model.saveChanges()
        XCTAssertTrue(firstSave)
        XCTAssertEqual(recorder.writes.map(\.0), ["account-1"])
        XCTAssertEqual(recorder.writes.map(\.1), [[]])
        XCTAssertTrue(model.message.isEmpty)

        model.toggle("usage", isOn: false)
        let secondSave = await model.saveChanges()
        XCTAssertTrue(secondSave)
        XCTAssertEqual(recorder.writes.map(\.1), [[], ["Codex"]])

        let clearSave = await model.saveChanges(clearing: true)
        XCTAssertTrue(clearSave)
        XCTAssertEqual(recorder.writes.map(\.1), [[], ["Codex"], []])
    }

    @MainActor
    func testEditorKeepsOneMetricCheckedAndReportsWriteFailures() async {
        struct WriteFailure: LocalizedError {
            var errorDescription: String? { "local service unreachable" }
        }
        let model = MetricFilterEditorModel(
            accountID: "account-1",
            filterNames: ["usage"],
            dataMetrics: ["usage"],
            save: { _, _ in throw WriteFailure() }
        )

        model.toggle("usage", isOn: false)
        XCTAssertTrue(model.isSelected("usage"))
        XCTAssertTrue(model.message.contains("at least one"))

        let save = await model.saveChanges()
        XCTAssertFalse(save)
        XCTAssertEqual(model.message, "local service unreachable")
    }

    @MainActor
    func testEditorMapsInvalidMetricErrorsToAVisibleMessage() async {
        let model = MetricFilterEditorModel(
            accountID: "account-1",
            filterNames: ["usage"],
            dataMetrics: ["usage"],
            save: { _, _ in throw LocalControlError.server("invalid_account_metrics") }
        )

        let save = await model.saveChanges()
        XCTAssertFalse(save)
        XCTAssertEqual(model.message, "The local service rejected the metric filter.")
    }

    @MainActor
    func testCandidateNamesFollowTheUnfilteredAccountProjection() throws {
        let usage = try XCTUnwrap(UllageFixtures.snapshot(named: "chatgpt").usage.data)
        XCTAssertEqual(
            metricFilterCandidates(for: usage),
            ["Codex", "requests", "GPT-5.3-Codex-Spark", "available count", "credit balance"]
        )
    }
}
