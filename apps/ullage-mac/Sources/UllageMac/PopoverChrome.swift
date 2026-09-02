import SwiftUI
import UllageKit

// MARK: - Surfaces

/// Backdrop behind the `NSPopover` on macOS 14 and 15, where there is no
/// Liquid Glass: a solid base carrying two blurred discs and a diagonal
/// streak, which the frosted panels pick up as tone and shape. macOS 26 draws
/// no backdrop at all; the popover surface there is glass over whatever the
/// panel covers (see `PopoverSurface`).
struct AtmosphereBackground: View {
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        StructuredBackdrop(isDark: colorScheme == .dark)
            .ignoresSafeArea()
    }
}

/// Two discs and a streak on a solid base. The shapes are anchored to the top
/// and bottom edges rather than scaled with the height, so the header always
/// has the warm disc and the streak behind it and the last card always has the
/// cool disc, whatever the popover's height. The blur keeps the edges from
/// reading as flat cut-outs while leaving enough contrast to show through the
/// frosted panels.
private struct StructuredBackdrop: View {
    let isDark: Bool

    var body: some View {
        GeometryReader { proxy in
            let width = proxy.size.width
            let height = proxy.size.height
            ZStack {
                base
                Circle()
                    .fill(warm)
                    .frame(width: 340, height: 340)
                    .position(x: width * 0.22, y: 40)
                    .blur(radius: 14)
                Circle()
                    .fill(rose)
                    .frame(width: 220, height: 220)
                    .position(x: width * 0.96, y: height * 0.48)
                    .blur(radius: 18)
                Circle()
                    .fill(cool)
                    .frame(width: 300, height: 300)
                    .position(x: width * 0.30, y: height - 30)
                    .blur(radius: 16)
                Capsule()
                    .fill(streak)
                    .frame(width: width * 1.3, height: 26)
                    .rotationEffect(.degrees(-24))
                    .position(x: width * 0.55, y: 132)
                    .blur(radius: 5)
            }
            .clipped()
        }
    }

    private var base: Color {
        isDark ? Color(red: 0.055, green: 0.051, blue: 0.067) : Color(white: 0.96)
    }

    private var warm: Color {
        isDark ? Color(red: 0.44, green: 0.08, blue: 0.19) : Color(red: 0.90, green: 0.56, blue: 0.63)
    }

    private var rose: Color {
        isDark ? Color(red: 0.60, green: 0.20, blue: 0.33) : Color(red: 0.96, green: 0.74, blue: 0.79)
    }

    private var cool: Color {
        isDark ? Color(red: 0.16, green: 0.23, blue: 0.48) : Color(red: 0.62, green: 0.72, blue: 0.93)
    }

    private var streak: Color {
        isDark
            ? Color(red: 0.93, green: 0.88, blue: 0.82).opacity(0.72)
            : Color(red: 0.42, green: 0.08, blue: 0.18).opacity(0.50)
    }
}

/// The popover's outer surface. On macOS 26 the whole popover is one Liquid
/// Glass slab in a transparent panel, so the desktop and windows underneath
/// show through it and bend at its edges; below macOS 26 it is the tinted
/// backdrop inside the `NSPopover` frame. The slab is `Glass.clear`, not
/// `.regular`: regular glass blurs and tints so heavily that a plain window
/// behind the popover (a white web page, a document) turns into a flat grey
/// and the effect disappears, while clear glass keeps the page's structure
/// visible and lenses it at the edges. Text sits on the frosted panels, not
/// on the slab, so the slab needs no dimming layer for legibility.
struct PopoverSurface: ViewModifier {
    static let cornerRadius: CGFloat = 22

    func body(content: Content) -> some View {
        if #available(macOS 26.0, *) {
            let shape = RoundedRectangle(cornerRadius: Self.cornerRadius, style: .continuous)
            content
                .clipShape(shape)
                .glassEffect(.clear, in: shape)
        } else {
            content.background { AtmosphereBackground() }
        }
    }
}

