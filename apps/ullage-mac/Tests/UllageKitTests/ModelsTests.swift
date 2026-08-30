import Foundation
import Testing
@testable import UllageKit

@Test func rfc3339AcceptsChronoPrecisionAndUTCOffsets() throws {
    let values = [
        "2026-08-31T12:34:56Z",
        "2026-08-31T12:34:56.123Z",
        "2026-08-31T12:34:56.123456+00:00",
        "2026-08-31T12:34:56.123456789Z",
    ]
    for value in values {
        #expect(RFC3339.date(from: value) != nil)
    }
    #expect(RFC3339.date(from: "2026-02-30T12:34:56Z") == nil)
    #expect(RFC3339.date(from: "2026-08-31 12:34:56Z") == nil)
    #expect(RFC3339.date(from: "2026-08-31T12:34:56.1234567890Z") == nil)
}

@Test func unknownProtocolTagsDoNotRejectTheResponse() throws {
    let json = Data(#"""
    {
      "account_id":"fixture",
      "usage":{"outcome":"future_outcome"},
      "last_success_at":"2026-08-31T00:00:00Z",
      "stale":true,
      "last_error":"future_error",
      "last_error_at":null
    }
    """#.utf8)
    let snapshot = try UllageJSON.makeDecoder().decode(SnapshotPayload.self, from: json)
    #expect(snapshot.usage == .unknown("future_outcome"))
    #expect(snapshot.lastError == .unknown("future_error"))

    let tagged = Data(#"""
    {
      "provider":"future","account_label":null,"plan":null,
      "subscription_expires_at":null,"observed_at":"2026-08-31T00:00:00Z",
      "windows":[{"window":{"kind":"future_window"},"resets_at":null,
      "measurements":[{"name":"future","used":1,"limit":null,"unit":{"kind":"future_unit"}}]}]
    }
    """#.utf8)
    let usage = try UllageJSON.makeDecoder().decode(SubscriptionUsage.self, from: tagged)
    #expect(usage.windows[0].window == .unknown("future_window"))
    #expect(usage.windows[0].measurements[0].unit == .unknown("future_unit"))
}

@Test func accountOrderingAndTabTitlesAreStable() {
    let accounts = [
        Account(id: "c2", provider: "claude", label: nil, enabled: true),
        Account(id: "x", provider: "chatgpt", label: "Personal", enabled: true),
        Account(id: "c1", provider: "claude", label: "Work", enabled: true),
        Account(id: "z", provider: "zed", label: nil, enabled: true),
    ]
    #expect(sortedAccounts(accounts).map(\.id) == ["x", "c2", "c1", "z"])
    let titles = tabTitles(for: accounts)
    #expect(titles["x"] == "ChatGPT")
    #expect(titles["c1"] == "Claude · Work")
    #expect(titles["c2"] == "Claude · c2")
    #expect(titles["z"] == "Zed")
}
