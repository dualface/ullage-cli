import AppKit
import CoreGraphics
import UllageKit

@MainActor
enum UllageMark {
    enum Palette: String, CaseIterable, Identifiable {
        case amber
        case oxblood
        case propellant
        case copper
        case paper
        case plum

        static let `default`: Self = .oxblood

        var id: Self { self }

        var displayName: String {
            rawValue.prefix(1).uppercased() + rawValue.dropFirst()
        }

        var groundInner: UInt32 {
            switch self {
            case .amber: 0x1B5044
            case .oxblood: 0x4A1020
            case .propellant: 0x2A2E35
            case .copper: 0x123C40
            case .paper: 0xF4ECDC
            case .plum: 0x3A1F4A
            }
        }

        var groundOuter: UInt32 {
            switch self {
            case .amber: 0x0B231E
            case .oxblood: 0x24060F
            case .propellant: 0x15171B
            case .copper: 0x071E21
            case .paper: 0xE7DCC4
            case .plum: 0x1E0F2A
            }
        }

        var wall: UInt32 {
            switch self {
            case .amber: 0xE9F2EC
            case .oxblood: 0xF3EBDD
            case .propellant: 0xDDE3E8
            case .copper: 0xEAF1EE
            case .paper: 0x1E1B18
            case .plum: 0xF1E9F4
            }
        }

        var liquidTop: UInt32 {
            switch self {
            case .amber: 0xF2B34A
            case .oxblood: 0xD9445F
            case .propellant: 0xFF6A2A
            case .copper: 0xD98A48
            case .paper: 0x2A3F5F
            case .plum: 0xF6B26B
            }
        }

        var liquidBottom: UInt32 {
            switch self {
            case .amber: 0xC47F1F
            case .oxblood: 0x8C1A33
            case .propellant: 0xD63A0A
            case .copper: 0x8E4E1F
            case .paper: 0x14213A
            case .plum: 0xE3703F
            }
        }

        var isLightGround: Bool { ((groundOuter >> 16) & 0xFF) > 0xA0 }
    }

    private enum Style {
        case applicationIcon
        case template
    }

    private static let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!

    static func menuBarImage() -> NSImage {
        menuBarImage(
            fillRatio: 0.3865,
            wavePhase: nil,
            accountLabel: nil,
            accessibilityDescription: "Ullage"
        )
    }

    /// `wavePhase` selects the surface: a phase draws the wobbling wave, `nil`
    /// draws a flat line for the still (frozen or animation-off) states.
    static func menuBarImage(
        fillRatio: Double?,
        wavePhase: Double? = nil,
        accountLabel: String? = nil,
        accessibilityRatio: Double? = nil
    ) -> NSImage {
        // The drawn height stays continuous so the breathing cycle does not
        // step; only the spoken value is quantized to the 5% grid.
        let drawnRatio = fillRatio.map { min(max($0.isFinite ? $0 : 0, 0), 1) }
        let accessibilityValue = (accessibilityRatio ?? fillRatio).map(quantizedMenuBarFillRatio)
        let description: String
        if let accessibilityValue {
            let percent = "Ullage — \(Int((accessibilityValue * 100).rounded()))% remaining"
            if let accountLabel, !accountLabel.isEmpty {
                description = percent + " · " + accountLabel
            } else {
                description = percent
            }
        } else {
            description = "Ullage — no data"
        }
        return menuBarImage(
            fillRatio: drawnRatio,
            wavePhase: wavePhase,
            accountLabel: accountLabel,
            accessibilityDescription: description
        )
    }

    private static func menuBarImage(
        fillRatio: Double?,
        wavePhase: Double?,
        accountLabel _: String?,
        accessibilityDescription: String
    ) -> NSImage {
        let size = NSSize(width: 18, height: 18)
        let image = NSImage(size: size, flipped: false) { rect in
            guard let context = NSGraphicsContext.current?.cgContext else { return false }
            draw(
                in: context,
                canvasSize: rect.width,
                style: .template,
                fillRatio: fillRatio,
                wavePhase: wavePhase
            )
            return true
        }
        image.accessibilityDescription = accessibilityDescription
        image.isTemplate = true
        return image
    }

    static func applicationIcon(pixelSize: Int, palette: Palette) throws -> NSBitmapImageRep {
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
        draw(in: context, canvasSize: CGFloat(pixelSize), style: .applicationIcon, palette: palette)
        graphicsContext.flushGraphics()
        NSGraphicsContext.restoreGraphicsState()
        return representation
    }

