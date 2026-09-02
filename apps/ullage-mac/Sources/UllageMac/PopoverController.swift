import AppKit
import SwiftUI

/// Presents the popover content below the status item. On macOS 26 the
/// content sits in a transparent borderless panel whose root is Liquid Glass,
/// so the desktop and windows the panel covers show through it, refracted.
/// `NSPopover` cannot do that: its frame draws its own material, which blurs
/// whatever is behind the window to a flat tone before the content view can
/// sample it. macOS 14 and 15 keep the `NSPopover`.
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
    private let openSettings: () -> Void
    private var hostingController: NSHostingController<RootView>?
    private let presentation = PopoverPresentation()
    private var sizing = PopoverSizing()
    private let popover: NSPopover?
    private var panel: NSPanel?
    private var anchor = NSRect.zero
    private var dismissalMonitors: [Any] = []
    private var activationObserver: NSObjectProtocol?

    var isShown: Bool {
        if let popover { return popover.isShown }
        return panel?.isVisible ?? false
    }

    init(store: UsageStore, settings: AppSettings, openSettings: @escaping () -> Void) {
        self.store = store
        self.settings = settings
        self.openSettings = openSettings
        if #available(macOS 26.0, *) {
            popover = nil
        } else {
            let popover = NSPopover()
            popover.behavior = .transient
            self.popover = popover
        }
        super.init()
        popover?.delegate = self
    }

    func show(relativeTo rect: NSRect, of view: NSView) {
        let controller = hostingController ?? makeHostingController()
        let screen = view.window?.screen ?? NSScreen.main
        sizing.update(preferredHeight: sizing.preferredHeight, maximumHeight: maximumHeight(for: screen))
        if let popover {
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
        if let popover {
            popover.performClose(nil)
        } else {
            removeDismissalMonitors()
            panel?.orderOut(nil)
            presentation.isShown = false
        }
    }

    /// The transient popover also closes on its own; the wave must stop then too.
    func popoverDidClose(_ notification: Notification) {
        presentation.isShown = false
    }

    private func makeHostingController() -> NSHostingController<RootView> {
        let controller = NSHostingController(rootView: RootView(
            store: store,
            settings: settings,
            presentation: presentation,
            openSettings: openSettings,
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