/// Panel surface inside the popover: a layered material with an inset
/// highlight. The panels sit on the glass slab on macOS 26 and on the tinted
/// backdrop below it; they are not glass themselves, since glass nested in
/// glass only re-samples the already blurred slab and adds cost without
/// adding refraction.
struct GlassPanel: ViewModifier {
    @Environment(\.colorScheme) private var colorScheme
    var cornerRadius: CGFloat = 18
    var prominent = false

    func body(content: Content) -> some View {
        content
            .background {
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .fill(.ultraThinMaterial)
                    .overlay {
                        RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                            .fill(
                                LinearGradient(
                                    colors: [fillTop, fillBottom],
                                    startPoint: .top,
                                    endPoint: .bottom
                                )
                            )
                    }
            }
            .overlay {
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .strokeBorder(
                        LinearGradient(
                            colors: [highlight, border],
                            startPoint: .top,
                            endPoint: .bottom
                        ),
                        lineWidth: 1
                    )
            }
            .clipShape(RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
            .shadow(color: .black.opacity(shadowOpacity), radius: prominent ? 16 : 10, y: prominent ? 8 : 5)
    }

    private var isDark: Bool { colorScheme == .dark }
    private var fillTop: Color {
        isDark ? .white.opacity(prominent ? 0.12 : 0.10) : .white.opacity(prominent ? 0.70 : 0.58)
    }
    private var fillBottom: Color {
        isDark ? .white.opacity(prominent ? 0.05 : 0.04) : .white.opacity(prominent ? 0.45 : 0.34)
    }
    private var highlight: Color { isDark ? .white.opacity(0.28) : .white.opacity(0.85) }
    private var border: Color { isDark ? .white.opacity(0.10) : .black.opacity(0.08) }
    private var shadowOpacity: Double { isDark ? (prominent ? 0.45 : 0.32) : (prominent ? 0.16 : 0.10) }
}

extension View {
    func glassPanel(cornerRadius: CGFloat = 18, prominent: Bool = false) -> some View {
        modifier(GlassPanel(cornerRadius: cornerRadius, prominent: prominent))
    }
}

// MARK: - Vessel

/// The U-vessel from the app mark, filled to `ratio` with the oxblood liquid.
/// Shares `UllageMark`'s 100x100 geometry so the popover, the menu bar and the
/// application icon read as the same object.
struct LiquidVessel: View {
    let ratio: Double
    /// Phase of the surface wave, in radians; `nil` draws a flat surface. The
    /// height never depends on it, so an animating phase only moves the wave.
    var wavePhase: Double? = nil
    var size: CGFloat = 64
    @Environment(\.colorScheme) private var colorScheme

    private var clamped: Double { min(max(ratio.isFinite ? ratio : 0, 0), 1) }
    private var surfaceY: CGFloat { 91.5 - 81.5 * CGFloat(clamped) }

    var body: some View {
        Canvas { context, canvasSize in
            let scale = canvasSize.width / 100
            context.scaleBy(x: scale, y: scale)

            if clamped > 0 {
                context.clip(to: Self.cavity)
                context.fill(liquidPath, with: .linearGradient(
                    Gradient(colors: [Self.liquidTop, Self.liquidBottom]),
                    startPoint: CGPoint(x: 50, y: surfaceY),
                    endPoint: CGPoint(x: 50, y: 100)
                ))
                context.stroke(
                    surfacePath,
                    with: .color(.white.opacity(0.45)),
                    lineWidth: 1.6
                )
            }
        }
        .frame(width: size, height: size)
        .overlay {
            Canvas { context, canvasSize in
                let scale = canvasSize.width / 100
                context.scaleBy(x: scale, y: scale)
                context.stroke(Self.wall, with: .color(wallColor), lineWidth: 9)
            }
        }
        .shadow(color: Self.liquidTop.opacity(clamped > 0 ? 0.55 : 0), radius: size * 0.16)
        .accessibilityHidden(true)
    }

    private var wallColor: Color {
        colorScheme == .dark ? Color(red: 0.95, green: 0.92, blue: 0.87) : Color(white: 0.14)
    }

    private static let liquidTop = Color(red: 0.85, green: 0.27, blue: 0.37)
    private static let liquidBottom = Color(red: 0.55, green: 0.10, blue: 0.20)

