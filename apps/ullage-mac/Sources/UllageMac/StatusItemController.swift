import AppKit
import IOKit.ps
import Observation
import ServiceManagement
import UllageKit

@MainActor
final class StatusItemController: NSObject {
    private let statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    private let store: UsageStore
    private let settings: AppSettings
    private let mode: AppMode
    private var popoverController: PopoverController?
    private var settingsController: SettingsPanelController?
    private var menuBarMode = MenuBarPresentation.initial
    private var animationState = MenuBarLiquidAnimationState()
    private var animationTimer: Timer?
    private var lastTickUptime: TimeInterval?
    private var lastMotionGate: MenuBarLiquidMotionGate?
    private var reduceMotionObserver: NSObjectProtocol?
    private var wakeObserver: NSObjectProtocol?
    private var powerSourceRunLoopSource: CFRunLoopSource?

    private enum MenuBarPresentation: Equatable {
        case initial
        case liquid([MenuBarAccountLevel])
        case noData
    }

    init(
        store: UsageStore,
        settings: AppSettings,
        mode: AppMode
    ) {
        self.store = store
        self.settings = settings
        self.mode = mode
        super.init()
        guard let button = statusItem.button else { return }
        button.image = UllageMark.menuBarImage()
        button.imagePosition = .imageOnly
        button.title = ""
        button.target = self
        button.action = #selector(handleStatusItem(_:))
        button.sendAction(on: [.leftMouseUp, .rightMouseUp])
        observeWorkspaceGates()
        startPowerSourceMonitoring()
        observeStore()
    }

    private func observeStore() {
        withObservationTracking {
            refreshPresentationFromStore()
        } onChange: { [weak self] in
            Task { @MainActor in self?.observeStore() }
        }
    }

