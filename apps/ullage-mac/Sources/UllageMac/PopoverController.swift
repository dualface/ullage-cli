import AppKit
import SwiftUI
import UllageKit

/// Presents the popover content below the status item. With Liquid Glass the
/// content sits in a transparent borderless panel whose root is glass, so the
/// desktop and windows the panel covers show through it, refracted. `NSPopover`
/// cannot do that: its frame draws its own material, which blurs whatever is
/// behind the window to a flat tone before the content view can sample it.
/// Without glass — macOS 14 and 15, or the option turned off — the `NSPopover`
/// is the presentation, frame and arrow included.
/// Whether the popover is on screen, for content that should only run while
/// it is visible, such as the hero's wave.
@MainActor
@Observable
final class PopoverPresentation {
    var isShown = false
    /// Where the panel's pointer aims, in points from the centre of the panel.
    /// Non-zero once a screen edge has pushed the panel off the status item.
    var pointerOffset: CGFloat = 0
}

@MainActor
final class PopoverController: NSObject, NSPopoverDelegate {
    private static let screenMargin: CGFloat = 24
    /// Clearance between the status item and the pointer's tip. The pointer
    /// itself sits inside the panel, so this is the whole visible gap.
    private static let panelGap: CGFloat = 2
    private static let panelEdgeMargin: CGFloat = 8

    private let store: UsageStore
    private let settings: AppSettings
    private let mode: AppMode
    private let openSettings: () -> Void
    private let writeAccountMetrics: @MainActor (_ account: String, _ metrics: [String]) async throws -> Account
    private var hostingController: NSHostingController<RootView>?
    private let presentation = PopoverPresentation()
    private var sizing = PopoverSizing()
    private var popover: NSPopover?
    private var panel: NSPanel?
    /// Which shell is built right now, so a change to the Liquid Glass setting
    /// tears the other one down instead of reusing it. `nil` until the first
    /// `show` builds one.
    private var presentsInGlass: Bool?
    private var anchor = NSRect.zero
    private var dismissalMonitors: [Any] = []
    private var activationObserver: NSObjectProtocol?

    var isShown: Bool {
        if let popover, popover.isShown { return true }
        return panel?.isVisible ?? false
    }

    init(
        store: UsageStore,
        settings: AppSettings,
        mode: AppMode,
        openSettings: @escaping () -> Void,
        writeAccountMetrics: @escaping @MainActor (_ account: String, _ metrics: [String]) async throws -> Account
    ) {
        self.store = store
        self.settings = settings
        self.mode = mode
        self.openSettings = openSettings
        self.writeAccountMetrics = writeAccountMetrics
        super.init()
    }

    func show(relativeTo rect: NSRect, of view: NSView) {
        // Read the setting here, not at launch: turning Liquid Glass off swaps
        // the whole shell, and the next opening is when that has to take hold.
        let inGlass = liquidGlassIsEnabled(settings: settings)
        if presentsInGlass != inGlass {
            discardShell()
            presentsInGlass = inGlass
        }
        let controller = hostingController ?? makeHostingController()
        let screen = view.window?.screen ?? NSScreen.main
        sizing.update(preferredHeight: sizing.preferredHeight, maximumHeight: maximumHeight(for: screen))
        if !inGlass {
            let popover = self.popover ?? makePopover()
            if popover.contentViewController == nil {
                popover.contentViewController = controller
            }
            popover.contentSize = sizing.contentSize
            popover.show(relativeTo: rect, of: view, preferredEdge: .minY)
        } else if let window = view.window {
            anchor = window.convertToScreen(view.convert(rect, to: nil))
            let panel = self.panel ?? makePanel(with: controller)
            let frame = panelFrame(height: sizing.height, screen: screen)
            presentation.pointerOffset = anchor.midX - frame.midX
            panel.setFrame(frame, display: true)
            panel.orderFrontRegardless()
            panel.makeKey()
            installDismissalMonitors()
        }
        presentation.isShown = true
        // The store polls on its own; opening still asks for fresh data so the
        // popover and the menu "Refresh" action do not wait for the next poll.
        store.refresh()
    }

    func close() {
        // Whichever shell is up: `performClose` on a popover that is not shown
        // and `orderOut` on a panel that is not visible are both no-ops.
        popover?.performClose(nil)
        removeDismissalMonitors()
        panel?.orderOut(nil)
        presentation.isShown = false
    }

    /// Drop both shells and the view that was hosted in one of them, so the
    /// next `show` builds what the setting now asks for. The hosting controller
    /// goes too: it belongs to one container at a time, and the two shells do
    /// not agree on height either — the glass panel carries the pointer inside
    /// its own frame — so the sizing starts over with them.
    private func discardShell() {
        close()
        popover?.contentViewController = nil
        popover?.delegate = nil
        popover = nil
        panel?.contentViewController = nil
        panel = nil
        hostingController = nil
        sizing = PopoverSizing()
    }