    /// Wave crest amplitude, in mark units; matches the menu bar mark.
    static let amplitude: CGFloat = 1.8

    private var surfacePath: Path {
        Self.surfacePath(surfaceY: surfaceY, wavePhase: wavePhase)
    }

    /// One wavelength of sine across the vessel at `wavePhase`, sampled the
    /// same way `UllageMark` draws the menu bar surface; a flat line when the
    /// phase is `nil`.
    static func surfacePath(surfaceY: CGFloat, wavePhase: Double?, steps: Int = 24) -> Path {
        let amplitude = wavePhase == nil ? 0 : amplitude
        let phase = wavePhase ?? 0
        var path = Path()
        for step in 0...steps {
            let t = Double(step) / Double(steps)
            let point = CGPoint(
                x: 100 * t,
                y: surfaceY + amplitude * CGFloat(sin(phase + t * .pi * 2))
            )
            if step == 0 {
                path.move(to: point)
            } else {
                path.addLine(to: point)
            }
        }
        return path
    }

    private var liquidPath: Path {
        var path = surfacePath
        path.addLine(to: CGPoint(x: 100, y: 100))
        path.addLine(to: CGPoint(x: 0, y: 100))
        path.closeSubpath()
        return path
    }

    private static let cavity: Path = {
        var path = Path()
        path.move(to: CGPoint(x: 16.5, y: 10))
        path.addLine(to: CGPoint(x: 16.5, y: 58))
        path.addArc(
            center: CGPoint(x: 50, y: 58),
            radius: 33.5,
            startAngle: .degrees(180),
            endAngle: .degrees(0),
            clockwise: true
        )
        path.addLine(to: CGPoint(x: 83.5, y: 10))
        path.closeSubpath()
        return path
    }()

    private static let wall: Path = {
        var path = Path()
        path.move(to: CGPoint(x: 12, y: 6))
        path.addLine(to: CGPoint(x: 12, y: 58))
        path.addArc(
            center: CGPoint(x: 50, y: 58),
            radius: 38,
            startAngle: .degrees(180),
            endAngle: .degrees(0),
            clockwise: true
        )
        path.addLine(to: CGPoint(x: 88, y: 6))
        return path
    }()
}

// MARK: - Provider marks

/// Simplified stand-in marks for each provider, drawn on a 24x24 grid.
/// They are approximations, not the companies' official logos.
struct ProviderMark: View {
    let provider: String
    var size: CGFloat = 16
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        Canvas { context, canvasSize in
            let scale = canvasSize.width / 24
            context.scaleBy(x: scale, y: scale)
            let style = StrokeStyle(lineWidth: lineWidth, lineCap: .round, lineJoin: .round)
            context.stroke(path, with: .color(tint), style: style)
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }

    private var tint: Color {
        switch provider {
        case "claude": Color(red: 0.85, green: 0.47, blue: 0.34)
        default: colorScheme == .dark ? Color(white: 0.96) : Color(white: 0.16)
        }
    }

    private var lineWidth: CGFloat {
        switch provider {
        case "claude": 2.6
        case "grok": 2.4
        default: 1.8
        }
    }

    private var path: Path {
        switch provider {
        case "chatgpt": Self.knot
        case "claude": Self.sunburst
        case "cursor": Self.cube
        case "grok": Self.cross
        default: Self.dot
        }
    }

    private static func hexagon(radius: CGFloat, center: CGPoint = CGPoint(x: 12, y: 12)) -> Path {
        var path = Path()
        for step in 0..<6 {
            let angle = Double(step) / 6 * 2 * .pi - .pi / 2
            let point = CGPoint(
                x: center.x + radius * CGFloat(cos(angle)),
                y: center.y + radius * CGFloat(sin(angle))
            )
            if step == 0 { path.move(to: point) } else { path.addLine(to: point) }
        }
        path.closeSubpath()
        return path
    }

