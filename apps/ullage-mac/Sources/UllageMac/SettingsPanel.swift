import AppKit
import SwiftUI
import UllageKit

@MainActor
final class SettingsPanelController: NSWindowController {
    init(
        settings: AppSettings,
        store: UsageStore,
        localService: LocalServiceManager? = nil,
        mode: AppMode,
        onSaved: @escaping () -> Void,
        onPaletteChanged: @escaping (AppPalette) -> Void
    ) {
        let localService = localService ?? LocalServiceManager(settings: settings)
        let view = SettingsView(
            settings: settings,
            store: store,
            localService: localService,
            mode: mode,
            onSaved: onSaved,
            onPaletteChanged: onPaletteChanged
        )
        let hostingController = NSHostingController(rootView: view)
        // Let the panel follow the view as the pairing section locks/unlocks.
        hostingController.sizingOptions = [.preferredContentSize]
        let panel = NSPanel(contentViewController: hostingController)
        panel.title = "Ullage Settings"
        // The content view carries the Settings surface through the transparent
        // title bar. SwiftUI content still observes the title-bar safe area;
        // only the surface background extends underneath it.
        panel.styleMask = [.titled, .closable, .fullSizeContentView]
        panel.titlebarAppearsTransparent = true
        panel.titleVisibility = .visible
        panel.titlebarSeparatorStyle = .none
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.isReleasedWhenClosed = false
        super.init(window: panel)
        panel.setContentSize(hostingController.view.fittingSize)
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

/// Keeps only uppercase alphanumeric characters, capped at six.
func normalizePairCodeInput(_ raw: String, maxLength: Int = 6) -> String {
    String(
        raw.uppercased()
            .unicodeScalars
            .filter { CharacterSet.alphanumerics.contains($0) }
            .prefix(maxLength)
    )
}

/// Formats a normalized six-character code as `XXX-XXX`.
func formattedPairCode(_ normalized: String) -> String? {
    guard normalized.count == 6 else { return nil }
    let characters = Array(normalized)
    return "\(String(characters[0..<3]))-\(String(characters[3..<6]))"
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

/// How the Connection and Pairing sections present themselves.
enum PairingEditingState: Equatable {
    /// Paired and locked: the server URL is read-only and the pair code is hidden.
    case locked
    /// Paired but unlocked: the URL is editable and a new code may be entered.
    case unlockedForRepair
    /// Never paired: editable, with no lock control to show.
    case unpaired
}

func pairingEditingState(isPaired: Bool, isUnlocked: Bool) -> PairingEditingState {
    guard isPaired else { return .unpaired }
    return isUnlocked ? .unlockedForRepair : .locked
}

@MainActor
func connectionTestRequiresPairing(
    mode: AppMode,
    settings: AppSettings,
    serverURL: String
) -> Bool {
    mode == .daemon && settings.pairedServerURL(matching: serverURL) == nil
}

struct SettingsView: View {
    @Bindable var settings: AppSettings
    let store: UsageStore
    @Bindable var localService: LocalServiceManager
    let mode: AppMode
    let onSaved: () -> Void
    let onPaletteChanged: (AppPalette) -> Void
    @State private var serverURL: String
    @State private var digits = Array(repeating: "", count: 6)
    @FocusState private var focusedDigit: Int?
    @State private var message = ""
    @State private var isPairing = false
    @State private var isTesting = false
    @State private var accountManager: LocalAccountManager
    /// Unlock lasts for this panel only; it is never persisted.
    @State private var isUnlocked = false

    init(
        settings: AppSettings,
        store: UsageStore,
        localService: LocalServiceManager,
        mode: AppMode,
        onSaved: @escaping () -> Void,
        onPaletteChanged: @escaping (AppPalette) -> Void
    ) {
        self.settings = settings
        self.store = store
        self.localService = localService
        self.mode = mode
        self.onSaved = onSaved
        self.onPaletteChanged = onPaletteChanged
        _serverURL = State(initialValue: settings.serverURL.absoluteString)
        _accountManager = State(initialValue: LocalAccountManager(
            localService: localService,
            dataChanged: onSaved
        ))
    }

    private var normalizedPairCode: String {
        digits.joined()
    }

    private var canPair: Bool {
        !isPairing && formattedPairCode(normalizedPairCode) != nil
    }

    private var isPaired: Bool {
        settings.pairedDeviceName != nil && settings.pairedAt != nil
    }

    private var editingState: PairingEditingState {
        pairingEditingState(isPaired: isPaired, isUnlocked: isUnlocked)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 8) {
                card { transportSection }
                if settings.transportMode == .local {
                    card { localServiceSection }
                    card { LocalAccountsSection(manager: accountManager) }
                } else {
                    card { connectionSection }
                    card { pairingSection }
                }
                card { statusSection }
                card { menuBarSection }
                card { appearanceSection }
            }
            .padding(12)
        }
        .frame(width: 420, height: 560)
        // The same surface the popover wears, minus the pointer: this window
        // has nothing to point at. The backdrop is held still here — the window
        // outlives its own visibility, and a timeline behind a closed window
        // would keep ticking.
        .modifier(PopoverSurface(placement: .window))
        .environment(\.usesLiquidGlass, liquidGlassIsEnabled(settings: settings))
        .environment(\.appPalette, settings.iconPalette)
        .task(id: settings.transportMode) {
            if settings.transportMode == .local { await accountManager.refresh() }
        }
    }

    private var transportSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Data Source").font(.headline)
            Picker("Mode", selection: Binding(
                get: { settings.transportMode },
                set: { mode in
                    settings.transportMode = mode
                    message = ""
                    onSaved()
                }
            )) {
                ForEach(ConnectionTransport.allCases) { transport in
                    Text(transport.title).tag(transport)
                }
            }
            .pickerStyle(.segmented)
            Text(settings.transportMode == .local
                 ? "Local connects privately to the embedded service in this app."
                 : "Remote connects to an already paired Ullage daemon over HTTP.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private var localServiceSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Local Service").font(.headline)
            Toggle("Enable Local Service", isOn: Binding(
                get: { settings.backgroundServiceEnabled },
                set: { enabled in
                    Task {
                        await localService.setEnabled(enabled)
                        onSaved()
                        if enabled { await accountManager.refresh() }
                    }
                }
            ))
            .disabled(localService.state == .updating || localService.state == .unavailable)
            Text(localService.state.detail)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if settings.backgroundServiceEnabled {
                Button("Restart Local Service") {
                    Task {
                        await localService.restart()
                        onSaved()
                        await accountManager.refresh()
                    }
                }
                .disabled(localService.state == .updating)
            }
            if !settings.localServiceChoiceMade {
                Text("The service is never enabled until you turn on this switch.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// One section, on the same frosted panel the popover's cards use.
    private func card<Content: View>(@ViewBuilder _ content: () -> Content) -> some View {
        content()
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .glassPanel()
    }

    private var connectionSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Connection")
                .font(.headline)
            HStack(spacing: 8) {
                TextField("Server URL", text: $serverURL)
                    .textFieldStyle(.roundedBorder)
                    .disabled(isPairing || editingState == .locked)
                switch editingState {
                case .locked:
                    // No focus ring: the button takes first responder as the
                    // window opens, and a ring around it reads as a warning
                    // rather than as a place the keyboard happens to be.
                    Button("Unlock") { unlock() }
                        .focusEffectDisabled()
                case .unlockedForRepair:
                    Button("Lock") { lock() }
                        .disabled(isPairing)
                        .focusEffectDisabled()
                case .unpaired:
                    EmptyView()
                }
            }
            Text("Use HTTP with localhost or a literal loopback, tailnet, or private LAN address.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var pairingSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Pairing")
                .font(.headline)
            if editingState != .locked {
                HStack(spacing: 8) {
                    pairDigitGroup(indices: 0..<3)
                    Text("-")
                        .font(.title2.monospacedDigit().weight(.medium))
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    pairDigitGroup(indices: 3..<6)
                    Button("Pair") { pair() }
                        .disabled(!canPair)
                    if isPairing {
                        ProgressView().controlSize(.small)
                    }
                }
                .accessibilityElement(children: .contain)
                .accessibilityLabel("Pair code")
            }
            if let name = settings.pairedDeviceName, let pairedAt = settings.pairedAt {
                Text("Paired as \(name) on \(pairedAt.formatted(date: .abbreviated, time: .shortened))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Text(pairingCaption)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var pairingCaption: String {
        switch editingState {
        case .locked:
            "Unlock to change the server URL or pair again."
        case .unlockedForRepair:
            "Run ullage device pair on the daemon host, then enter the new one-use code to replace this pairing."
        case .unpaired:
            "Run ullage device pair on the daemon host, then enter the one-use code here."
        }
    }

    private var statusSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Status")
                .font(.headline)
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Button("Test connection") { testConnection() }
                    .disabled(isTesting)
                if isTesting {
                    ProgressView().controlSize(.small)
                }
                Text(message)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
            }
        }
    }

    private var appearanceSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Appearance")
                .font(.headline)
            HStack(alignment: .top, spacing: 12) {
                VStack(alignment: .leading, spacing: 8) {
                    Picker("Icon palette", selection: iconPaletteSelection) {
                        ForEach(AppPalette.allCases) { palette in
                            Text(palette.displayName).tag(palette)
                        }
                    }
                    if #available(macOS 26.0, *) {
                        Toggle("Liquid Glass", isOn: $settings.usesLiquidGlass)
                        Text("Off uses the flat palette surface the next time the popover opens.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                Spacer(minLength: 0)
                if let preview = try? applicationIconImage(
                    palette: settings.iconPalette,
                    pixelSize: 128,
                    pointSize: 56
                ) {
                    Image(nsImage: preview)
                        .resizable()
                        .frame(width: 56, height: 56)
                        .accessibilityLabel(
                            "\(settings.iconPalette.displayName) icon preview"
                        )
                }
            }
        }
    }

    private var iconPaletteSelection: Binding<AppPalette> {
        Binding(
            get: { settings.iconPalette },
            set: { palette in
                selectIconPalette(
                    palette,
                    settings: settings,
                    onPaletteChanged: onPaletteChanged
                )
            }
        )
    }

    private var menuBarSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Menu Bar")
                .font(.headline)
            Toggle("Animate liquid", isOn: $settings.animatesMenuBarLiquid)
            Text("Off keeps the liquid at the tracked level with a flat surface.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Picker("Liquid tracks", selection: $settings.menuBarMetricID) {
                Text("Lowest remaining across accounts").tag(String?.none)
                ForEach(liquidMetricOptions) { option in
                    Text(option.title).tag(String?.some(option.id))
                }
            }
            Text(liquidMetricCaption)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// Current Overview rows, plus a placeholder for a pinned row that is not
    /// available right now so the picker never shows an empty selection.
    private var liquidMetricOptions: [MenuBarMetricOption] {
        var options = menuBarMetricOptions(
            accounts: store.accounts,
            snapshots: store.snapshots,
            hiddenOverviewItemIDs: settings.hiddenOverviewItemIDs,
            shownOverviewItemIDs: settings.shownOverviewItemIDs
        )
        if let pinned = settings.menuBarMetricID, !options.contains(where: { $0.id == pinned }) {
            options.append(MenuBarMetricOption(
                id: pinned,
                accountID: "",
                title: "Pinned metric (currently unavailable)",
                remainingRatio: 0,
                resetsAt: nil
            ))
        }
        return options
    }

    private var liquidMetricCaption: String {
        if let pinned = settings.menuBarMetricID,
           !menuBarMetricOptions(
                accounts: store.accounts,
                snapshots: store.snapshots,
                hiddenOverviewItemIDs: settings.hiddenOverviewItemIDs,
                shownOverviewItemIDs: settings.shownOverviewItemIDs
           )
               .contains(where: { $0.id == pinned }) {
            return "The pinned row is missing from the latest refresh; the liquid falls back to the lowest remaining until it returns."
        }
        return "Pin one Overview row to the menu bar, or leave it on the lowest remaining."
    }

    private func pairDigitGroup(indices: Range<Int>) -> some View {
        HStack(spacing: 6) {
            ForEach(Array(indices), id: \.self) { index in
                pairDigitField(index: index)
            }
        }
    }

    private func pairDigitField(index: Int) -> some View {
        TextField("", text: digitBinding(at: index))
            .textFieldStyle(.roundedBorder)
            .multilineTextAlignment(.center)
            .font(.body.monospaced())
            .frame(width: 34)
            .focused($focusedDigit, equals: index)
            .disabled(isPairing)
            .accessibilityLabel("Pair code digit \(index + 1)")
            .onKeyPress(.delete) {
                handleDelete(at: index)
            }
            .onChange(of: digits[index]) { _, newValue in
                handleDigitChange(at: index, newValue: newValue)
            }
    }

    private func digitBinding(at index: Int) -> Binding<String> {
        Binding(
            get: { digits[index] },
            set: { digits[index] = $0 }
        )
    }

    private func handleDigitChange(at index: Int, newValue: String) {
        let normalized = normalizePairCodeInput(newValue)
        if normalized.count > 1 {
            applyPaste(normalized)
            return
        }
        if digits[index] != normalized {
            digits[index] = normalized
        }
        if !normalized.isEmpty, index < 5 {
            focusedDigit = index + 1
        }
    }

    private func applyPaste(_ normalized: String) {
        let characters = Array(normalized)
        for offset in 0..<6 {
            digits[offset] = offset < characters.count ? String(characters[offset]) : ""
        }
        focusedDigit = min(characters.count, 5)
    }

    private func handleDelete(at index: Int) -> KeyPress.Result {
        if !digits[index].isEmpty {
            return .ignored
        }
        guard index > 0 else { return .ignored }
        digits[index - 1] = ""
        focusedDigit = index - 1
        return .handled
    }

    private func clearPairCode() {
        digits = Array(repeating: "", count: 6)
        focusedDigit = 0
    }

    private func unlock() {
        isUnlocked = true
        message = ""
        focusedDigit = 0
    }

    /// Discard unsaved edits and return to the read-only paired view.
    private func lock() {
        serverURL = settings.serverURL.absoluteString
        digits = Array(repeating: "", count: 6)
        focusedDigit = nil
        message = ""
        isUnlocked = false
    }

    private func pair() {
        guard let target = PairingServerTarget(serverURL) else {
            message = ConnectionTestResult.hostRejected.rawValue
            return
        }
        guard let pairCode = formattedPairCode(normalizedPairCode) else {
            message = "enter all six pair-code characters"
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
                clearPairCode()
                focusedDigit = nil
                isUnlocked = false
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
        if settings.transportMode == .local {
            isTesting = true
            Task {
                defer { isTesting = false }
                do {
                    guard let socketURL = localService.socketURL else {
                        throw LocalControlError.unavailable("App Group container is unavailable")
                    }
                    _ = try await LocalControlClient(socketURL: socketURL).status()
                    message = ConnectionTestResult.connected.rawValue
                } catch {
                    message = ConnectionTestResult.unreachable.rawValue
                }
            }
            return
        }
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
func selectIconPalette(
    _ palette: AppPalette,
    settings: AppSettings,
    onPaletteChanged: (AppPalette) -> Void
) {
    settings.iconPalette = palette
    onPaletteChanged(palette)
}