    private static func draw(
        in context: CGContext,
        canvasSize: CGFloat,
        style: Style,
        palette: Palette = .default,
        fillRatio: Double? = nil,
        wavePhase: Double? = nil
    ) {
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
                cornerRadius: cornerRadius,
                palette: palette
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
        drawVessel(
            in: context,
            style: style,
            palette: palette,
            unit: markScale,
            fillRatio: fillRatio,
            wavePhase: wavePhase
        )
        context.restoreGState()
    }

    private static func drawBackground(
        in context: CGContext,
        rect: CGRect,
        cornerRadius: CGFloat,
        palette: Palette
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
        let radius = hypot(rect.width, rect.height) * 0.62
        let radialGradient = gradient([(palette.groundInner, 1, 0), (palette.groundOuter, 1, 1)])
        context.drawRadialGradient(
            radialGradient,
            startCenter: center,
            startRadius: 0,
            endCenter: center,
            endRadius: radius,
            options: [.drawsAfterEndLocation]
        )
        context.drawLinearGradient(
            gradient([(0xFFFFFF, palette.isLightGround ? 0.35 : 0.14, 0), (0xFFFFFF, 0, 1)]),
            start: CGPoint(x: 0, y: rect.minY),
            end: CGPoint(x: 0, y: rect.minY + rect.height * 0.45),
            options: []
        )
        context.drawLinearGradient(
            gradient([(0x000000, 0, 0), (0x000000, palette.isLightGround ? 0.10 : 0.35, 1)]),
            start: CGPoint(x: 0, y: rect.minY + rect.height * 0.55),
            end: CGPoint(x: 0, y: rect.maxY),
            options: []
        )
        context.addPath(path)
        context.setStrokeColor(color(0xFFFFFF, alpha: palette.isLightGround ? 0.6 : 0.18))
        context.setLineWidth(rect.width / 824 * 1024 * 0.012)
        context.strokePath()
        context.restoreGState()
    }

    private static func drawVessel(
        in context: CGContext,
        style: Style,
        palette: Palette,
        unit: CGFloat,
        fillRatio: Double?,
        wavePhase: Double?
    ) {
        switch style {
        case .applicationIcon:
            drawApplicationIconVessel(in: context, palette: palette, unit: unit)
        case .template:
            drawTemplateVessel(in: context, fillRatio: fillRatio, wavePhase: wavePhase)
        }
    }

