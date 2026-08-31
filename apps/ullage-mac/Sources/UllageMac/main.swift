import AppKit
import Foundation

let arguments = Array(ProcessInfo.processInfo.arguments.dropFirst())
if arguments.first == "--render-iconset" {
    exit(IconsetCommand.run(arguments: Array(arguments.dropFirst())))
} else if arguments.contains("--dump") {
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