    private func observeWorkspaceGates() {
        let center = NSWorkspace.shared.notificationCenter
        reduceMotionObserver = center.addObserver(
            forName: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor in self?.reconcileAnimationTimer() }
        }
        wakeObserver = center.addObserver(
            forName: NSWorkspace.didWakeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor in self?.reconcileAnimationTimer() }
        }
    }

    private func startPowerSourceMonitoring() {
        guard powerSourceRunLoopSource == nil else { return }
        let callback: IOPowerSourceCallbackType = { context in
            guard let context else { return }
            let controller = Unmanaged<StatusItemController>.fromOpaque(context).takeUnretainedValue()
            Task { @MainActor in
                controller.reconcileAnimationTimer()
            }
        }
        let context = Unmanaged.passUnretained(self).toOpaque()
        guard let source = IOPSNotificationCreateRunLoopSource(callback, context)?.takeRetainedValue()
        else { return }
        CFRunLoopAddSource(CFRunLoopGetMain(), source, .commonModes)
        powerSourceRunLoopSource = source
    }

    private func refreshPresentationFromStore() {
        let accounts = store.accounts
        let snapshots = store.snapshots
        let connectionState = store.connectionState
        let presentation: MenuBarPresentation
        if store.lastRefreshedAt == nil {
            presentation = .initial
        } else if connectionState.hasMenuBarData {
            let levels = menuBarAccountLevels(accounts: accounts, snapshots: snapshots)
            presentation = levels.isEmpty ? .noData : .liquid(levels)
        } else if connectionState == .loading {
            return
        } else {
            presentation = .noData
        }

        let wasLiquid: Bool
        if case .liquid = menuBarMode {
            wasLiquid = true
        } else {
            wasLiquid = false
        }
        menuBarMode = presentation
        switch presentation {
        case .initial:
            stopAnimationTimer()
            statusItem.button?.image = UllageMark.menuBarImage()
        case .noData:
            stopAnimationTimer()
            statusItem.button?.image = UllageMark.menuBarImage(fillRatio: nil)
        case .liquid(let levels):
            if !wasLiquid {
                seedAnimation(with: levels)
            }
            reconcileAnimationTimer()
        }
    }

    private func seedAnimation(with levels: [MenuBarAccountLevel]) {
        let index: Int
        if let accountID = animationState.accountID,
           let existing = levels.firstIndex(where: { $0.accountID == accountID }) {
            index = existing
        } else {
            index = 0
        }
        let level = levels[index]
        let ratio = quantizedMenuBarFillRatio(level.remainingRatio)
        animationState = MenuBarLiquidAnimationState(
            displayedRatio: ratio,
            targetRatio: ratio,
            accountIndex: index,
            accountID: level.accountID,
            displayName: level.displayName,
            wavePhase: animationState.wavePhase,
            secondsInAccount: 0
        )
    }

    private func motionGate() -> MenuBarLiquidMotionGate {
        switch menuBarMode {
        case .initial, .noData:
            return .stop
        case .liquid:
            if NSWorkspace.shared.accessibilityDisplayShouldReduceMotion || PowerSource.isOnBattery {
                return .freeze
            }
            return .animate
        }
    }

    private func reconcileAnimationTimer() {
        guard case .liquid(let levels) = menuBarMode else {
            stopAnimationTimer()
            return
        }
        let gate = motionGate()
        animationState = MenuBarLiquidAnimation.advance(
            state: animationState,
            levels: levels,
            gate: gate,
            dt: 0
        )
        applyLiquidImage(gate: gate)
        if gate == .animate {
            startAnimationTimerIfNeeded()
        } else {
            stopAnimationTimer()
        }
    }

    private func startAnimationTimerIfNeeded() {
        guard animationTimer == nil else { return }
        lastTickUptime = ProcessInfo.processInfo.systemUptime
        let interval = MenuBarLiquidAnimation.tickInterval()
        let timer = Timer(timeInterval: interval, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.tickAnimation() }
        }
        timer.tolerance = interval * 0.2
        RunLoop.main.add(timer, forMode: .common)
        animationTimer = timer
    }

    private func stopAnimationTimer() {
        animationTimer?.invalidate()
        animationTimer = nil
        lastTickUptime = nil
    }

    private func tickAnimation() {
        guard case .liquid(let levels) = menuBarMode else {
            stopAnimationTimer()
            return
        }
        let gate = motionGate()
        guard gate == .animate else {
            reconcileAnimationTimer()
            return
        }
        let now = ProcessInfo.processInfo.systemUptime
        let elapsed = lastTickUptime.map { now - $0 } ?? MenuBarLiquidAnimation.tickInterval()
        lastTickUptime = now
        let dt = min(max(elapsed, 0), 0.5)
        animationState = MenuBarLiquidAnimation.advance(
            state: animationState,
            levels: levels,
            gate: gate,
            dt: dt
        )
        applyLiquidImage(gate: gate)
    }

    private func applyLiquidImage(gate: MenuBarLiquidMotionGate) {
        lastMotionGate = gate
        statusItem.button?.image = UllageMark.menuBarImage(
            fillRatio: animationState.displayedRatio,
            wavePhase: gate == .animate ? animationState.wavePhase : 0,
            accountLabel: animationState.displayName,
            accessibilityRatio: animationState.targetRatio
        )
    }

    @objc private func handleStatusItem(_ sender: NSStatusBarButton) {
        guard let event = NSApp.currentEvent else { return }
        if event.type == .rightMouseUp || event.modifierFlags.contains(.control) {
            statusItem.menu = makeMenu()
            sender.performClick(nil)
            statusItem.menu = nil
        } else {
            togglePopover(relativeTo: sender)
        }
    }

    private func togglePopover(relativeTo button: NSStatusBarButton) {
        let controller = popoverController ?? makePopoverController()
        if controller.isShown {
            controller.close()
        } else {
            controller.show(relativeTo: button.bounds, of: button)
        }
    }

    private func makePopoverController() -> PopoverController {
        let controller = PopoverController(
            store: store,
            openSettings: { [weak self] in self?.showSettings() }
        )
        popoverController = controller
        return controller
    }

    private func makeMenu() -> NSMenu {
        let menu = NSMenu()
        menu.addItem(withTitle: "Refresh", action: #selector(refresh), keyEquivalent: "r").target = self
        menu.addItem(withTitle: "Settings…", action: #selector(showSettings), keyEquivalent: ",").target = self
        let launch = menu.addItem(
            withTitle: "Launch at Login",
            action: #selector(toggleLaunchAtLogin),
            keyEquivalent: ""
        )
        launch.target = self
        launch.isEnabled = isApplicationBundleURL(Bundle.main.bundleURL)
        launch.state = launch.isEnabled && SMAppService.mainApp.status == .enabled ? .on : .off
        if !launch.isEnabled {
            launch.toolTip = "Launch at Login requires running Ullage from an app bundle."
        }
        menu.addItem(.separator())
        menu.addItem(withTitle: "Quit Ullage", action: #selector(quit), keyEquivalent: "q").target = self
        return menu
    }

    @objc private func toggleLaunchAtLogin() {
        guard isApplicationBundleURL(Bundle.main.bundleURL) else { return }
        let service = SMAppService.mainApp
        do {
            if service.status == .enabled {
                try service.unregister()
            } else {
                try service.register()
            }
        } catch {
            let alert = NSAlert()
            alert.alertStyle = .warning
            alert.messageText = "Unable to Update Launch at Login"
            alert.informativeText = error.localizedDescription
            alert.runModal()
        }
    }

    @objc private func refresh() {
        guard let button = statusItem.button else { return }
        let controller = popoverController ?? makePopoverController()
        if !controller.isShown {
            controller.show(relativeTo: button.bounds, of: button)
        } else {
            store.refresh()
        }
    }

    @objc private func showSettings() {
        let controller = settingsController ?? SettingsPanelController(
            settings: settings,
            mode: mode,
            onSaved: { [weak self] in self?.store.invalidateDataSource() }
        )
        settingsController = controller
        controller.show()
    }

    @objc private func quit() {
        stopAnimationTimer()
        store.stop()
        NSApp.terminate(nil)
    }
}

private extension ConnectionState {
    var hasMenuBarData: Bool {
        self == .connected
    }
}

enum PowerSource {
    /// True when the machine is drawing from battery rather than AC/UPS.
    static var isOnBattery: Bool {
        guard let info = IOPSCopyPowerSourcesInfo()?.takeRetainedValue(),
              let type = IOPSGetProvidingPowerSourceType(info)?.takeUnretainedValue() as String?
        else {
            return false
        }
        return type == kIOPSBatteryPowerValue
    }
}

func isApplicationBundleURL(_ url: URL) -> Bool {
    url.pathExtension.caseInsensitiveCompare("app") == .orderedSame
}
