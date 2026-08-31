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
        guard let parsed = parse(arguments: arguments) else {
            return 2
        }

        do {
            try render(
                to: URL(fileURLWithPath: parsed.directory, isDirectory: true),
                palette: parsed.palette
            )
            return 0
        } catch {
            writeIconsetError("could not render iconset: \(error.localizedDescription)")
            return 1
        }
    }

    static func render(to directory: URL, palette: UllageMark.Palette) throws {
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        for entry in entries {
            let image = try UllageMark.applicationIcon(
                pixelSize: entry.pixelSize,
                palette: palette
            )
            guard let png = image.representation(using: .png, properties: [:]) else {
                throw IconsetError.couldNotEncodePNG(entry.filename)
            }
            try png.write(to: directory.appendingPathComponent(entry.filename), options: .atomic)
        }
    }

    private static func parse(arguments: [String]) -> (directory: String, palette: UllageMark.Palette)? {
        guard arguments.count == 1 || arguments.count == 3,
              let directory = arguments.first else {
            writeUsage()
            return nil
        }
        guard arguments.count == 3 else { return (directory, .default) }
        guard arguments[1] == "--palette",
              let palette = UllageMark.Palette(rawValue: arguments[2]) else {
            writeUsage()
            return nil
        }
        return (directory, palette)
    }

    private static func writeUsage() {
        let palettes = UllageMark.Palette.allCases.map(\.rawValue).joined(separator: ", ")
        writeIconsetError("usage: UllageMac --render-iconset <directory> [--palette <key>]")
        writeIconsetError("palette must be one of: \(palettes)")
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
