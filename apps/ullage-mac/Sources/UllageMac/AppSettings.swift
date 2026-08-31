import Foundation
import Observation

@MainActor
@Observable
final class AppSettings {
    static let defaultServerURL = URL(string: "http://127.0.0.1:7878")!
    private static let serverURLKey = "serverURL"
    private static let iconPaletteKey = "iconPalette"
    private let defaults: UserDefaults

    var iconPalette: UllageMark.Palette {
        didSet { defaults.set(iconPalette.rawValue, forKey: Self.iconPaletteKey) }
    }

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        iconPalette = defaults.string(forKey: Self.iconPaletteKey)
            .flatMap(UllageMark.Palette.init(rawValue:)) ?? .default
    }

    var serverURL: URL {
        guard let stored = defaults.string(forKey: Self.serverURLKey),
              let url = Self.validatedServerURL(stored) else {
            return Self.defaultServerURL
        }
        return url
    }

    func save(serverURL rawValue: String, token: String?) throws {
        guard let url = Self.validatedServerURL(rawValue) else {
            throw SettingsError.invalidServerURL
        }
        defaults.set(url.absoluteString, forKey: Self.serverURLKey)
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
