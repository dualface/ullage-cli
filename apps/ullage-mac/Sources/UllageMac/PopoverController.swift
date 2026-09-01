import AppKit
import SwiftUI

@MainActor
final class PopoverController: NSObject, NSPopoverDelegate {
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
        sizing.update(preferredHeight: preferredHeight)
        popover.contentSize = sizing.contentSize
    }
}

struct PopoverSizing {
    private(set) var height: CGFloat = 260

    var contentSize: NSSize { NSSize(width: 360, height: height) }

    mutating func update(preferredHeight: CGFloat) {
        height = min(520, max(180, preferredHeight))
    }
}
