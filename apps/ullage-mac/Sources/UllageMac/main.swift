import AppKit
import Foundation

if ProcessInfo.processInfo.arguments.contains("--dump") {
    Task { @MainActor in
        exit(await DumpCommand.run())
    }
    dispatchMain()
} else {
    let application = NSApplication.shared
    let delegate = AppDelegate()
    application.delegate = delegate
    application.setActivationPolicy(.accessory)
    application.run()
}
