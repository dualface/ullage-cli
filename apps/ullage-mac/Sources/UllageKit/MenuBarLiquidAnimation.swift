import Foundation

/// Whether the menu bar liquid may animate.
public enum MenuBarLiquidMotionGate: Equatable, Sendable {
    /// Animation enabled in Settings, AC power, and reduce-motion off: wobble
    /// and the breathing cycle may run.
    case animate
    /// Animation turned off in Settings, battery, or reduce-motion: hold the
    /// liquid still at the floor level with a flat surface.
    case freeze
    /// Error / no data: stop motion; caller draws the exclamation mark.
    case stop
}

/// Pure animation state for the menu bar liquid surface.
///
/// The liquid breathes between a full vessel and `floorRatio`, the lowest
/// remaining ratio across the accounts currently shown. `displayedRatio` is
/// the continuous height for the current frame; `floorRatio` is the quantized
/// data value the accessibility description and the frozen image report.
public struct MenuBarLiquidAnimationState: Equatable, Sendable {
    public var displayedRatio: Double
    public var floorRatio: Double
    public var floorAccountID: String?
    public var floorDisplayName: String?
    public var wavePhase: Double
    /// Position inside the breathing cycle, in `0..<cycleDuration`.
    public var secondsInCycle: Double

    public init(
        displayedRatio: Double = 1,
        floorRatio: Double = 1,
        floorAccountID: String? = nil,
        floorDisplayName: String? = nil,
        wavePhase: Double = 0,
        secondsInCycle: Double = 0
    ) {
        self.displayedRatio = displayedRatio
        self.floorRatio = floorRatio
        self.floorAccountID = floorAccountID
        self.floorDisplayName = floorDisplayName
        self.wavePhase = wavePhase
        self.secondsInCycle = secondsInCycle
    }
}

public enum MenuBarLiquidAnimation {
    /// Target frame rate for the wobble timer (8–12 fps band).
    public static let framesPerSecond: Double = 10
    /// One full breath: full -> floor -> full.
    public static let cycleDuration: TimeInterval = 40
    public static let waveRadiansPerSecond: Double = 2.4

    public static func tickInterval() -> TimeInterval {
        1 / framesPerSecond
    }

    /// The account with the lowest remaining ratio; ties keep the earlier
    /// account in the stable order.
    public static func floorLevel(in levels: [MenuBarAccountLevel]) -> MenuBarAccountLevel? {
        levels.min { $0.remainingRatio < $1.remainingRatio }
    }

    /// Height of the breathing cycle at `secondsInCycle`: `1` at the start and
    /// end of the cycle, `floor` halfway through, eased with a cosine so the
    /// turnarounds are smooth.
    public static func cycleRatio(floor: Double, secondsInCycle: Double) -> Double {
        let clampedFloor = min(max(floor, 0), 1)
        let progress = (1 - cos(secondsInCycle / cycleDuration * 2 * .pi)) / 2
        return 1 - (1 - clampedFloor) * progress
    }

    /// Advance wobble and the breathing cycle. No-ops when `gate == .stop`;
    /// under `.freeze` only the floor account is refreshed and the liquid is
    /// parked at the floor.
    public static func advance(
        state: MenuBarLiquidAnimationState,
        levels: [MenuBarAccountLevel],
        gate: MenuBarLiquidMotionGate,
        dt: TimeInterval
    ) -> MenuBarLiquidAnimationState {
        guard gate != .stop, let floor = floorLevel(in: levels) else { return state }

        var next = state
        next.floorRatio = quantizedMenuBarFillRatio(floor.remainingRatio)
        next.floorAccountID = floor.accountID
        next.floorDisplayName = floor.displayName

        if gate == .freeze {
            // Park at the trough so a later resume rises out of the frozen
            // level instead of jumping to wherever the cycle had been.
            next.displayedRatio = next.floorRatio
            next.secondsInCycle = cycleDuration / 2
            return next
        }

        let clampedDT = max(dt, 0)
        next.secondsInCycle = (next.secondsInCycle + clampedDT)
            .truncatingRemainder(dividingBy: cycleDuration)
        next.displayedRatio = cycleRatio(
            floor: next.floorRatio,
            secondsInCycle: next.secondsInCycle
        )
        if clampedDT > 0 {
            next.wavePhase = next.wavePhase + waveRadiansPerSecond * clampedDT
            if next.wavePhase > .pi * 2 {
                next.wavePhase -= .pi * 2
            }
        }
        return next
    }
}
