import Foundation
import Observation
import ServiceManagement

@MainActor
protocol LocalServiceRegistration: AnyObject {
    var status: SMAppService.Status { get }
    func register() throws
    func unregister() async throws
}

extension SMAppService: LocalServiceRegistration {}

enum LocalServiceState: Equatable {
    case unavailable
    case disabled
    case enabled
    case requiresApproval
    case updating
    case failed(String)

    var detail: String {
        switch self {
        case .unavailable: "Run Ullage from its signed app bundle to manage the local service."
        case .disabled: "The local service is off."
        case .enabled: "The local service stays available after the menu bar UI quits."
        case .requiresApproval:
            "Allow Ullage Local Service in System Settings > General > Login Items."
        case .updating: "Updating the local service…"
        case .failed(let message): message
        }
    }
}

@MainActor
@Observable
final class LocalServiceManager {
    static let appGroupIdentifier = "group.com.ullage.mac"
    static let helperIdentifier = "com.ullage.mac.daemon"

    private let settings: AppSettings
    private let service: any LocalServiceRegistration
    private let applicationBundleURL: URL
    private(set) var state: LocalServiceState = .disabled

    init(
        settings: AppSettings,
        service: any LocalServiceRegistration = SMAppService.loginItem(identifier: helperIdentifier),
        applicationBundleURL: URL = Bundle.main.bundleURL
    ) {
        self.settings = settings
        self.service = service
        self.applicationBundleURL = applicationBundleURL
        refresh()
    }

    var socketURL: URL? {
        FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: Self.appGroupIdentifier
        )?.appendingPathComponent("run/control.sock")
    }

    func refresh() {
        guard isApplicationBundleURL(applicationBundleURL) else {
            state = .unavailable
            return
        }
        state = switch service.status {
        case .enabled: .enabled
        case .requiresApproval: .requiresApproval
        case .notRegistered, .notFound: .disabled
        @unknown default: .disabled
        }
    }

    /// Makes Login Item registration match the selected transport. This is the
    /// only service enable/disable path: Local registers, Remote unregisters.
    func reconcileWithTransportMode() async {
        guard isApplicationBundleURL(applicationBundleURL) else {
            state = .unavailable
            return
        }
        let requestedMode = settings.transportMode
        state = .updating
        do {
            switch requestedMode {
            case .local:
                switch service.status {
                case .notRegistered, .notFound:
                    try service.register()
                case .enabled, .requiresApproval:
                    break
                @unknown default:
                    break
                }
            case .remote:
                switch service.status {
                case .notRegistered, .notFound:
                    break
                case .enabled, .requiresApproval:
                    try await service.unregister()
                @unknown default:
                    try await service.unregister()
                }
            }
            if settings.transportMode != requestedMode {
                await reconcileWithTransportMode()
                return
            }
            refresh()
        } catch {
            if settings.transportMode != requestedMode {
                await reconcileWithTransportMode()
            } else {
                state = .failed(error.localizedDescription)
            }
        }
    }

    func restart() async {
        guard settings.transportMode == .local else {
            await reconcileWithTransportMode()
            return
        }
        guard isApplicationBundleURL(applicationBundleURL) else {
            state = .unavailable
            return
        }
        state = .updating
        do {
            switch service.status {
            case .notRegistered, .notFound:
                break
            case .enabled, .requiresApproval:
                try await service.unregister()
            @unknown default:
                try await service.unregister()
            }
            guard settings.transportMode == .local else {
                await reconcileWithTransportMode()
                return
            }
            try service.register()
            refresh()
        } catch {
            state = .failed(error.localizedDescription)
        }
    }
}
