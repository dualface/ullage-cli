import AppKit
import Foundation
import UllageKit

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var statusItemController: StatusItemController?
    private var applicationIconController: ApplicationIconController?

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let mode = AppMode.current
        let settings = AppSettings()
        let iconController = ApplicationIconController()
        do {
            try iconController.apply(palette: settings.iconPalette)
        } catch {
            NSLog("Could not set the application icon: \(error.localizedDescription)")
        }
        applicationIconController = iconController
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
        statusItemController = StatusItemController(
            store: store,
            settings: settings,
            mode: mode,
            applicationIconController: iconController
        )
    }
}

@MainActor
final class ApplicationIconController {
    private let setter: (NSImage) -> Void

    init(setter: @escaping (NSImage) -> Void = { NSApp.applicationIconImage = $0 }) {
        self.setter = setter
    }

    func apply(palette: UllageMark.Palette) throws {
        let representation = try UllageMark.applicationIcon(pixelSize: 512, palette: palette)
        let image = NSImage(size: NSSize(width: 512, height: 512))
        image.addRepresentation(representation)
        setter(image)
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