    private static let knot: Path = {
        var path = hexagon(radius: 8.8)
        path.addPath(hexagon(radius: 3.8))
        for step in 0..<6 {
            let angle = Double(step) / 6 * 2 * .pi - .pi / 2
            let inner = CGPoint(x: 12 + 3.8 * CGFloat(cos(angle)), y: 12 + 3.8 * CGFloat(sin(angle)))
            let outer = CGPoint(x: 12 + 8.8 * CGFloat(cos(angle)), y: 12 + 8.8 * CGFloat(sin(angle)))
            path.move(to: inner)
            path.addLine(to: outer)
        }
        return path
    }()

    private static let sunburst: Path = {
        var path = Path()
        for step in 0..<8 {
            let angle = Double(step) / 8 * 2 * .pi
            let inner = CGPoint(x: 12 + 4.2 * CGFloat(cos(angle)), y: 12 + 4.2 * CGFloat(sin(angle)))
            let outer = CGPoint(x: 12 + 8.2 * CGFloat(cos(angle)), y: 12 + 8.2 * CGFloat(sin(angle)))
            path.move(to: inner)
            path.addLine(to: outer)
        }
        return path
    }()

    private static let cube: Path = {
        var path = Path()
        path.move(to: CGPoint(x: 12, y: 2.8))
        path.addLine(to: CGPoint(x: 20, y: 7.4))
        path.addLine(to: CGPoint(x: 20, y: 16.6))
        path.addLine(to: CGPoint(x: 12, y: 21.2))
        path.addLine(to: CGPoint(x: 4, y: 16.6))
        path.addLine(to: CGPoint(x: 4, y: 7.4))
        path.closeSubpath()
        path.move(to: CGPoint(x: 4, y: 7.4))
        path.addLine(to: CGPoint(x: 12, y: 12))
        path.addLine(to: CGPoint(x: 20, y: 7.4))
        path.move(to: CGPoint(x: 12, y: 12))
        path.addLine(to: CGPoint(x: 12, y: 21.2))
        return path
    }()

    private static let cross: Path = {
        var path = Path()
        path.move(to: CGPoint(x: 5, y: 4))
        path.addLine(to: CGPoint(x: 19, y: 20))
        path.move(to: CGPoint(x: 19, y: 4))
        path.addLine(to: CGPoint(x: 13.5, y: 10.3))
        path.move(to: CGPoint(x: 5, y: 20))
        path.addLine(to: CGPoint(x: 10.5, y: 13.7))
        return path
    }()

    private static let dot: Path = {
        Path(ellipseIn: CGRect(x: 8, y: 8, width: 8, height: 8))
    }()
}

/// Provider mark inside a rounded tile, as used beside every account name.
struct ProviderBadge: View {
    let provider: String
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        ProviderMark(provider: provider)
            .frame(width: 26, height: 26)
            .background {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(colorScheme == .dark ? Color.white.opacity(0.10) : Color.white.opacity(0.65))
            }
            .overlay {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .strokeBorder(
                        colorScheme == .dark ? Color.white.opacity(0.14) : Color.black.opacity(0.08),
                        lineWidth: 1
                    )
            }
    }
}

// MARK: - Hero

/// The single number the menu bar liquid is tracking, plus the account it came
/// from and the runner-up, so the popover and the mark agree at a glance.
struct PopoverHero: View {
    let model: HeroModel
    let isRefreshing: Bool
    /// True while the popover is on screen; the wave only runs then.
    let isPresented: Bool
    let settings: AppSettings
    let refresh: () -> Void
    let openSettings: () -> Void

