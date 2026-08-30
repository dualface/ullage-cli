import Foundation

@MainActor
final class AppSettings {
    static let defaultServerURL = URL(string: "http://127.0.0.1:7878")!
    private static let serverURLKey = "serverURL"

    var serverURL: URL {
        guard let stored = UserDefaults.standard.string(forKey: Self.serverURLKey),
              let url = Self.validatedServerURL(stored) else {
            return Self.defaultServerURL
        }
        return url
    }

    func save(serverURL rawValue: String, token: String?) throws {
        guard let url = Self.validatedServerURL(rawValue) else {
            throw SettingsError.invalidServerURL
        }
        UserDefaults.standard.set(url.absoluteString, forKey: Self.serverURLKey)
        if let token, !token.isEmpty {
            try Keychain.saveToken(token)
        }
    }

    nonisolated static func validatedServerURL(_ rawValue: String) -> URL? {
        guard let components = URLComponents(string: rawValue),
              components.scheme?.lowercased() == "http",
              let host = components.host?.lowercased(),
              host == "127.0.0.1" || host == "localhost",
              components.user == nil,
              components.password == nil,
              components.query == nil,
              components.fragment == nil else { return nil }
        return components.url
    }
}

enum SettingsError: Error {
    case invalidServerURL
}
