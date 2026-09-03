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
        let localService = LocalServiceManager(settings: settings)
        let iconController = ApplicationIconController()
        do {
            try iconController.apply(palette: settings.iconPalette)
        } catch {
            NSLog("Could not set the application icon: \(error.localizedDescription)")
        }
        let store = UsageStore(dataSourceFactory: {
            switch mode {
            case .mock:
                return MockDataSource()
            case .daemon:
                switch settings.transportMode {
                case .local:
                    guard let socketURL = localService.socketURL else {
                        throw LocalControlError.unavailable("App Group container is unavailable")
                    }
                    return LocalControlClient(socketURL: socketURL)
                case .remote:
                    guard let token = try Keychain.loadDeviceToken(), !token.isEmpty else {
                        throw DataSourceSetupError.deviceNotPaired
                    }
                    return DaemonClient(baseURL: settings.serverURL, token: token)
                }
            }
        })
        let controller = StatusItemController(
            store: store,
            settings: settings,
            localService: localService,
            mode: mode,
            applicationIconController: iconController
        )
        statusItemController = controller
        if case .daemon = mode {
            Task { await localService.reconcileWithTransportMode() }
        }
    }
}

@MainActor
final class ApplicationIconController {
    private let setter: (NSImage) -> Void

    init(setter: @escaping (NSImage) -> Void = { NSApp.applicationIconImage = $0 }) {
        self.setter = setter
    }

    func apply(palette: AppPalette) throws {
        setter(try applicationIconImage(palette: palette, pixelSize: 512, pointSize: 512))
    }
}

@MainActor
func applicationIconImage(
    palette: AppPalette,
    pixelSize: Int,
    pointSize: CGFloat
) throws -> NSImage {
    let representation = try UllageMark.applicationIcon(pixelSize: pixelSize, palette: palette)
    let image = NSImage(size: NSSize(width: pointSize, height: pointSize))
    image.addRepresentation(representation)
    return image
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
