import Foundation

public struct Account: Codable, Equatable, Sendable {
    public let id: String
    public let provider: String
    public let label: String?
    public let enabled: Bool
    /// Display metric names the daemon stores for this account; empty means no
    /// filter. Payloads without the field decode to an empty list.
    public let metrics: [String]

    public init(
        id: String,
        provider: String,
        label: String?,
        enabled: Bool,
        metrics: [String] = []
    ) {
        self.id = id
        self.provider = provider
        self.label = label
        self.enabled = enabled
        self.metrics = metrics
    }

    private enum CodingKeys: String, CodingKey { case id, provider, label, enabled, metrics }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        provider = try container.decode(String.self, forKey: .provider)
        label = try container.decodeIfPresent(String.self, forKey: .label)
        enabled = try container.decode(Bool.self, forKey: .enabled)
        metrics = try container.decodeIfPresent([String].self, forKey: .metrics) ?? []
    }
}

public struct PartialFailure: Codable, Equatable, Sendable {
    public let scope: String
    public let message: String
}

public enum QueryOutcome<Payload: Sendable>: Sendable {
    case complete(Payload)
    case partial(Payload, failures: [PartialFailure])
    case unknown(String)

    public var data: Payload? {
        switch self {
        case .complete(let payload), .partial(let payload, _): payload
        case .unknown: nil
        }
    }
}

extension QueryOutcome: Equatable where Payload: Equatable {}

extension QueryOutcome: Codable where Payload: Codable {
    private enum CodingKeys: String, CodingKey { case outcome, data, failures }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let outcome = try container.decode(String.self, forKey: .outcome)
        switch outcome {
        case "complete":
            self = .complete(try container.decode(Payload.self, forKey: .data))
        case "partial":
            self = .partial(
                try container.decode(Payload.self, forKey: .data),
                failures: try container.decode([PartialFailure].self, forKey: .failures)
            )
        default:
            self = .unknown(outcome)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .complete(let payload):
            try container.encode("complete", forKey: .outcome)
            try container.encode(payload, forKey: .data)
        case .partial(let payload, let failures):
            try container.encode("partial", forKey: .outcome)
            try container.encode(payload, forKey: .data)
            try container.encode(failures, forKey: .failures)
        case .unknown(let outcome):
            try container.encode(outcome, forKey: .outcome)
        }
    }
}

public enum UsageWindowKind: Codable, Equatable, Sendable {
    case fiveHours
    case weekly
    case monthly
    case other(id: String, label: String)
    case unknown(String)

    private enum CodingKeys: String, CodingKey { case kind, id, label }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(String.self, forKey: .kind)
        switch kind {
        case "five_hours": self = .fiveHours
        case "weekly": self = .weekly
        case "monthly": self = .monthly
        case "other":
            self = .other(
                id: try container.decode(String.self, forKey: .id),
                label: try container.decode(String.self, forKey: .label)
            )
        default: self = .unknown(kind)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .fiveHours: try container.encode("five_hours", forKey: .kind)
        case .weekly: try container.encode("weekly", forKey: .kind)
        case .monthly: try container.encode("monthly", forKey: .kind)
        case .other(let id, let label):
            try container.encode("other", forKey: .kind)
            try container.encode(id, forKey: .id)
            try container.encode(label, forKey: .label)
        case .unknown(let kind): try container.encode(kind, forKey: .kind)
        }
    }
}

public enum MeasurementUnit: Codable, Equatable, Sendable {
    case requests
    case tokens
    case percent
    case credits
    case currency(code: String)
    case other(id: String, label: String)
    case unknown(String)

    private enum CodingKeys: String, CodingKey { case kind, code, id, label }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(String.self, forKey: .kind)
        switch kind {
        case "requests": self = .requests
        case "tokens": self = .tokens
        case "percent": self = .percent
        case "credits": self = .credits
        case "currency": self = .currency(code: try container.decode(String.self, forKey: .code))
        case "other":
            self = .other(
                id: try container.decode(String.self, forKey: .id),
                label: try container.decode(String.self, forKey: .label)
            )
        default: self = .unknown(kind)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .requests: try container.encode("requests", forKey: .kind)
        case .tokens: try container.encode("tokens", forKey: .kind)
        case .percent: try container.encode("percent", forKey: .kind)
        case .credits: try container.encode("credits", forKey: .kind)
        case .currency(let code):
            try container.encode("currency", forKey: .kind)
            try container.encode(code, forKey: .code)
        case .other(let id, let label):
            try container.encode("other", forKey: .kind)
            try container.encode(id, forKey: .id)
            try container.encode(label, forKey: .label)
        case .unknown(let kind): try container.encode(kind, forKey: .kind)
        }
    }
}

