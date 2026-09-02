import Darwin
import Foundation
import Observation
import UllageKit

@MainActor
@Observable
final class AppSettings {
    static let defaultServerURL = URL(string: "http://127.0.0.1:7878")!
    private static let serverURLKey = "serverURL"
    private static let iconPaletteKey = "iconPalette"
    private static let pairedDeviceNameKey = "pairedDeviceName"
    private static let pairedAtKey = "pairedAt"
    private static let animatesMenuBarLiquidKey = "animatesMenuBarLiquid"
    private static let usesLiquidGlassKey = "usesLiquidGlass"
    private static let menuBarMetricIDKey = "menuBarMetricID"
    private static let hiddenOverviewItemIDsKey = "hiddenOverviewItemIDs"
    private static let shownOverviewItemIDsKey = "shownOverviewItemIDs"
    private let defaults: UserDefaults
    private let saveDeviceToken: (String) throws -> Void

    private(set) var pairedDeviceName: String?
    private(set) var pairedAt: Date?

    var iconPalette: UllageMark.Palette {
        didSet { defaults.set(iconPalette.rawValue, forKey: Self.iconPaletteKey) }
    }

    /// Whether the menu bar liquid breathes and wobbles. Off draws the floor
    /// level with a flat surface and runs no animation timer.
    var animatesMenuBarLiquid: Bool {
        didSet { defaults.set(animatesMenuBarLiquid, forKey: Self.animatesMenuBarLiquidKey) }
    }

    /// Whether the popover draws itself in Liquid Glass. Off is the
    /// presentation macOS 14 and 15 get, down to the `NSPopover` frame, and it
    /// is what the option means on macOS 26 as well. Below macOS 26 the value
    /// is stored but has nothing to switch.
    var usesLiquidGlass: Bool {
        didSet { defaults.set(usesLiquidGlass, forKey: Self.usesLiquidGlassKey) }
    }

    /// `MenuBarMetricOption.id` pinned as the liquid level; `nil` means the
    /// lowest remaining across all accounts.
    var menuBarMetricID: String? {
        didSet {
            if let menuBarMetricID {
                defaults.set(menuBarMetricID, forKey: Self.menuBarMetricIDKey)
            } else {
                defaults.removeObject(forKey: Self.menuBarMetricIDKey)
            }
        }
    }

    /// Catalog progress IDs the user hid from Overview.
    var hiddenOverviewItemIDs: Set<String> {
        didSet {
            defaults.set(Array(hiddenOverviewItemIDs).sorted(), forKey: Self.hiddenOverviewItemIDsKey)
        }
    }

    /// Extra (non-catalog) progress IDs the user added to Overview.
    var shownOverviewItemIDs: Set<String> {
        didSet {
            defaults.set(Array(shownOverviewItemIDs).sorted(), forKey: Self.shownOverviewItemIDsKey)
        }
    }

    init(
        defaults: UserDefaults = .standard,
        saveDeviceToken: @escaping (String) throws -> Void = { try Keychain.replaceDeviceToken($0) }
    ) {
        self.defaults = defaults
        self.saveDeviceToken = saveDeviceToken
        pairedDeviceName = defaults.string(forKey: Self.pairedDeviceNameKey)
        pairedAt = defaults.object(forKey: Self.pairedAtKey) as? Date
        iconPalette = defaults.string(forKey: Self.iconPaletteKey)
            .flatMap(UllageMark.Palette.init(rawValue:)) ?? .default
        animatesMenuBarLiquid = defaults.object(forKey: Self.animatesMenuBarLiquidKey) as? Bool ?? true
        usesLiquidGlass = defaults.object(forKey: Self.usesLiquidGlassKey) as? Bool ?? true
        menuBarMetricID = defaults.string(forKey: Self.menuBarMetricIDKey).flatMap { $0.isEmpty ? nil : $0 }
        hiddenOverviewItemIDs = Set(defaults.stringArray(forKey: Self.hiddenOverviewItemIDsKey) ?? [])
        shownOverviewItemIDs = Set(defaults.stringArray(forKey: Self.shownOverviewItemIDsKey) ?? [])
    }

    func setOverviewItemVisible(_ id: String, visible: Bool, catalogDefault: Bool = true) {
        if visible {
            hiddenOverviewItemIDs.remove(id)
            if catalogDefault {
                shownOverviewItemIDs.remove(id)
            } else {
                shownOverviewItemIDs.insert(id)
            }
        } else {
            shownOverviewItemIDs.remove(id)
            if catalogDefault {
                hiddenOverviewItemIDs.insert(id)
            } else {
                hiddenOverviewItemIDs.remove(id)
            }
        }
    }

    var serverURL: URL {
        guard let stored = defaults.string(forKey: Self.serverURLKey),
              let url = Self.validatedServerURL(stored) else {
            return Self.defaultServerURL
        }
        return url
    }

    func pairedServerURL(matching rawValue: String) -> URL? {
        guard pairedDeviceName != nil,
              pairedAt != nil,
              let url = Self.validatedServerURL(rawValue),
              url == serverURL else { return nil }
        return url
    }

    func completePairing(
        serverURL rawValue: String,
        credential: PairedDeviceCredential,
        pairedAt: Date = Date()
    ) throws {
        guard let url = Self.validatedServerURL(rawValue) else {
            throw SettingsError.invalidServerURL
        }
        try saveDeviceToken(credential.deviceToken)
        defaults.set(url.absoluteString, forKey: Self.serverURLKey)
        defaults.set(credential.deviceName, forKey: Self.pairedDeviceNameKey)
        defaults.set(pairedAt, forKey: Self.pairedAtKey)
        pairedDeviceName = credential.deviceName
        self.pairedAt = pairedAt
    }

    nonisolated static func validatedServerURL(_ rawValue: String) -> URL? {
        guard let components = URLComponents(string: rawValue),
              components.scheme?.lowercased() == "http",
              let host = components.host?.lowercased(),
              let encodedHost = components.percentEncodedHost,
              !encodedHost.contains("%"),
              !host.contains("\0"),
              host == "localhost" || literalServerAddressIsAllowed(host),
              components.user == nil,
              components.password == nil,
              components.query == nil,
              components.fragment == nil else { return nil }
        return components.url
    }
}

nonisolated func literalServerAddressIsAllowed(_ host: String) -> Bool {
    var ipv4 = in_addr()
    if host.withCString({ inet_pton(AF_INET, $0, &ipv4) }) == 1 {
        let bytes = withUnsafeBytes(of: &ipv4) { Array($0) }
        return bytes[0] == 127
            || (bytes[0] == 100 && (64...127).contains(bytes[1]))
            || bytes[0] == 10
            || (bytes[0] == 172 && (16...31).contains(bytes[1]))
            || (bytes[0] == 192 && bytes[1] == 168)
    }

    let address = if host.first == "[", host.last == "]" {
        String(host.dropFirst().dropLast())
    } else {
        host
    }
    var ipv6 = in6_addr()
    guard address.withCString({ inet_pton(AF_INET6, $0, &ipv6) }) == 1 else {
        return false
    }
    let bytes = withUnsafeBytes(of: &ipv6) { Array($0) }
    let isLoopback = bytes.dropLast().allSatisfy { $0 == 0 } && bytes.last == 1
    let tailscalePrefix: [UInt8] = [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0]
    let isUniqueLocal = bytes[0] & 0xfe == 0xfc
    return isLoopback || bytes.starts(with: tailscalePrefix) || isUniqueLocal
}

enum SettingsError: Error {
    case invalidServerURL
}