    private func makePopover() -> NSPopover {
        let popover = NSPopover()
        popover.behavior = .transient
        popover.delegate = self
        self.popover = popover
        return popover
    }

    /// The transient popover also closes on its own; the wave must stop then too.
    func popoverDidClose(_ notification: Notification) {
        presentation.isShown = false
    }

    private func makeHostingController() -> NSHostingController<RootView> {
        let controller = NSHostingController(rootView: RootView(
            store: store,
            settings: settings,
            mode: mode,
            presentation: presentation,
            openSettings: openSettings,
            writeAccountMetrics: writeAccountMetrics,
            onPreferredHeightChanged: { [weak self] height in self?.updateContentHeight(height) }
        ))
        hostingController = controller
        return controller
    }

    private func makePanel(with controller: NSHostingController<RootView>) -> NSPanel {
        let panel = NSPanel(
            contentRect: NSRect(origin: .zero, size: sizing.contentSize),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.level = .popUpMenu
        panel.collectionBehavior = [.canJoinAllSpaces, .transient, .ignoresCycle]
        panel.isReleasedWhenClosed = false
        panel.hidesOnDeactivate = false
        panel.animationBehavior = .utilityWindow
        panel.contentViewController = controller
        self.panel = panel
        return panel
    }

    /// Centered under the status item, kept inside the screen, hanging from
    /// just below the menu bar so it grows downward as the content changes.
    private func panelFrame(height: CGFloat, screen: NSScreen?) -> NSRect {
        let size = NSSize(width: sizing.contentSize.width, height: height)
        var origin = NSPoint(x: anchor.midX - size.width / 2, y: anchor.minY - Self.panelGap - size.height)
        if let bounds = screen?.visibleFrame {
            let minX = bounds.minX + Self.panelEdgeMargin
            let maxX = bounds.maxX - Self.panelEdgeMargin - size.width
            origin.x = min(max(origin.x, minX), max(minX, maxX))
            origin.y = max(origin.y, bounds.minY + Self.panelEdgeMargin)
        }
        return NSRect(origin: origin, size: size)
    }

    /// The panel is not an `NSPopover`, so transient dismissal is rebuilt: a
    /// click anywhere outside it, Escape, or another application coming to
    /// the front closes it. Clicks on the status item are left to the item's
    /// own action, which toggles the panel; closing here as well would make
    /// that toggle reopen it.
    private func installDismissalMonitors() {
        removeDismissalMonitors()
        let clicks: NSEvent.EventTypeMask = [.leftMouseDown, .rightMouseDown, .otherMouseDown]
        if let monitor = NSEvent.addGlobalMonitorForEvents(matching: clicks, handler: { [weak self] _ in
            Task { @MainActor in self?.closeUnlessOnAnchor() }
        }) {
            dismissalMonitors.append(monitor)
        }
        if let monitor = NSEvent.addLocalMonitorForEvents(matching: clicks.union(.keyDown), handler: { [weak self] event in
            guard let self else { return event }
            if event.type == .keyDown {
                guard event.keyCode == 53 else { return event }
                close()
                return nil
            }
            if event.window !== panel {
                closeUnlessOnAnchor()
            }
            return event
        }) {
            dismissalMonitors.append(monitor)
        }
        activationObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didActivateApplicationNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor in self?.close() }
        }
    }

    private func closeUnlessOnAnchor() {
        guard !anchor.contains(NSEvent.mouseLocation) else { return }
        close()
    }

    private func removeDismissalMonitors() {
        for monitor in dismissalMonitors {
            NSEvent.removeMonitor(monitor)
        }
        dismissalMonitors.removeAll()
        if let activationObserver {
            NSWorkspace.shared.notificationCenter.removeObserver(activationObserver)
            self.activationObserver = nil
        }
    }

    private func updateContentHeight(_ preferredHeight: CGFloat) {
        let screen = hostingController?.view.window?.screen ?? NSScreen.main
        sizing.update(preferredHeight: preferredHeight, maximumHeight: maximumHeight(for: screen))
        if let popover {
            popover.contentSize = sizing.contentSize
        } else if let panel, panel.isVisible {
            // Only when it actually moved: setting the same frame still makes
            // the panel redraw, and redrawing a glass root shows as a flicker.
            let frame = panelFrame(height: sizing.height, screen: screen)
            if frame != panel.frame {
                panel.setFrame(frame, display: true, animate: false)
            }
        }
    }

    private func maximumHeight(for screen: NSScreen?) -> CGFloat {
        guard let screen else { return 520 }
        return max(180, screen.visibleFrame.height - Self.screenMargin)
    }
}

struct PopoverSizing {
    private(set) var preferredHeight: CGFloat = 260
    private(set) var height: CGFloat = 260

    var contentSize: NSSize { NSSize(width: 360, height: height) }

    mutating func update(preferredHeight: CGFloat, maximumHeight: CGFloat) {
        self.preferredHeight = preferredHeight
        height = max(180, min(maximumHeight, preferredHeight))
    }
}