public struct UsageMeasurement: Codable, Equatable, Sendable {
    public let name: String
    public let used: Double
    public let limit: Double?
    public let unit: MeasurementUnit
}

public struct UsageWindow: Codable, Equatable, Sendable {
    public let window: UsageWindowKind
    public let resetsAt: Date?
    public let measurements: [UsageMeasurement]

    private enum CodingKeys: String, CodingKey {
        case window, measurements
        case resetsAt = "resets_at"
    }
}

public struct SubscriptionUsage: Codable, Equatable, Sendable {
    public let provider: String
    public let accountLabel: String?
    public let plan: String?
    public let subscriptionExpiresAt: Date?
    public let observedAt: Date
    public let windows: [UsageWindow]

    private enum CodingKeys: String, CodingKey {
        case provider, plan, windows
        case accountLabel = "account_label"
        case subscriptionExpiresAt = "subscription_expires_at"
        case observedAt = "observed_at"
    }
}

public enum SanitizedErrorPayload: Codable, Equatable, Sendable {
    case authenticationInvalid
    case rateLimited(retryAfterSeconds: UInt64?)
    case network
    case protocolIncompatible
    case unsupportedCapability
    case timeout
    case cancelled
    case providerNotFound
    case storage
    case unknown(String)

    private enum RateLimitKeys: String, CodingKey { case retryAfterSeconds = "retry_after_seconds" }

    public init(from decoder: Decoder) throws {
        if let value = try? decoder.singleValueContainer().decode(String.self) {
            self = Self.unitValue(for: value)
            return
        }
        let container = try decoder.container(keyedBy: AnyCodingKey.self)
        guard let key = container.allKeys.first else {
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "Empty error payload"))
        }
        if key.stringValue == "rate_limited" {
            let detail = try container.nestedContainer(keyedBy: RateLimitKeys.self, forKey: key)
            self = .rateLimited(retryAfterSeconds: try detail.decodeIfPresent(UInt64.self, forKey: .retryAfterSeconds))
        } else {
            self = .unknown(key.stringValue)
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .rateLimited(let retryAfterSeconds):
            var container = encoder.container(keyedBy: AnyCodingKey.self)
            let key = AnyCodingKey("rate_limited")
            var detail = container.nestedContainer(keyedBy: RateLimitKeys.self, forKey: key)
            try detail.encodeIfPresent(retryAfterSeconds, forKey: .retryAfterSeconds)
        default:
            var container = encoder.singleValueContainer()
            try container.encode(tag)
        }
    }

    private static func unitValue(for tag: String) -> Self {
        switch tag {
        case "authentication_invalid": .authenticationInvalid
        case "network": .network
        case "protocol_incompatible": .protocolIncompatible
        case "unsupported_capability": .unsupportedCapability
        case "timeout": .timeout
        case "cancelled": .cancelled
        case "provider_not_found": .providerNotFound
        case "storage": .storage
        default: .unknown(tag)
        }
    }

    private var tag: String {
        switch self {
        case .authenticationInvalid: "authentication_invalid"
        case .rateLimited: "rate_limited"
        case .network: "network"
        case .protocolIncompatible: "protocol_incompatible"
        case .unsupportedCapability: "unsupported_capability"
        case .timeout: "timeout"
        case .cancelled: "cancelled"
        case .providerNotFound: "provider_not_found"
        case .storage: "storage"
        case .unknown(let tag): tag
        }
    }
}

public struct SnapshotPayload: Codable, Equatable, Sendable {
    public let accountId: String
    public let usage: QueryOutcome<SubscriptionUsage>
    public let lastSuccessAt: Date
    public let stale: Bool
    public let lastError: SanitizedErrorPayload?
    public let lastErrorAt: Date?
    /// Display metric names stored for the account when the snapshot was read;
    /// empty means no filter. Payloads without the field decode to empty.
    public let metrics: [String]

    private enum CodingKeys: String, CodingKey {
        case usage, stale, metrics
        case accountId = "account_id"
        case lastSuccessAt = "last_success_at"
        case lastError = "last_error"
        case lastErrorAt = "last_error_at"
    }