    private static func drawApplicationIconVessel(
        in context: CGContext,
        palette: Palette,
        unit: CGFloat
    ) {
        context.saveGState()
        context.setShadow(
            offset: CGSize(width: 0, height: -3.5 * unit),
            blur: 7 * unit,
            color: color(0x000000, alpha: palette.isLightGround ? 0.28 : 0.55)
        )
        context.addPath(closedU(radius: 29, top: 16))
        context.setFillColor(color(palette.groundOuter))
        context.fillPath()
        context.restoreGState()

        context.saveGState()
        context.addPath(closedU(radius: 19, top: 16))
        context.clip()
        context.drawLinearGradient(
            gradient([(palette.wall, 0.16, 0), (palette.wall, 0.05, 1)]),
            start: CGPoint(x: 31, y: 16),
            end: CGPoint(x: 69, y: 16),
            options: []
        )
        context.drawLinearGradient(
            gradient([(0x000000, 0.28, 0), (0x000000, 0, 1)]),
            start: CGPoint(x: 0, y: 16),
            end: CGPoint(x: 0, y: 30),
            options: []
        )
        context.restoreGState()

        let surfaceY: CGFloat = 52
        let surfaceRadiusY: CGFloat = 3.2
        context.saveGState()
        context.addPath(closedU(radius: 19.8, top: surfaceY))
        context.clip()
        context.drawLinearGradient(
            gradient([
                (palette.liquidTop, 1, 0),
                (palette.liquidBottom, 1, 0.75),
                (mix(palette.liquidBottom, 0x000000, amount: 0.35), 1, 1),
            ]),
            start: CGPoint(x: 50, y: surfaceY),
            end: CGPoint(x: 50, y: 79),
            options: [.drawsBeforeStartLocation, .drawsAfterEndLocation]
        )
        context.drawLinearGradient(
            gradient([
                (0x000000, 0.10, 0),
                (0x000000, 0, 0.10),
                (0x000000, 0, 0.90),
                (0x000000, 0.10, 1),
            ]),
            start: CGPoint(x: 31, y: 0),
            end: CGPoint(x: 69, y: 0),
            options: []
        )
        context.restoreGState()

        let surface = CGRect(
            x: 31,
            y: surfaceY - surfaceRadiusY,
            width: 38,
            height: surfaceRadiusY * 2
        )
        context.saveGState()
        context.addEllipse(in: surface)
        context.clip()
        context.drawLinearGradient(
            gradient([
                (mix(palette.liquidTop, 0xFFFFFF, amount: 0.45), 1, 0),
                (mix(palette.liquidTop, 0xFFFFFF, amount: 0.15), 1, 1),
            ]),
            start: CGPoint(x: 0, y: surface.minY),
            end: CGPoint(x: 0, y: surface.maxY),
            options: []
        )
        context.restoreGState()
        context.addEllipse(in: surface.insetBy(dx: 0.6, dy: 0.4))
        context.setStrokeColor(color(0xFFFFFF, alpha: 0.55))
        context.setLineWidth(0.7)
        context.strokePath()
        context.addEllipse(in: CGRect(x: 38, y: surfaceY - 1.4, width: 9, height: 1.6))
        context.setFillColor(color(0xFFFFFF, alpha: 0.6))
        context.fillPath()

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

        context.saveGState()
        context.addPath(vessel)
        context.setLineWidth(10)
        context.setLineCap(.butt)
        context.replacePathWithStrokedPath()
        context.clip()
        context.drawLinearGradient(
            gradient([
                (mix(palette.wall, 0xFFFFFF, amount: palette.isLightGround ? 0 : 0.6), 0.95, 0),
                (palette.wall, 0.92, 0.35),
                (mix(palette.wall, 0x000000, amount: palette.isLightGround ? 0.25 : 0.22), 0.92, 0.7),
                (palette.wall, 0.92, 1),
            ]),
            start: CGPoint(x: 21, y: 0),
            end: CGPoint(x: 79, y: 0),
            options: [.drawsBeforeStartLocation, .drawsAfterEndLocation]
        )
        context.drawLinearGradient(
            gradient([(0x000000, 0, 0), (0x000000, 0.18, 1)]),
            start: CGPoint(x: 0, y: 40),
            end: CGPoint(x: 0, y: 84),
            options: []
        )

        context.setLineWidth(2.4)
        context.setLineCap(.round)
        context.setStrokeColor(color(0xFFFFFF, alpha: palette.isLightGround ? 0.5 : 0.85))
        let outerHighlight = CGMutablePath()
        outerHighlight.move(to: CGPoint(x: 23.2, y: 22))
        outerHighlight.addLine(to: CGPoint(x: 23.2, y: 58))
        outerHighlight.addArc(
            center: CGPoint(x: 50, y: 60),
            radius: 26.8,
            startAngle: .pi,
            endAngle: .pi * 0.62,
            clockwise: true
        )
        context.addPath(outerHighlight)
        context.strokePath()

        context.setLineWidth(1.6)
        context.setStrokeColor(color(0xFFFFFF, alpha: palette.isLightGround ? 0.25 : 0.35))
        let innerReflection = CGMutablePath()
        innerReflection.move(to: CGPoint(x: 70.6, y: 30))
        innerReflection.addLine(to: CGPoint(x: 70.6, y: 60))
        context.addPath(innerReflection)
        context.strokePath()
        context.restoreGState()

        for x in [CGFloat(21), CGFloat(69)] {
            let rim = CGRect(x: x, y: 14.6, width: 10, height: 2.8)
            context.saveGState()
            context.addPath(CGPath(
                roundedRect: rim,
                cornerWidth: 1.2,
                cornerHeight: 1.2,
                transform: nil
            ))
            context.clip()
            context.drawLinearGradient(
                gradient([
                    (0xFFFFFF, palette.isLightGround ? 0.55 : 0.95, 0),
                    (mix(palette.wall, 0xFFFFFF, amount: 0.3), 1, 1),
                ]),
                start: CGPoint(x: 0, y: rim.minY),
                end: CGPoint(x: 0, y: rim.maxY),
                options: []
            )
            context.restoreGState()
        }
    }

    private static func drawTemplateVessel(
        in context: CGContext,
        fillRatio: Double?,
        wavePhase: Double?
    ) {
        let vessel = CGMutablePath()
        vessel.move(to: CGPoint(x: 12, y: 6))
        vessel.addLine(to: CGPoint(x: 12, y: 58))
        vessel.addArc(
            center: CGPoint(x: 50, y: 58),
            radius: 38,
            startAngle: .pi,
            endAngle: 0,
            clockwise: true
        )
        vessel.addLine(to: CGPoint(x: 88, y: 6))

        if let fillRatio {
            drawTemplateBrimLine(in: context)
            let clamped = min(max(fillRatio, 0), 1)
            if clamped > 0 {
                drawTemplateLiquidFill(
                    in: context,
                    fillRatio: clamped,
                    wavePhase: wavePhase
                )
            }
        } else {
            context.setFillColor(NSColor.black.withAlphaComponent(0.65).cgColor)
            context.addPath(CGPath(
                roundedRect: CGRect(x: 44.5, y: 32, width: 11, height: 36),
                cornerWidth: 5.5,
                cornerHeight: 5.5,
                transform: nil
            ))
            context.fillPath()
            context.fillEllipse(in: CGRect(x: 43.5, y: 73.5, width: 13, height: 13))
        }
        context.setStrokeColor(NSColor.black.cgColor)
        context.setLineWidth(9)
        context.setLineCap(.butt)
        context.addPath(vessel)
        context.strokePath()
    }

