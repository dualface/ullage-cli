import AppKit
import Foundation
import UllageKit

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var statusItemController: StatusItemController?

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let mode = AppMode.current
        let settings = AppSettings()
        let store = UsageStore(dataSourceFactory: {
            switch mode {
            case .mock:
                return MockDataSource()
            case .daemon:
                guard let token = try Keychain.loadToken(), !token.isEmpty else {
                    throw DataSourceSetupError.tokenNotConfigured
                }
                return DaemonClient(baseURL: settings.serverURL, token: token)
            }
        })
        statusItemController = StatusItemController(store: store, settings: settings, mode: mode)
    }
}

enum AppMode {
    case mock
    case daemon

    static var current: Self {
        let arguments = ProcessInfo.processInfo.arguments
        let environment = ProcessInfo.processInfo.environment
        return arguments.contains("--mock") || environment["ULLAGE_MOCK"] == "1" ? .mock : .daemon
    }
}