    public init(
        accountId: String,
        usage: QueryOutcome<SubscriptionUsage>,
        lastSuccessAt: Date,
        stale: Bool,
        lastError: SanitizedErrorPayload?,
        lastErrorAt: Date?,
        metrics: [String] = []
    ) {
        self.accountId = accountId
        self.usage = usage
        self.lastSuccessAt = lastSuccessAt
        self.stale = stale
        self.lastError = lastError
        self.lastErrorAt = lastErrorAt
        self.metrics = metrics
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        accountId = try container.decode(String.self, forKey: .accountId)
        usage = try container.decode(QueryOutcome<SubscriptionUsage>.self, forKey: .usage)
        lastSuccessAt = try container.decode(Date.self, forKey: .lastSuccessAt)
        stale = try container.decode(Bool.self, forKey: .stale)
        lastError = try container.decodeIfPresent(SanitizedErrorPayload.self, forKey: .lastError)
        lastErrorAt = try container.decodeIfPresent(Date.self, forKey: .lastErrorAt)
        metrics = try container.decodeIfPresent([String].self, forKey: .metrics) ?? []
    }
}

public struct ProbePayload: Codable, Equatable, Sendable {
    public let accountId: String
    public let usage: QueryOutcome<SubscriptionUsage>
    /// Display metric names stored for the account when the probe ran; empty
    /// means no filter. Payloads without the field decode to empty.
    public let metrics: [String]

    private enum CodingKeys: String, CodingKey {
        case usage, metrics
        case accountId = "account_id"
    }

    public init(
        accountId: String,
        usage: QueryOutcome<SubscriptionUsage>,
        metrics: [String] = []
    ) {
        self.accountId = accountId
        self.usage = usage
        self.metrics = metrics
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        accountId = try container.decode(String.self, forKey: .accountId)
        usage = try container.decode(QueryOutcome<SubscriptionUsage>.self, forKey: .usage)
        metrics = try container.decodeIfPresent([String].self, forKey: .metrics) ?? []
    }
}

public enum ProbeResult: Equatable, Sendable {
    case accepted
    case completed(ProbePayload)
}

public enum CredentialBackendID: Codable, Equatable, Sendable {
    case linuxSecretService, macOSKeychain, windowsCredentialManager, fileFallback, otherPlatform
    case unknown(String)

    public init(from decoder: Decoder) throws {
        let tag = try decoder.singleValueContainer().decode(String.self)
        self = switch tag {
        case "linux_secret_service": .linuxSecretService
        case "macos_keychain": .macOSKeychain
        case "windows_credential_manager": .windowsCredentialManager
        case "file_fallback": .fileFallback
        case "other_platform": .otherPlatform
        default: .unknown(tag)
        }
    }

    public func encode(to encoder: Encoder) throws {
        let tag = switch self {
        case .linuxSecretService: "linux_secret_service"
        case .macOSKeychain: "macos_keychain"
        case .windowsCredentialManager: "windows_credential_manager"
        case .fileFallback: "file_fallback"
        case .otherPlatform: "other_platform"
        case .unknown(let tag): tag
        }
        var container = encoder.singleValueContainer()
        try container.encode(tag)
    }
}

public struct AccountStatusPayload: Codable, Equatable, Sendable {
    public let accountId: String
    public let provider: String
    public let enabled: Bool
    public let inFlight: Bool
    public let consecutiveFailures: UInt32
    public let nextProbeAt: Date?
    public let hasSnapshot: Bool
    public let stale: Bool
    public let lastError: SanitizedErrorPayload?

    private enum CodingKeys: String, CodingKey {
        case provider, enabled, stale
        case accountId = "account_id"
        case inFlight = "in_flight"
        case consecutiveFailures = "consecutive_failures"
        case nextProbeAt = "next_probe_at"
        case hasSnapshot = "has_snapshot"
        case lastError = "last_error"
    }
}

public struct DaemonStatusPayload: Codable, Equatable, Sendable {
    public let version: UInt16
    public let shuttingDown: Bool
    public let accounts: [AccountStatusPayload]
    public let credentialBackend: CredentialBackendID

    private enum CodingKeys: String, CodingKey {
        case version, accounts
        case shuttingDown = "shutting_down"
        case credentialBackend = "credential_backend"
    }

    public init(
        version: UInt16,
        shuttingDown: Bool,
        accounts: [AccountStatusPayload],
        credentialBackend: CredentialBackendID
    ) {
        self.version = version
        self.shuttingDown = shuttingDown
        self.accounts = accounts
        self.credentialBackend = credentialBackend
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        version = try container.decodeIfPresent(UInt16.self, forKey: .version) ?? 0
        shuttingDown = try container.decode(Bool.self, forKey: .shuttingDown)
        accounts = try container.decode([AccountStatusPayload].self, forKey: .accounts)
        credentialBackend = try container.decode(CredentialBackendID.self, forKey: .credentialBackend)
    }
}

struct AnyCodingKey: CodingKey {
    let stringValue: String
    let intValue: Int? = nil

    init(_ stringValue: String) { self.stringValue = stringValue }
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { return nil }
}
