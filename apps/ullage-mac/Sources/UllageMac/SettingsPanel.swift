import AppKit
import SwiftUI
import UllageKit

@MainActor
final class SettingsPanelController: NSWindowController {
    init(
        settings: AppSettings,
        mode: AppMode,
        onSaved: @escaping () -> Void,
        onPaletteChanged: @escaping (UllageMark.Palette) -> Void
    ) {
        let view = SettingsView(
            settings: settings,
            mode: mode,
            onSaved: onSaved,
            onPaletteChanged: onPaletteChanged
        )
        let hostingController = NSHostingController(rootView: view)
        let panel = NSPanel(contentViewController: hostingController)
        panel.title = "Ullage Settings"
        panel.styleMask = [.titled, .closable]
        panel.setContentSize(NSSize(width: 440, height: 460))
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

enum ConnectionTestResult: String {
    case connected
    case notPaired = "pair this Mac first"
    case deviceRejected = "device token rejected; pair again"
    case hostRejected = "host rejected"
    case unreachable
}

func pairingMessage(for error: DaemonError) -> String {
    switch error {
    case .unauthorized(let kind) where kind == "pair_code_invalid":
        "pair code is invalid, expired, or already used"
    case .rateLimited(let retryAfter, _):
        retryAfter.map { "too many pair attempts; retry in \(Int(ceil($0)))s" }
            ?? "too many pair attempts; retry shortly"
    case .forbiddenHost:
        "daemon rejected this server address"
    case .storage:
        "daemon could not store the device"
    case .timeout(let kind) where kind == "request_timeout":
        "pair request timed out"
    case .unexpectedStatus(400, let kind) where kind == "bad_request":
        "daemon rejected the pair request"
    case .unexpectedStatus(413, let kind) where kind == "payload_too_large":
        "pair request is too large"
    case .decoding:
        "daemon returned an invalid pair response"
    default:
        "daemon unreachable"
    }
}

func localDeviceName() -> String {
    ProcessInfo.processInfo.hostName
}

struct PairingServerTarget {
    let rawValue: String
    let url: URL

    init?(_ rawValue: String) {
        guard let url = AppSettings.validatedServerURL(rawValue) else { return nil }
        self.rawValue = rawValue
        self.url = url
    }
}

@MainActor
func connectionTestRequiresPairing(
    mode: AppMode,
    settings: AppSettings,
    serverURL: String
) -> Bool {
    mode == .daemon && settings.pairedServerURL(matching: serverURL) == nil
}

private struct SettingsView: View {
    @Bindable var settings: AppSettings
    let mode: AppMode
    let onSaved: () -> Void
    let onPaletteChanged: (UllageMark.Palette) -> Void
    @State private var serverURL: String
    @State private var pairCode = ""
    @State private var message = ""
    @State private var isPairing = false
    @State private var isTesting = false

    init(
        settings: AppSettings,
        mode: AppMode,
        onSaved: @escaping () -> Void,
        onPaletteChanged: @escaping (UllageMark.Palette) -> Void
    ) {
        self.settings = settings
        self.mode = mode
        self.onSaved = onSaved
        self.onPaletteChanged = onPaletteChanged
        _serverURL = State(initialValue: settings.serverURL.absoluteString)
    }

    var body: some View {
        Form {
            TextField("Server URL", text: $serverURL)
                .textFieldStyle(.roundedBorder)
                .disabled(isPairing)
            Text("Use HTTP with localhost or a literal loopback, tailnet, or private LAN address.")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack {
                TextField("Pair code", text: $pairCode)
                    .textFieldStyle(.roundedBorder)
                Button("Pair") { pair() }
                    .disabled(isPairing || pairCode.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                if isPairing { ProgressView().controlSize(.small) }
            }
            if let name = settings.pairedDeviceName, let pairedAt = settings.pairedAt {
                Text("Paired as \(name) on \(pairedAt.formatted(date: .abbreviated, time: .shortened))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                Text("Run ullage device pair on the daemon host, then enter the one-use code here.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            HStack {
                Picker("Icon palette", selection: $settings.iconPalette) {
                    ForEach(UllageMark.Palette.allCases) { palette in
                        Text(palette.displayName).tag(palette)
                    }
                }
                Spacer()
                if let preview = iconPreviewImage(palette: settings.iconPalette) {
                    Image(nsImage: preview)
                        .resizable()
                        .frame(width: 64, height: 64)
                        .accessibilityLabel("\(settings.iconPalette.displayName) icon preview")
                }
            }
            HStack {
                Button("Test connection") { testConnection() }
                    .disabled(isTesting)
                if isTesting { ProgressView().controlSize(.small) }
                Spacer()
                Text(message).foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .padding()
        .frame(width: 440, height: 460)
        .onChange(of: settings.iconPalette) { _, palette in
            onPaletteChanged(palette)
        }
    }

    private func pair() {
        guard let target = PairingServerTarget(serverURL) else {
            message = ConnectionTestResult.hostRejected.rawValue
            return
        }
        guard mode == .daemon else {
            message = "pairing unavailable in mock mode"
            return
        }
        isPairing = true
        Task {
            defer { isPairing = false }
            do {
                let credential = try await DaemonClient.pair(
                    baseURL: target.url,
                    pairCode: pairCode,
                    deviceName: localDeviceName()
                )
                try settings.completePairing(serverURL: target.rawValue, credential: credential)
                pairCode = ""
                message = "paired"
                onSaved()
            } catch SettingsError.invalidServerURL {
                message = ConnectionTestResult.hostRejected.rawValue
            } catch let error as DaemonError {
                message = pairingMessage(for: error)
            } catch {
                message = "could not save device token"
            }
        }
    }

    private func testConnection() {
        guard let url = AppSettings.validatedServerURL(serverURL) else {
            message = ConnectionTestResult.hostRejected.rawValue
            return
        }
        guard !connectionTestRequiresPairing(
            mode: mode,
            settings: settings,
            serverURL: serverURL
        ) else {
            message = ConnectionTestResult.notPaired.rawValue
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
                guard let token = try Keychain.loadDeviceToken(), !token.isEmpty else {
                    message = ConnectionTestResult.notPaired.rawValue
                    return
                }
                _ = try await DaemonClient(baseURL: url, token: token).status()
                message = ConnectionTestResult.connected.rawValue
            } catch let error as DaemonError {
                message = switch error {
                case .unauthorized, .authenticationInvalid: ConnectionTestResult.deviceRejected.rawValue
                case .forbiddenHost: ConnectionTestResult.hostRejected.rawValue
                default: ConnectionTestResult.unreachable.rawValue
                }
            } catch {
                message = ConnectionTestResult.unreachable.rawValue
            }
        }
    }
}

@MainActor
func iconPreviewImage(palette: UllageMark.Palette) -> NSImage? {
    guard let representation = try? UllageMark.applicationIcon(
        pixelSize: 128,
        palette: palette
    ) else { return nil }
    let image = NSImage(size: NSSize(width: 64, height: 64))
    image.addRepresentation(representation)
    return image
}
