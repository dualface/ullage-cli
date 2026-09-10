import Foundation
import Testing
@testable import UllageKit

@Test func metricFilterTrimsDeduplicatesAndMatchesCaseInsensitively() throws {
    let filter = try MetricFilter(names: [" usage ", "Codex", "codex", "USAGE"])
    #expect(filter.names == ["usage", "Codex"])
    #expect(filter.isActive)
    #expect(filter.matches("Usage"))
    #expect(filter.matches(" codex "))
    #expect(filter.matches("CODEX"))
    #expect(!filter.matches("Codex usage"))
    #expect(!MetricFilter.inactive.isActive)
    #expect(!MetricFilter.inactive.matches("usage"))
    #expect(MetricFilter.namesAreEquivalent("Codex", "cODEX"))
    #expect(!MetricFilter.namesAreEquivalent("Codex", "Codex-2"))
}

@Test func metricFilterRejectsInvalidNames() throws {
    #expect(throws: MetricFilter.ValidationError.emptyName) {
        _ = try MetricFilter(names: ["usage", "   "])
    }
    #expect(throws: MetricFilter.ValidationError.nameTooLong) {
        _ = try MetricFilter(names: [String(repeating: "x", count: 129)])
    }
    #expect(throws: MetricFilter.ValidationError.tooManyNames) {
        _ = try MetricFilter(names: (0...64).map { "metric-\($0)" })
    }
    for unsafe in ["bad\u{7}", "bad\u{202e}name", "bad\u{200e}name", "bad\u{2066}name"] {
        #expect(throws: MetricFilter.ValidationError.unsafeCharacter) {
            _ = try MetricFilter(names: [unsafe])
        }
    }
    #expect(try MetricFilter(names: [String(repeating: "x", count: 128)]).names.count == 1)
    #expect(try MetricFilter(names: Array(repeating: "usage", count: 64)).names == ["usage"])
}

@Test func persistedMetricFilterNeverHidesRowsWhenInvalid() throws {
    let invalid = MetricFilter(persistedNames: ["usage", ""])
    #expect(!invalid.isActive)
    #expect(!invalid.matches("usage"))
    #expect(MetricFilter(persistedNames: ["usage"]).isActive)
}