    var body: some View {
        // The gate is read here, outside the timeline, so the power source
        // and Reduce Motion are queried when the hero's inputs change, not on
        // every frame. The level is fixed by `model.ratio`; only the surface
        // moves, at the menu bar's frame rate and wave speed.
        let animates = isPresented && liquidMotionIsAllowed(settings: settings)
        HStack(spacing: 14) {
            TimelineView(.animation(minimumInterval: MenuBarLiquidAnimation.tickInterval(), paused: !animates)) { context in
                LiquidVessel(
                    ratio: model.ratio,
                    wavePhase: animates ? Self.wavePhase(at: context.date) : nil,
                    size: 64
                )
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(model.caption)
                    .font(.system(size: 11, weight: .semibold))
                    .kerning(0.6)
                    .textCase(.uppercase)
                    .foregroundStyle(.secondary)
                // Last baseline, not first: a wrapped account name keeps its
                // bottom line sitting on the percentage's baseline.
                HStack(alignment: .lastTextBaseline, spacing: 8) {
                    Text(percentageText(model.ratio * 100))
                        .font(.system(size: 34, weight: .bold, design: .rounded))
                        .monospacedDigit()
                        .kerning(-1)
                        .foregroundStyle(tierColor)
                        .shadow(color: tierColor.opacity(0.55), radius: 12)
                        .contentTransition(.numericText())
                    Text(wrapFriendlyTitle(model.title))
                        .font(.system(size: 13, weight: .semibold))
                        .multilineTextAlignment(.leading)
                        // Bounded lines plus scale-to-fit: a token wider than
                        // the column would otherwise break mid-word and strand
                        // its closing bracket on a line of its own.
                        .lineLimit(3)
                        .minimumScaleFactor(0.8)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if let detail = model.detail {
                    Text(detail)
                        .font(.system(size: 12))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .minimumScaleFactor(0.85)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            VStack(spacing: 8) {
                CircleIconButton(
                    systemName: "arrow.clockwise",
                    label: "Refresh",
                    spinning: isRefreshing,
                    action: refresh
                )
                CircleIconButton(systemName: "gearshape", label: "Settings", action: openSettings)
            }
        }
        .padding(14)
        .glassPanel(cornerRadius: 22, prominent: true)
        .animation(.snappy(duration: 0.35), value: model.ratio)
    }

    private var tierColor: Color {
        Color(nsColor: progressColor(for: RemainingTier(ratio: model.ratio)))
    }

    /// Wave phase for a frame: the menu bar's angular speed applied to the
    /// clock, so the wave keeps the same pace across openings of the popover.
    static func wavePhase(at date: Date) -> Double {
        let radians = date.timeIntervalSinceReferenceDate * MenuBarLiquidAnimation.waveRadiansPerSecond
        return radians.truncatingRemainder(dividingBy: 2 * .pi)
    }
}

private struct CircleIconButton: View {
    let systemName: String
    let label: String
    var spinning = false
    let action: () -> Void
    @Environment(\.colorScheme) private var colorScheme
    @State private var isHovering = false

    var body: some View {
        Button(action: action) {
            icon
                .rotationEffect(.degrees(spinning ? 360 : 0))
                .animation(
                    spinning
                        ? .linear(duration: 0.9).repeatForever(autoreverses: false)
                        : .default,
                    value: spinning
                )
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .focusEffectDisabled()
        .onHover { isHovering = $0 }
        .help(label)
        .accessibilityLabel(label)
    }

    @ViewBuilder
    private var icon: some View {
        let glyph = Image(systemName: systemName)
            .font(.system(size: 12, weight: .semibold))
            .foregroundStyle(.secondary)
            .frame(width: 28, height: 28)
        if #available(macOS 26.0, *) {
            glyph.glassEffect(.regular.interactive(), in: .circle)
        } else {
            glyph
                .background {
                    Circle().fill(
                        colorScheme == .dark
                            ? Color.white.opacity(isHovering ? 0.16 : 0.08)
                            : Color.white.opacity(isHovering ? 0.90 : 0.65)
                    )
                }
                .overlay {
                    Circle().strokeBorder(
                        colorScheme == .dark ? Color.white.opacity(0.12) : Color.black.opacity(0.07),
                        lineWidth: 1
                    )
                }
        }
    }
}

/// Bind punctuation to its neighbour so a wrapped title never leaves a
/// separator or a bracket alone on a line. The separator takes a non-breaking
/// space before it, and brackets take a word joiner on their inner side, which
/// pushes the break into the surrounding words instead.
func wrapFriendlyTitle(_ title: String) -> String {
    let wordJoiner = "\u{2060}"
    let nonBreakingSpace = "\u{00A0}"
    let opening: Set<Character> = ["(", "[", "{"]
    let closing: Set<Character> = [")", "]", "}", ",", ".", ":", ";"]

    var result = ""
    let characters = Array(title)
    for (index, character) in characters.enumerated() {
        let next: Character? = index + 1 < characters.count ? characters[index + 1] : nil
        if character == " ", next == "\u{00B7}" {
            result += nonBreakingSpace
        } else {
            result.append(character)
            if opening.contains(character) { result += wordJoiner }
        }
        if let next, closing.contains(next) { result += wordJoiner }
    }
    return result
}

/// What the hero shows: the level the menu bar tracks, where it came from, and
/// the runner-up account when there is one.
struct HeroModel: Equatable {
    let ratio: Double
    let title: String
    let caption: String
    let detail: String?
}

/// Build the hero from the same levels that drive the menu bar liquid, so the
/// two never disagree. Returns `nil` when no account has a usable level.
@MainActor
/// The row-visibility sets are required, not defaulted: the header resolves the
/// pinned row through the same lists the menu bar and Settings use, and passing
/// empty sets here would hide rows the user opted in and silently unpin them.
func heroModel(
    accounts: [Account],
    snapshots: [SnapshotPayload],
    pinnedMetricID: String?,
    hiddenOverviewItemIDs: Set<String>,
    shownOverviewItemIDs: Set<String>,
    now: Date = Date()
) -> HeroModel? {
    let levels = menuBarLiquidLevels(
        accounts: accounts,
        snapshots: snapshots,
        pinnedMetricID: pinnedMetricID,
        hiddenOverviewItemIDs: hiddenOverviewItemIDs,
        shownOverviewItemIDs: shownOverviewItemIDs
    )
    guard let floor = MenuBarLiquidAnimation.floorLevel(in: levels) else { return nil }
    let pinned: MenuBarMetricOption? = pinnedMetricID.flatMap { id in
        menuBarMetricOptions(
            accounts: accounts,
            snapshots: snapshots,
            hiddenOverviewItemIDs: hiddenOverviewItemIDs,
            shownOverviewItemIDs: shownOverviewItemIDs
        ).first { $0.id == id }
    }
    let caption = pinned == nil ? "Lowest remaining" : "Tracking"

    var parts: [String] = []
    // A pinned row reports its own window; otherwise the floor account's
    // soonest visible reset.
    let reset: String? = if let pinned {
        pinned.resetsAt.flatMap { $0 > now ? relativeTimeText($0, now: now) : nil }
    } else {
        soonestReset(
            accountID: floor.accountID,
            snapshots: snapshots,
            hiddenOverviewItemIDs: hiddenOverviewItemIDs,
            shownOverviewItemIDs: shownOverviewItemIDs,
            now: now
        )
    }
    if let reset {
        parts.append("resets \(reset)")
    }
    let runnerUp = levels
        .filter { $0.accountID != floor.accountID }
        .min { $0.remainingRatio < $1.remainingRatio }
    if let runnerUp {
        parts.append("next \(runnerUp.displayName) \(percentageText(runnerUp.remainingRatio * 100))")
    }

    return HeroModel(
        ratio: floor.remainingRatio,
        title: floor.displayName,
        caption: caption,
        detail: parts.isEmpty ? nil : parts.joined(separator: " · ")
    )
}

/// Soonest future reset among an account's Overview rows that carry a ratio.
private func soonestReset(
    accountID: String,
    snapshots: [SnapshotPayload],
    hiddenOverviewItemIDs: Set<String>,
    shownOverviewItemIDs: Set<String>,
    now: Date
) -> String? {
    guard let usage = snapshots.first(where: { $0.accountId == accountID })?.usage.data else {
        return nil
    }
    let resets = visibleOverviewItems(
        for: usage,
        accountID: accountID,
        hiddenIDs: hiddenOverviewItemIDs,
        shownIDs: shownOverviewItemIDs
    )
        .filter { $0.row.remainingRatio != nil }
        .compactMap(\.row.resetsAt)
        .filter { $0 > now }
    guard let soonest = resets.min() else { return nil }
    return relativeTimeText(soonest, now: now)
}
