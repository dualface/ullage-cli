import Foundation

public enum UllageFixtures {
    public static let names = ["chatgpt", "claude", "cursor", "grok"]

    public static func snapshot(named name: String) throws -> SnapshotPayload {
        guard names.contains(name),
              let url = Bundle.module.url(forResource: name, withExtension: "json", subdirectory: "Fixtures")
                ?? Bundle.module.url(forResource: name, withExtension: "json") else {
            throw FixtureError.notFound(name)
        }
        return try UllageJSON.makeDecoder().decode(SnapshotPayload.self, from: Data(contentsOf: url))
    }

    public static func snapshots() throws -> [SnapshotPayload] {
        try names.map(snapshot(named:))
    }
}

public enum FixtureError: Error, Equatable {
    case notFound(String)
}
