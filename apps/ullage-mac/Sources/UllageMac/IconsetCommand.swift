import AppKit
import Foundation

struct IconsetEntry: Equatable {
    let filename: String
    let pixelSize: Int
}

@MainActor
enum IconsetCommand {
    static let entries = [
        IconsetEntry(filename: "icon_16x16.png", pixelSize: 16),
        IconsetEntry(filename: "icon_16x16@2x.png", pixelSize: 32),
        IconsetEntry(filename: "icon_32x32.png", pixelSize: 32),
        IconsetEntry(filename: "icon_32x32@2x.png", pixelSize: 64),
        IconsetEntry(filename: "icon_128x128.png", pixelSize: 128),
        IconsetEntry(filename: "icon_128x128@2x.png", pixelSize: 256),
        IconsetEntry(filename: "icon_256x256.png", pixelSize: 256),
        IconsetEntry(filename: "icon_256x256@2x.png", pixelSize: 512),
        IconsetEntry(filename: "icon_512x512.png", pixelSize: 512),
        IconsetEntry(filename: "icon_512x512@2x.png", pixelSize: 1024),
    ]

    static func run(arguments: [String]) -> Int32 {
        guard arguments.count == 1 else {
            writeIconsetError("usage: UllageMac --render-iconset <directory>")
            return 2
        }

        do {
            try render(to: URL(fileURLWithPath: arguments[0], isDirectory: true))
            return 0
        } catch {
            writeIconsetError("could not render iconset: \(error.localizedDescription)")
            return 1
        }
    }

    static func render(to directory: URL) throws {
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        for entry in entries {
            let image = try UllageMark.applicationIcon(pixelSize: entry.pixelSize)
            guard let png = image.representation(using: .png, properties: [:]) else {
                throw IconsetError.couldNotEncodePNG(entry.filename)
            }
            try png.write(to: directory.appendingPathComponent(entry.filename), options: .atomic)
        }
    }
}

private enum IconsetError: LocalizedError {
    case couldNotEncodePNG(String)

    var errorDescription: String? {
        switch self {
        case .couldNotEncodePNG(let filename):
            return "could not encode \(filename) as PNG"
        }
    }
}

private func writeIconsetError(_ message: String) {
    FileHandle.standardError.write(Data((message + "\n").utf8))
}
