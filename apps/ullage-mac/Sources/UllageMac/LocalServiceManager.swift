import Foundation
import Observation
import ServiceManagement

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
    private let service: SMAppService
    private(set) var state: LocalServiceState = .disabled

    init(
        settings: AppSettings,
        service: SMAppService = .loginItem(identifier: helperIdentifier)
    ) {
        self.settings = settings
        self.service = service
        refresh()
    }

    var socketURL: URL? {
        FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: Self.appGroupIdentifier
        )?.appendingPathComponent("run/control.sock")
    }

    func refresh() {
        guard isApplicationBundleURL(Bundle.main.bundleURL) else {
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

    func setEnabled(_ enabled: Bool) async {
        guard isApplicationBundleURL(Bundle.main.bundleURL) else {
            state = .unavailable
            return
        }
        state = .updating
        do {
            if enabled {
                try service.register()
            } else if service.status != .notRegistered {
                try await service.unregister()
            }
            settings.recordLocalService(enabled: enabled)
            refresh()
        } catch {
            state = .failed(error.localizedDescription)
        }
    }

    func restoreEnabledServiceIfNeeded() async {
        guard settings.backgroundServiceEnabled else { return }
        switch service.status {
        case .notRegistered, .notFound:
            await setEnabled(true)
        case .enabled, .requiresApproval:
            refresh()
        @unknown default:
            refresh()
        }
    }

    func restart() async {
        state = .updating
        do {
            if service.status != .notRegistered {
                try await service.unregister()
            }
            try service.register()
            settings.recordLocalService(enabled: true)
            refresh()
        } catch {
            state = .failed(error.localizedDescription)
        }
    }
}