    /// Faint bar just below the cavity top: the full mark the liquid breathes
    /// up to. Drawn only in liquid states, never with the exclamation mark.
    private static func drawTemplateBrimLine(in context: CGContext) {
        context.saveGState()
        context.addPath(closedU(radius: 33.5, top: 10, centerY: 58))
        context.clip()
        context.setFillColor(NSColor.black.withAlphaComponent(0.35).cgColor)
        context.fill(CGRect(x: 0, y: 12, width: 100, height: 4.5))
        context.restoreGState()
    }

    /// Solid liquid filling the cavity up to the surface at the remaining
    /// height: a wave at `wavePhase`, or a flat line when the phase is `nil`.
    private static func drawTemplateLiquidFill(
        in context: CGContext,
        fillRatio: Double,
        wavePhase: Double?
    ) {
        let surfaceY = 91.5 - 81.5 * CGFloat(fillRatio)
        let amplitude: CGFloat = wavePhase == nil ? 0 : 1.8
        let wavePhase = wavePhase ?? 0
        let minX: CGFloat = 0
        let maxX: CGFloat = 100
        let liquid = CGMutablePath()
        let steps = 20
        for step in 0...steps {
            let t = CGFloat(step) / CGFloat(steps)
            let x = minX + (maxX - minX) * t
            let phase = wavePhase + Double(t) * .pi * 2
            let y = surfaceY + amplitude * CGFloat(sin(phase))
            if step == 0 {
                liquid.move(to: CGPoint(x: x, y: y))
            } else {
                liquid.addLine(to: CGPoint(x: x, y: y))
            }
        }
        liquid.addLine(to: CGPoint(x: maxX, y: 100))
        liquid.addLine(to: CGPoint(x: minX, y: 100))
        liquid.closeSubpath()

        context.saveGState()
        context.addPath(closedU(radius: 33.5, top: 10, centerY: 58))
        context.clip()
        context.setFillColor(NSColor.black.withAlphaComponent(0.5).cgColor)
        context.addPath(liquid)
        context.fillPath()
        context.restoreGState()
    }

    private static func closedU(
        radius: CGFloat,
        top: CGFloat,
        centerY: CGFloat = 60
    ) -> CGPath {
        let path = CGMutablePath()
        path.move(to: CGPoint(x: 50 - radius, y: top))
        path.addLine(to: CGPoint(x: 50 - radius, y: centerY))
        path.addArc(
            center: CGPoint(x: 50, y: centerY),
            radius: radius,
            startAngle: .pi,
            endAngle: 0,
            clockwise: true
        )
        path.addLine(to: CGPoint(x: 50 + radius, y: top))
        path.closeSubpath()
        return path
    }

    private static func gradient(_ stops: [(UInt32, CGFloat, CGFloat)]) -> CGGradient {
        CGGradient(
            colorsSpace: colorSpace,
            colors: stops.map { color($0.0, alpha: $0.1) } as CFArray,
            locations: stops.map(\.2)
        )!
    }

    private static func mix(_ first: UInt32, _ second: UInt32, amount: CGFloat) -> UInt32 {
        func channel(shift: Int) -> UInt32 {
            let start = CGFloat((first >> shift) & 0xFF)
            let end = CGFloat((second >> shift) & 0xFF)
            return UInt32((start + (end - start) * amount).rounded()) << shift
        }
        return channel(shift: 16) | channel(shift: 8) | channel(shift: 0)
    }

    private static func color(_ hexadecimal: UInt32, alpha: CGFloat = 1) -> CGColor {
        let red = CGFloat((hexadecimal >> 16) & 0xFF) / 255
        let green = CGFloat((hexadecimal >> 8) & 0xFF) / 255
        let blue = CGFloat(hexadecimal & 0xFF) / 255
        return CGColor(colorSpace: colorSpace, components: [red, green, blue, alpha])!
    }
}

enum UllageMarkError: Error {
    case invalidPixelSize
    case couldNotCreateBitmap
}
