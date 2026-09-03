import Darwin
import Dispatch
import Foundation

private let appGroupIdentifier = "group.com.ullage.mac"

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("Ullage local service failed: \(message)\n".utf8))
    exit(EXIT_FAILURE)
}

func ensurePrivateDirectory(_ url: URL) throws {
    var metadata = stat()
    if lstat(url.path, &metadata) == 0 {
        guard metadata.st_mode & S_IFMT == S_IFDIR, metadata.st_uid == geteuid() else {
            throw HelperError.invalidDirectory
        }
        guard metadata.st_mode & 0o077 == 0 else { throw HelperError.invalidDirectory }
        return
    }
    guard errno == ENOENT else { throw HelperError.system(errno) }
    guard mkdir(url.path, 0o700) == 0 else { throw HelperError.system(errno) }
}

func createInitialConfiguration(at url: URL) throws {
    let descriptor = open(url.path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0o600)
    if descriptor < 0 {
        guard errno == EEXIST else { throw HelperError.system(errno) }
        var metadata = stat()
        guard lstat(url.path, &metadata) == 0,
              metadata.st_mode & S_IFMT == S_IFREG,
              metadata.st_uid == geteuid(),
              metadata.st_mode & 0o077 == 0 else {
            throw HelperError.invalidConfiguration
        }
        return
    }
    defer { close(descriptor) }
    let configuration = Data("{\"version\":1,\"http\":{\"enabled\":false}}\n".utf8)
    try configuration.withUnsafeBytes { bytes in
        var offset = 0
        while offset < configuration.count {
            let written = Darwin.write(
                descriptor,
                bytes.baseAddress!.advanced(by: offset),
                configuration.count - offset
            )
            if written < 0 {
                if errno == EINTR { continue }
                throw HelperError.system(errno)
            }
            offset += written
        }
    }
    guard fsync(descriptor) == 0 else { throw HelperError.system(errno) }
}

enum HelperError: Error {
    case invalidDirectory
    case invalidConfiguration
    case system(Int32)
}

guard let container = FileManager.default.containerURL(
    forSecurityApplicationGroupIdentifier: appGroupIdentifier
) else {
    fail("App Group container is unavailable")
}

let dataDirectory = container.appendingPathComponent("data", isDirectory: true)
let runDirectory = container.appendingPathComponent("run", isDirectory: true)
do {
    try ensurePrivateDirectory(dataDirectory)
    try ensurePrivateDirectory(runDirectory)
    try createInitialConfiguration(at: dataDirectory.appendingPathComponent("config.json"))
} catch {
    fail("could not prepare private storage")
}

guard let daemonURL = Bundle.main.resourceURL?.appendingPathComponent("ullage-daemon") else {
    fail("embedded daemon is missing")
}
var daemonMetadata = stat()
guard lstat(daemonURL.path, &daemonMetadata) == 0,
      daemonMetadata.st_mode & S_IFMT == S_IFREG,
      daemonMetadata.st_mode & 0o111 != 0 else {
    fail("embedded daemon is not executable")
}

let environment = [
    "ULLAGE_CONFIG_FILE": dataDirectory.appendingPathComponent("config.json").path,
    "ULLAGE_STATE_FILE": dataDirectory.appendingPathComponent("state.json").path,
    "ULLAGE_CONTROL_SOCKET": runDirectory.appendingPathComponent("control.sock").path,
]
let daemon = Process()
daemon.executableURL = daemonURL
daemon.arguments = ["__daemon"]
daemon.environment = ProcessInfo.processInfo.environment.merging(environment) { _, path in path }

let forwardedSignals = [SIGTERM, SIGINT, SIGHUP]
let signalSources = forwardedSignals.map { number in
    signal(number, SIG_IGN)
    let source = DispatchSource.makeSignalSource(signal: number, queue: .global())
    source.setEventHandler {
        let processIdentifier = daemon.processIdentifier
        if processIdentifier > 0 { kill(processIdentifier, number) }
    }
    source.resume()
    return source
}

do {
    try daemon.run()
} catch {
    fail("could not launch embedded daemon")
}
daemon.waitUntilExit()
signalSources.forEach { $0.cancel() }
exit(daemon.terminationStatus)
