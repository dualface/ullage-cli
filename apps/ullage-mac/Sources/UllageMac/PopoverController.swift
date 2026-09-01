import AppKit
import SwiftUI

@MainActor
final class PopoverController: NSObject, NSPopoverDelegate {
    private static let screenMargin: CGFloat = 24

    private let popover = NSPopover()
    private let store: UsageStore
    private let openSettings: () -> Void
    private var hostingController: NSHostingController<RootView>?
    private var sizing = PopoverSizing()

    var isShown: Bool { popover.isShown }

    init(store: UsageStore, openSettings: @escaping () -> Void) {
        self.store = store
        self.openSettings = openSettings
        super.init()
        popover.behavior = .transient
        popover.delegate = self
    }

    func show(relativeTo rect: NSRect, of view: NSView) {
        if hostingController == nil {
            let controller = NSHostingController(rootView: RootView(
                store: store,
                openSettings: openSettings,
                onPreferredHeightChanged: { [weak self] height in self?.updateContentHeight(height) }
            ))
            hostingController = controller
            popover.contentViewController = controller
        }
        sizing.update(
            preferredHeight: sizing.preferredHeight,
            maximumHeight: maximumHeight(for: view.window?.screen ?? NSScreen.main)
        )
        popover.contentSize = sizing.contentSize
        popover.show(relativeTo: rect, of: view, preferredEdge: .minY)
        store.start()
    }

    func close() {
        popover.performClose(nil)
        store.stop()
    }

    func popoverDidClose(_ notification: Notification) {
        store.stop()
    }

    private func updateContentHeight(_ preferredHeight: CGFloat) {
        sizing.update(
            preferredHeight: preferredHeight,
            maximumHeight: maximumHeight(for: hostingController?.view.window?.screen ?? NSScreen.main)
        )
        popover.contentSize = sizing.contentSize
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
