import AppKit
import SwiftUI

@MainActor
final class PopoverController: NSObject, NSPopoverDelegate {
    private let popover = NSPopover()
    private let store: UsageStore
    private let openSettings: () -> Void
    private var hostingController: NSHostingController<RootView>?

    var isShown: Bool { popover.isShown }

    init(store: UsageStore, openSettings: @escaping () -> Void) {
        self.store = store
        self.openSettings = openSettings
        super.init()
        popover.behavior = .transient
        popover.delegate = self
        store.onContentChanged = { [weak self] in self?.updateContentSize() }
    }

    func show(relativeTo rect: NSRect, of view: NSView) {
        if hostingController == nil {
            let controller = NSHostingController(rootView: RootView(store: store, openSettings: openSettings))
            hostingController = controller
            popover.contentViewController = controller
        }
        updateContentSize()
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

    private func updateContentSize() {
        let rows = max(store.accounts.count, 1)
        let height = min(520, max(260, 125 + rows * 115))
        popover.contentSize = NSSize(width: 360, height: height)
    }
}
