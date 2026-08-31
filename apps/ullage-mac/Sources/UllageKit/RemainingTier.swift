public enum RemainingTier: Equatable, Sendable {
    case healthy
    case caution
    case low
    case critical

    public init(ratio: Double) {
        self = if ratio > 0.50 {
            .healthy
        } else if ratio > 0.25 {
            .caution
        } else if ratio > 0.10 {
            .low
        } else {
            .critical
        }
    }
}
