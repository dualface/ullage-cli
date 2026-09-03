import SwiftUI

/// The app's shared color vocabulary. Each named palette owns one canonical
/// set of icon colors; flat surfaces derive their colors from the same set.
enum AppPalette: String, CaseIterable, Identifiable {
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

    var colors: Colors {
        switch self {
        case .amber:
            Colors(groundInner: 0x1B5044, groundOuter: 0x0B231E, wall: 0xE9F2EC,
                   liquidTop: 0xF2B34A, liquidBottom: 0xC47F1F)
        case .oxblood:
            Colors(groundInner: 0x4A1020, groundOuter: 0x24060F, wall: 0xF3EBDD,
                   liquidTop: 0xD9445F, liquidBottom: 0x8C1A33)
        case .propellant:
            Colors(groundInner: 0x2A2E35, groundOuter: 0x15171B, wall: 0xDDE3E8,
                   liquidTop: 0xFF6A2A, liquidBottom: 0xD63A0A)
        case .copper:
            Colors(groundInner: 0x123C40, groundOuter: 0x071E21, wall: 0xEAF1EE,
                   liquidTop: 0xD98A48, liquidBottom: 0x8E4E1F)
        case .paper:
            Colors(groundInner: 0xF4ECDC, groundOuter: 0xE7DCC4, wall: 0x1E1B18,
                   liquidTop: 0x2A3F5F, liquidBottom: 0x14213A)
        case .plum:
            Colors(groundInner: 0x3A1F4A, groundOuter: 0x1E0F2A, wall: 0xF1E9F4,
                   liquidTop: 0xF6B26B, liquidBottom: 0xE3703F)
        }
    }

    var isLightGround: Bool { colors.isLightGround }

    /// Colors for the non-Liquid-Glass backdrop. Light surfaces lift the
    /// canonical colors toward white, while dark surfaces deepen the ground;
    /// the named palette remains recognizable in both appearances.
    func flatSurface(isDark: Bool) -> FlatSurfaceColors {
        let colors = colors
        if isDark {
            return FlatSurfaceColors(
                base: colors.groundOuter.mixed(
                    with: 0x000000,
                    amount: colors.isLightGround ? 0.80 : 0.45
                ),
                primary: colors.liquidTop,
                secondary: colors.liquidBottom.mixed(with: colors.liquidTop, amount: 0.40),
                tertiary: colors.groundInner,
                decoration: colors.isLightGround ? colors.groundInner : colors.wall
            )
        }
        return FlatSurfaceColors(
            base: colors.groundInner.mixed(with: 0xFFFFFF, amount: 0.85),
            primary: colors.liquidTop.mixed(with: 0xFFFFFF, amount: 0.35),
            secondary: colors.liquidBottom.mixed(with: 0xFFFFFF, amount: 0.60),
            tertiary: colors.groundOuter.mixed(with: 0xFFFFFF, amount: 0.55),
            decoration: colors.groundOuter.mixed(with: 0x000000, amount: 0.15)
        )
    }

    struct Colors: Equatable {
        let groundInner: UInt32
        let groundOuter: UInt32
        let wall: UInt32
        let liquidTop: UInt32
        let liquidBottom: UInt32

        var isLightGround: Bool { ((groundOuter >> 16) & 0xFF) > 0xA0 }
    }
}

struct FlatSurfaceColors: Equatable, Hashable {
    let base: UInt32
    let primary: UInt32
    let secondary: UInt32
    let tertiary: UInt32
    let decoration: UInt32
}

extension UInt32 {
    fileprivate func mixed(with other: UInt32, amount: Double) -> UInt32 {
        func channel(shift: Int) -> UInt32 {
            let start = Double((self >> shift) & 0xFF)
            let end = Double((other >> shift) & 0xFF)
            return UInt32((start + (end - start) * amount).rounded()) << shift
        }
        return channel(shift: 16) | channel(shift: 8) | channel(shift: 0)
    }

    var swiftUIColor: Color {
        Color(
            red: Double((self >> 16) & 0xFF) / 255,
            green: Double((self >> 8) & 0xFF) / 255,
            blue: Double(self & 0xFF) / 255
        )
    }
}
