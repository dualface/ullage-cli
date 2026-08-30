import AppKit
import ServiceManagement

@MainActor
final class StatusItemController: NSObject {
    private let statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    private let store: UsageStore
    private let settings: AppSettings
    private let mode: AppMode
    private var popoverController: PopoverController?
    private var settingsController: SettingsPanelController?

    init(store: UsageStore, settings: AppSettings, mode: AppMode) {
        self.store = store
        self.settings = settings
        self.mode = mode
        super.init()
        guard let button = statusItem.button else { return }
        let image = NSImage(systemSymbolName: "gauge.with.dots.needle.67percent", accessibilityDescription: "Ullage")
        image?.isTemplate = true
        button.image = image
        button.imagePosition = .imageOnly
        button.title = ""
        button.target = self
        button.action = #selector(handleStatusItem(_:))
        button.sendAction(on: [.leftMouseUp, .rightMouseUp])
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
        store.stop()
        NSApp.terminate(nil)
    }
}

func isApplicationBundleURL(_ url: URL) -> Bool {
    url.pathExtension.caseInsensitiveCompare("app") == .orderedSame
}
