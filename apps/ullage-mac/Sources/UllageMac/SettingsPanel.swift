import AppKit
import SwiftUI
import UllageKit

@MainActor
final class SettingsPanelController: NSWindowController {
    init(settings: AppSettings, mode: AppMode, onSaved: @escaping () -> Void) {
        let view = SettingsView(settings: settings, mode: mode, onSaved: onSaved)
        let hostingController = NSHostingController(rootView: view)
        let panel = NSPanel(contentViewController: hostingController)
        panel.title = "Ullage Settings"
        panel.styleMask = [.titled, .closable]
        panel.setContentSize(NSSize(width: 420, height: 250))
        panel.isReleasedWhenClosed = false
        super.init(window: panel)
    }

    required init?(coder: NSCoder) { nil }

    func show() {
        showWindow(nil)
        window?.center()
        window?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
}

private enum ConnectionTestResult: String {
    case connected
    case tokenRejected = "token rejected"
    case hostRejected = "host rejected"
    case unreachable
}

private struct SettingsView: View {
    let settings: AppSettings
    let mode: AppMode
    let onSaved: () -> Void
    @State private var serverURL: String
    @State private var token = ""
    @State private var message = ""
    @State private var isTesting = false

    init(settings: AppSettings, mode: AppMode, onSaved: @escaping () -> Void) {
        self.settings = settings
        self.mode = mode
        self.onSaved = onSaved
        _serverURL = State(initialValue: settings.serverURL.absoluteString)
    }

    var body: some View {
        Form {
            TextField("Server URL", text: $serverURL)
                .textFieldStyle(.roundedBorder)
            SecureField("Token", text: $token)
                .textFieldStyle(.roundedBorder)
            Text("The saved token is never displayed. Leave it blank to keep the current token.")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack {
                Button("Save") { save() }
                Button("Test connection") { testConnection() }
                    .disabled(isTesting)
                if isTesting { ProgressView().controlSize(.small) }
                Spacer()
                Text(message).foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .padding()
        .frame(width: 420, height: 250)
    }

    private func save() {
        do {
            try settings.save(serverURL: serverURL, token: token.isEmpty ? nil : token)
            token = ""
            message = "saved"
            onSaved()
        } catch SettingsError.invalidServerURL {
            message = "host rejected"
        } catch {
            message = "could not save token"
        }
    }

    private func testConnection() {
        guard let url = AppSettings.validatedServerURL(serverURL) else {
            message = ConnectionTestResult.hostRejected.rawValue
            return
        }
        isTesting = true
        Task {
            defer { isTesting = false }
            if mode == .mock {
                message = ConnectionTestResult.connected.rawValue
                return
            }
            do {
                let saved = try Keychain.loadToken()
                let effectiveToken = token.isEmpty ? saved : token
                guard let effectiveToken, !effectiveToken.isEmpty else {
                    message = ConnectionTestResult.tokenRejected.rawValue
                    return
                }
                _ = try await DaemonClient(baseURL: url, token: effectiveToken).status()
                message = ConnectionTestResult.connected.rawValue
            } catch let error as DaemonError {
                message = switch error {
                case .unauthorized, .authenticationInvalid: ConnectionTestResult.tokenRejected.rawValue
                case .forbiddenHost: ConnectionTestResult.hostRejected.rawValue
                default: ConnectionTestResult.unreachable.rawValue
                }
            } catch {
                message = ConnectionTestResult.unreachable.rawValue
            }
        }
    }
}
