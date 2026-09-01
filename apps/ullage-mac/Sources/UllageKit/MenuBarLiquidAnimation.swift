import Foundation

/// Whether the menu bar liquid may animate.
public enum MenuBarLiquidMotionGate: Equatable, Sendable {
    /// AC power and reduce-motion off: wobble and height easing may run.
    case animate
    /// Battery or reduce-motion: snap to and hold the current target height.
    case freeze
    /// Error / no data: stop motion; caller draws the exclamation mark.
    case stop
}

/// Pure animation state for the menu bar liquid surface.
public struct MenuBarLiquidAnimationState: Equatable, Sendable {
    public var displayedRatio: Double
    public var targetRatio: Double
    public var accountIndex: Int
    public var accountID: String?
    public var displayName: String?
    public var wavePhase: Double
    public var secondsInAccount: Double

    public init(
        displayedRatio: Double = 0.4,
        targetRatio: Double = 0.4,
        accountIndex: Int = 0,
        accountID: String? = nil,
        displayName: String? = nil,
        wavePhase: Double = 0,
        secondsInAccount: Double = 0
    ) {
        self.displayedRatio = displayedRatio
        self.targetRatio = targetRatio
        self.accountIndex = accountIndex
        self.accountID = accountID
        self.displayName = displayName
        self.wavePhase = wavePhase
        self.secondsInAccount = secondsInAccount
    }
}

public enum MenuBarLiquidAnimation {
    /// Target frame rate for the wobble timer (8–12 fps band).
    public static let framesPerSecond: Double = 10
    public static let accountRotateInterval: TimeInterval = 60
    /// Approximate seconds for a height transition to settle.
    public static let heightTransitionDuration: TimeInterval = 1.25
    public static let waveRadiansPerSecond: Double = 2.4

    public static func tickInterval() -> TimeInterval {
        1 / framesPerSecond
    }

    /// Move `current` toward `target` with an exponential ease (monotonic).
    public static func approachRatio(
        current: Double,
        target: Double,
        dt: TimeInterval,
        duration: TimeInterval = heightTransitionDuration
    ) -> Double {
        let safeDuration = max(duration, 0.001)
        let safeDT = max(dt, 0)
        let alpha = 1 - exp(-safeDT * 4.5 / safeDuration)
        let next = current + (target - current) * alpha
        if abs(target - next) < 0.0005 { return target }
        return next
    }

    /// Advance wobble / height / account rotation. No-ops when `gate != .animate`
    /// except still syncing the target account when levels change under `.freeze`.
    public static func advance(
        state: MenuBarLiquidAnimationState,
        levels: [MenuBarAccountLevel],
        gate: MenuBarLiquidMotionGate,
        dt: TimeInterval
    ) -> MenuBarLiquidAnimationState {
        guard gate != .stop, !levels.isEmpty else { return state }

        var next = state
        let clampedDT = max(dt, 0)

        if let currentID = next.accountID,
           let index = levels.firstIndex(where: { $0.accountID == currentID }) {
            next.accountIndex = index
        } else {
            next.accountIndex = min(max(next.accountIndex, 0), levels.count - 1)
        }

        let level = levels[next.accountIndex]
        next.accountID = level.accountID
        next.displayName = level.displayName
        next.targetRatio = quantizedMenuBarFillRatio(level.remainingRatio)

        if gate == .freeze {
            next.displayedRatio = next.targetRatio
            return next
        }

        if levels.count > 1 {
            next.secondsInAccount += clampedDT
            if next.secondsInAccount >= accountRotateInterval {
                next.secondsInAccount = 0
                next.accountIndex = (next.accountIndex + 1) % levels.count
                let rotated = levels[next.accountIndex]
                next.accountID = rotated.accountID
                next.displayName = rotated.displayName
                next.targetRatio = quantizedMenuBarFillRatio(rotated.remainingRatio)
            }
        } else {
            next.secondsInAccount = 0
        }

        next.displayedRatio = approachRatio(
            current: next.displayedRatio,
            target: next.targetRatio,
            dt: clampedDT
        )
        next.wavePhase = next.wavePhase + waveRadiansPerSecond * clampedDT
        if next.wavePhase > .pi * 2 {
            next.wavePhase -= .pi * 2
        }
        return next
    }
}
