import Foundation
import ServiceManagement

@MainActor
enum LocalServiceCommand {
    static func run(arguments: [String]) async -> Int32 {
        guard arguments.count == 1,
              ["register", "unregister", "status"].contains(arguments[0]) else {
            write("usage: UllageMac --local-service-test <register|unregister|status>")
            return 2
        }
        let service = SMAppService.loginItem(identifier: LocalServiceManager.helperIdentifier)
        do {
            switch arguments[0] {
            case "register": try service.register()
            case "unregister":
                if service.status != .notRegistered { try await service.unregister() }
            default: break
            }
            write(statusName(service.status), standardError: false)
            return 0
        } catch {
            write(error.localizedDescription)
            return 1
        }
    }

    private static func statusName(_ status: SMAppService.Status) -> String {
        switch status {
        case .notRegistered: "not_registered"
        case .enabled: "enabled"
        case .requiresApproval: "requires_approval"
        case .notFound: "not_found"
        @unknown default: "unknown"
        }
    }

    private static func write(_ message: String, standardError: Bool = true) {
        let handle = standardError ? FileHandle.standardError : FileHandle.standardOutput
        handle.write(Data((message + "\n").utf8))
    }
}
