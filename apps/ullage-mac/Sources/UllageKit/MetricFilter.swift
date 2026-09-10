import Foundation

/// A validated set of display-metric names selecting which summary rows a
/// projection shows.
///
/// Names are trimmed, matched case-insensitively (ASCII, like the daemon's
/// shared filter) against exact display names, and deduplicated while keeping
/// the first spelling. An empty filter is inactive, which means "show every
/// row". Matching ignores the window a row belongs to.
public struct MetricFilter: Equatable, Sendable {
    /// Upper bound on the number of names one filter accepts, shared with the
    /// daemon's shared filter contract.
    public static let maximumNames = 64
    /// Upper bound on the character count of one name, shared with the
    /// daemon's shared filter contract.
    public static let maximumNameCharacters = 128

    /// Why a metric filter value was rejected.
    public enum ValidationError: Error, Equatable, CustomStringConvertible, LocalizedError {
        case emptyName
        case nameTooLong
        case tooManyNames
        case unsafeCharacter

        public var description: String {
            switch self {
            case .emptyName:
                "metric filter names must not be empty"
            case .nameTooLong:
                "metric filter names must be at most \(MetricFilter.maximumNameCharacters) characters"
            case .tooManyNames:
                "a metric filter accepts at most \(MetricFilter.maximumNames) names"
            case .unsafeCharacter:
                "metric filter names must not contain control or bidirectional text characters"
            }
        }

        public var errorDescription: String? { description }
    }

    /// The normalized names, in first-seen order.
    public let names: [String]

    /// A filter that shows every row.
    public static let inactive = MetricFilter(uncheckedNames: [])

    /// Bypasses validation for constants the type itself controls.
    private init(uncheckedNames: [String]) {
        names = uncheckedNames
    }

    /// Validates and normalizes the given names.
    public init(names: [String]) throws {
        guard names.count <= Self.maximumNames else { throw ValidationError.tooManyNames }
        var normalized: [String] = []
        normalized.reserveCapacity(names.count)
        for name in names {
            let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !name.isEmpty else { throw ValidationError.emptyName }
            guard name.count <= Self.maximumNameCharacters else { throw ValidationError.nameTooLong }
            guard !name.unicodeScalars.contains(where: isUnsafeMetricFilterCharacter) else {
                throw ValidationError.unsafeCharacter
            }
            guard !normalized.contains(where: { asciiCaseInsensitiveEqual($0, name) }) else { continue }
            normalized.append(name)
        }
        self.names = normalized
    }

    /// Builds a filter from a persisted list without failing.
    ///
    /// Persisted values were validated by the daemon before storage, so a list
    /// that fails validation here can only come from corrupt or foreign data.
    /// Such a list must never hide rows; it becomes the inactive filter.
    public init(persistedNames: [String]) {
        self = (try? MetricFilter(names: persistedNames)) ?? .inactive
    }

    /// Whether the filter selects rows. An inactive filter shows every row.
    public var isActive: Bool { !names.isEmpty }

    /// Case-insensitive (ASCII) exact match against a display name.
    ///
    /// An inactive filter matches nothing, matching the daemon's shared
    /// filter; callers that want "show everything" check `isActive` first.
    public func matches(_ display: String) -> Bool {
        guard isActive else { return false }
        let display = display.trimmingCharacters(in: .whitespacesAndNewlines)
        return names.contains { asciiCaseInsensitiveEqual($0, display) }
    }

    /// The filter's name equality rule, for callers that need to group or
    /// compare display names the way matching does.
    public static func namesAreEquivalent(_ lhs: String, _ rhs: String) -> Bool {
        asciiCaseInsensitiveEqual(lhs, rhs)
    }
}

/// The daemon compares with `eq_ignore_ascii_case`; mirroring it keeps the
/// client from matching names the daemon would treat as different.
private func asciiCaseInsensitiveEqual(_ lhs: String, _ rhs: String) -> Bool {
    let left = Array(lhs.utf8)
    let right = Array(rhs.utf8)
    guard left.count == right.count else { return false }
    for (lhsByte, rhsByte) in zip(left, right) {
        guard asciiLowercased(lhsByte) == asciiLowercased(rhsByte) else { return false }
    }
    return true
}

private func asciiLowercased(_ byte: UInt8) -> UInt8 {
    (0x41...0x5A).contains(byte) ? byte + 0x20 : byte
}

/// Mirrors the daemon's unsafe-character rule: Unicode control characters plus
/// the bidirectional formatting characters that could reorder a display name.
private func isUnsafeMetricFilterCharacter(_ scalar: Unicode.Scalar) -> Bool {
    if CharacterSet.controlCharacters.contains(scalar) { return true }
    switch scalar.value {
    case 0x061C, 0x200E, 0x200F, 0x202A...0x202E, 0x2066...0x2069:
        return true
    default:
        return false
    }
}
