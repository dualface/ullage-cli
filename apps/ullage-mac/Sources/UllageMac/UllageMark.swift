import AppKit
import CoreGraphics

@MainActor
enum UllageMark {
    private enum Style {
        case applicationIcon
        case template
    }

    private static let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!

    static func menuBarImage() -> NSImage {
        let size = NSSize(width: 18, height: 18)
        let image = NSImage(size: size, flipped: false) { rect in
            guard let context = NSGraphicsContext.current?.cgContext else { return false }
            draw(in: context, canvasSize: rect.width, style: .template)
            return true
        }
        image.accessibilityDescription = "Ullage"
        image.isTemplate = true
        return image
    }

    static func applicationIcon(pixelSize: Int) throws -> NSBitmapImageRep {
        guard pixelSize > 0 else { throw UllageMarkError.invalidPixelSize }
        guard let representation = NSBitmapImageRep(
            bitmapDataPlanes: nil,
            pixelsWide: pixelSize,
            pixelsHigh: pixelSize,
            bitsPerSample: 8,
            samplesPerPixel: 4,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: .deviceRGB,
            bytesPerRow: 0,
            bitsPerPixel: 0
        ) else {
            throw UllageMarkError.couldNotCreateBitmap
        }
        representation.size = NSSize(width: pixelSize, height: pixelSize)

        guard let graphicsContext = NSGraphicsContext(bitmapImageRep: representation) else {
            throw UllageMarkError.couldNotCreateBitmap
        }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = graphicsContext
        let context = graphicsContext.cgContext
        context.clear(CGRect(x: 0, y: 0, width: pixelSize, height: pixelSize))
        draw(in: context, canvasSize: CGFloat(pixelSize), style: .applicationIcon)
        graphicsContext.flushGraphics()
        NSGraphicsContext.restoreGraphicsState()
        return representation
    }

    private static func draw(in context: CGContext, canvasSize: CGFloat, style: Style) {
        context.saveGState()
        defer { context.restoreGState() }
        context.setShouldAntialias(true)
        context.translateBy(x: 0, y: canvasSize)
        context.scaleBy(x: 1, y: -1)

        let markOrigin: CGFloat
        let markScale: CGFloat
        switch style {
        case .applicationIcon:
            let backgroundOrigin = canvasSize * 100 / 1024
            let backgroundSide = canvasSize * 824 / 1024
            let cornerRadius = canvasSize * 186 / 1024
            drawBackground(
                in: context,
                rect: CGRect(
                    x: backgroundOrigin,
                    y: backgroundOrigin,
                    width: backgroundSide,
                    height: backgroundSide
                ),
                cornerRadius: cornerRadius
            )
            markOrigin = backgroundOrigin
            markScale = backgroundSide / 100
        case .template:
            markOrigin = 0
            markScale = canvasSize / 100
        }

        context.saveGState()
        context.translateBy(x: markOrigin, y: markOrigin)
        context.scaleBy(x: markScale, y: markScale)
        drawVessel(in: context, style: style)
        context.restoreGState()
    }

    private static func drawBackground(
        in context: CGContext,
        rect: CGRect,
        cornerRadius: CGFloat
    ) {
        let path = CGPath(
            roundedRect: rect,
            cornerWidth: cornerRadius,
            cornerHeight: cornerRadius,
            transform: nil
        )
        context.saveGState()
        context.addPath(path)
        context.clip()

        let center = CGPoint(x: rect.midX, y: rect.minY + rect.height * 0.28)
        let farthestCorner = CGPoint(x: rect.maxX, y: rect.maxY)
        let radius = hypot(farthestCorner.x - center.x, farthestCorner.y - center.y)
        let gradient = CGGradient(
            colorsSpace: colorSpace,
            colors: [color(0x1B5044), color(0x0B231E)] as CFArray,
            locations: [0, 1]
        )!
        context.drawRadialGradient(
            gradient,
            startCenter: center,
            startRadius: 0,
            endCenter: center,
            endRadius: radius,
            options: [.drawsAfterEndLocation]
        )
        context.restoreGState()
    }

    private static func drawVessel(in context: CGContext, style: Style) {
        let vessel = CGMutablePath()
        vessel.move(to: CGPoint(x: 26, y: 16))
        vessel.addLine(to: CGPoint(x: 26, y: 60))
        vessel.addArc(
            center: CGPoint(x: 50, y: 60),
            radius: 24,
            startAngle: .pi,
            endAngle: 0,
            clockwise: true
        )
        vessel.addLine(to: CGPoint(x: 74, y: 16))

        let liquid = CGMutablePath()
        liquid.move(to: CGPoint(x: 31, y: 52))
        liquid.addLine(to: CGPoint(x: 31, y: 60))
        liquid.addArc(
            center: CGPoint(x: 50, y: 60),
            radius: 19,
            startAngle: .pi,
            endAngle: 0,
            clockwise: true
        )
        liquid.addLine(to: CGPoint(x: 69, y: 52))
        liquid.closeSubpath()

        switch style {
        case .applicationIcon:
            context.saveGState()
            context.addPath(liquid)
            context.clip()
            let gradient = CGGradient(
                colorsSpace: colorSpace,
                colors: [color(0xF2B34A), color(0xC47F1F)] as CFArray,
                locations: [0, 1]
            )!
            context.drawLinearGradient(
                gradient,
                start: CGPoint(x: 50, y: 52),
                end: CGPoint(x: 50, y: 79),
                options: [.drawsBeforeStartLocation, .drawsAfterEndLocation]
            )
            context.restoreGState()
            context.setStrokeColor(color(0xE9F2EC))
            context.setLineWidth(10)
        case .template:
            context.addPath(liquid)
            context.setFillColor(NSColor.black.cgColor)
            context.fillPath()
            context.setStrokeColor(NSColor.black.cgColor)
            context.setLineWidth(12)
        }

        context.setLineCap(.butt)
        context.addPath(vessel)
        context.strokePath()
    }

    private static func color(_ hexadecimal: UInt32) -> CGColor {
        let red = CGFloat((hexadecimal >> 16) & 0xFF) / 255
        let green = CGFloat((hexadecimal >> 8) & 0xFF) / 255
        let blue = CGFloat(hexadecimal & 0xFF) / 255
        return CGColor(colorSpace: colorSpace, components: [red, green, blue, 1])!
    }
}

enum UllageMarkError: Error {
    case invalidPixelSize
    case couldNotCreateBitmap
}
