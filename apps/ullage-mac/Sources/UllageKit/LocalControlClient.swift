import Darwin
import Foundation

public enum LocalControlError: Error, Equatable, Sendable, CustomStringConvertible, LocalizedError {
    case unavailable(String)
    case invalidSocket
    case socketOwner(expected: uid_t, actual: uid_t)
    case socketMode(mode_t)
    case peerOwner(expected: uid_t, actual: uid_t)
    case timeout
    case responseTooLarge
    case invalidFraming
    case protocolMismatch(client: UInt16, server: UInt16)
    case requestMismatch
    case unexpectedResult(String)
    case server(String)
    case decoding

    public var description: String {
        switch self {
        case .unavailable(let message): message
        case .invalidSocket: "local service socket is invalid"
        case .socketOwner: "local service socket has an unexpected owner"
        case .socketMode: "local service socket permissions are not private"
        case .peerOwner: "local service peer has an unexpected owner"
        case .timeout: "local service did not respond in time"
        case .responseTooLarge: "local service response exceeds 1 MiB"
        case .invalidFraming: "local service returned invalid framing"
        case .protocolMismatch(let client, let server):
            "protocol mismatch (client \(client), server \(server))"
        case .requestMismatch: "local service returned a mismatched request id"
        case .unexpectedResult(let result): "local service returned unexpected result \(result)"
        case .server(let kind): kind
        case .decoding: "local service response decoding failed"
        }
    }

    public var errorDescription: String? { description }
}

public struct ProviderDescriptor: Codable, Equatable, Sendable, Identifiable {
    public let id: String
    public let displayName: String
    public let capabilities: [String]

    private enum CodingKeys: String, CodingKey {
        case id, capabilities
        case displayName = "display_name"
    }
}

public enum AuthenticationMethod: Equatable, Sendable {
    case browserOAuth
    case deviceCode
    case apiToken
    case sessionImport
    case other(String)
}

extension AuthenticationMethod: Codable {
    public init(from decoder: Decoder) throws {
        if let value = try? decoder.singleValueContainer().decode(String.self) {
            self = switch value {
            case "browser_o_auth": .browserOAuth
            case "device_code": .deviceCode
            case "api_token": .apiToken
            case "session_import": .sessionImport
            default: .other(value)
            }
            return
        }
        let container = try decoder.container(keyedBy: AnyCodingKey.self)
        guard let key = container.allKeys.first, key.stringValue == "other" else {
            throw DecodingError.dataCorrupted(
                .init(codingPath: decoder.codingPath, debugDescription: "Unknown auth method")
            )
        }
        self = .other(try container.decode(String.self, forKey: key))
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .browserOAuth, .deviceCode, .apiToken, .sessionImport:
            var container = encoder.singleValueContainer()
            try container.encode(snakeCaseValue)
        case .other(let value):
            var container = encoder.container(keyedBy: AnyCodingKey.self)
            try container.encode(value, forKey: AnyCodingKey("other"))
        }
    }

    var snakeCaseValue: String {
        switch self {
        case .browserOAuth: "browser_o_auth"
        case .deviceCode: "device_code"
        case .apiToken: "api_token"
        case .sessionImport: "session_import"
        case .other(let value): value
        }
    }
}

public struct AuthenticationInput: Codable, Equatable, Sendable {
    public let prompt: String
    public let secret: Bool
}

public struct AuthenticationChallenge: Codable, Equatable, Sendable {
    public let flowId: String
    public let method: AuthenticationMethod
    public let verificationURI: String?
    public let userCode: String?
    public let expiresAt: Date?
    public let input: AuthenticationInput?

    private enum CodingKeys: String, CodingKey {
        case method, input
        case flowId = "flow_id"
        case verificationURI = "verification_uri"
        case userCode = "user_code"
        case expiresAt = "expires_at"
    }
}

public enum AuthenticationState: Equatable, Sendable {
    case notAuthenticated
    case pending(flowId: String, expiresAt: Date?)
    case authenticated(accountLabel: String?, expiresAt: Date?)
    case invalid(reason: String)
}

extension AuthenticationState: Codable {
    private enum CodingKeys: String, CodingKey {
        case state, reason
        case flowId = "flow_id"
        case expiresAt = "expires_at"
        case accountLabel = "account_label"
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(String.self, forKey: .state) {
        case "not_authenticated": self = .notAuthenticated
        case "pending":
            self = .pending(
                flowId: try container.decode(String.self, forKey: .flowId),
                expiresAt: try container.decodeIfPresent(Date.self, forKey: .expiresAt)
            )
        case "authenticated":
            self = .authenticated(
                accountLabel: try container.decodeIfPresent(String.self, forKey: .accountLabel),
                expiresAt: try container.decodeIfPresent(Date.self, forKey: .expiresAt)
            )
        case "invalid": self = .invalid(reason: try container.decode(String.self, forKey: .reason))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .state,
                in: container,
                debugDescription: "Unknown authentication state"
            )
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .notAuthenticated:
            try container.encode("not_authenticated", forKey: .state)
        case .pending(let flowId, let expiresAt):
            try container.encode("pending", forKey: .state)
            try container.encode(flowId, forKey: .flowId)
            try container.encodeIfPresent(expiresAt, forKey: .expiresAt)
        case .authenticated(let accountLabel, let expiresAt):
            try container.encode("authenticated", forKey: .state)
            try container.encodeIfPresent(accountLabel, forKey: .accountLabel)
            try container.encodeIfPresent(expiresAt, forKey: .expiresAt)
        case .invalid(let reason):
            try container.encode("invalid", forKey: .state)
            try container.encode(reason, forKey: .reason)
        }
    }
}

public final class LocalControlClient: @unchecked Sendable {
    public static let protocolVersion: UInt16 = 9
    public static let maximumResponseBytes = 1024 * 1024

    private let socketURL: URL
    private let timeout: TimeInterval
    private let expectedUID: uid_t
    private let transport: @Sendable (Data) throws -> Data

    public init(
        socketURL: URL,
        timeout: TimeInterval = 10,
        expectedUID: uid_t = geteuid()
    ) {
        self.socketURL = socketURL
        self.timeout = timeout
        self.expectedUID = expectedUID
        self.transport = { request in
            try Self.exchange(
                request,
                socketURL: socketURL,
                timeout: timeout,
                expectedUID: expectedUID
            )
        }
    }

    init(transport: @escaping @Sendable (Data) throws -> Data) {
        socketURL = URL(fileURLWithPath: "/unused")
        timeout = 1
        expectedUID = 0
        self.transport = transport
    }

    public func status() async throws -> DaemonStatusPayload {
        let payload: DaemonStatusPayload = try await send(
            command: ["command": "daemon_status"],
            expectedResult: "daemon_status"
        )
        return DaemonStatusPayload(
            version: Self.protocolVersion,
            shuttingDown: payload.shuttingDown,
            accounts: payload.accounts,
            credentialBackend: payload.credentialBackend
        )
    }

    public func accounts() async throws -> [Account] {
        try await send(command: ["command": "list_accounts"], expectedResult: "accounts")
    }

    public func usage() async throws -> [SnapshotPayload] {
        try await send(
            command: ["command": "show", "account_id": NSNull()],
            expectedResult: "snapshots"
        )
    }

    public func probe(accountId: String, wait: Bool) async throws -> ProbeResult {
        if wait {
            let payload: ProbePayload = try await send(
                command: ["command": "probe", "account_id": accountId, "wait": true],
                expectedResult: "probe"
            )
            return .completed(payload)
        }
        try await sendAck(command: [
            "command": "probe", "account_id": accountId, "wait": false,
        ])
        return .accepted
    }

    public func providers() async throws -> [ProviderDescriptor] {
        try await send(command: ["command": "list_providers"], expectedResult: "providers")
    }

    public func addAccount(provider: String, label: String?) async throws -> Account {
        let labelValue: Any = label.map { $0 as Any } ?? NSNull()
        let command: [String: Any] = [
            "command": "add_account",
            "provider": provider,
            "label": labelValue,
        ]
        let account: Account = try await send(command: command, expectedResult: "account")
        return account
    }

    public func setAccountEnabled(_ account: String, enabled: Bool) async throws -> Account {
        try await send(
            command: [
                "command": "set_account_enabled",
                "account": account,
                "enabled": enabled,
            ],
            expectedResult: "account"
        )
    }

    public func setAccountLabel(_ account: String, label: String?) async throws -> Account {
        try await send(
            command: [
                "command": "set_account_label",
                "account": account,
                "label": label ?? NSNull(),
            ],
            expectedResult: "account"
        )
    }

    public func removeAccount(_ account: String) async throws {
        try await sendAck(command: ["command": "remove_account", "account": account])
    }

    public func startAuthentication(
        provider: String,
        account: String,
        method: AuthenticationMethod? = nil
    ) async throws -> AuthenticationChallenge {
        let methodValue: Any = method.map { $0.snakeCaseValue as Any } ?? NSNull()
        let request: [String: Any] = ["method": methodValue]
        let command: [String: Any] = [
            "command": "start_auth",
            "provider": provider,
            "account": account,
            "request": request,
        ]
        let challenge: AuthenticationChallenge = try await send(
            command: command,
            expectedResult: "auth_challenge"
        )
        return challenge
    }

    public func completeAuthentication(
        provider: String,
        account: String,
        flowId: String,
        input: String?,
        redirectURI: String? = nil
    ) async throws -> AuthenticationState {
        let inputValue: Any = input.map { $0 as Any } ?? NSNull()
        let redirectValue: Any = redirectURI.map { $0 as Any } ?? NSNull()
        let request: [String: Any] = [
            "flow_id": flowId,
            "authorization_code": inputValue,
            "redirect_uri": redirectValue,
        ]
        let command: [String: Any] = [
            "command": "complete_auth",
            "provider": provider,
            "account": account,
            "request": request,
        ]
        let state: AuthenticationState = try await send(
            command: command,
            expectedResult: "auth_state"
        )
        return state
    }

    public func authenticationStatus(provider: String, account: String) async throws -> AuthenticationState {
        try await send(
            command: ["command": "auth_status", "provider": provider, "account": account],
            expectedResult: "auth_state"
        )
    }

    public func logout(provider: String, account: String, accountLabel: String?) async throws {
        let labelValue: Any = accountLabel.map { $0 as Any } ?? NSNull()
        try await sendAck(command: [
            "command": "logout",
            "provider": provider,
            "account": account,
            "request": ["account_label": labelValue],
        ])
    }

    private func send<Payload: Decodable>(
        command: [String: Any],
        expectedResult: String
    ) async throws -> Payload {
        let response = try await response(command: command)
        guard response.result == expectedResult else {
            throw response.result == "error"
                ? LocalControlError.server(response.errorKind ?? "local service error")
                : LocalControlError.unexpectedResult(response.result)
        }
        guard let payload = response.payload else { throw LocalControlError.decoding }
        do {
            return try UllageJSON.makeDecoder().decode(Payload.self, from: payload)
        } catch {
            throw LocalControlError.decoding
        }
    }

    private func sendAck(command: [String: Any]) async throws {
        let response = try await response(command: command)
        guard response.result == "ack" else {
            throw response.result == "error"
                ? LocalControlError.server(response.errorKind ?? "local service error")
                : LocalControlError.unexpectedResult(response.result)
        }
    }

    private func response(command: [String: Any]) async throws -> ParsedControlResponse {
        let requestID = UUID().uuidString
        let request = try JSONSerialization.data(withJSONObject: [
            "version": Self.protocolVersion,
            "request_id": requestID,
            "command": command,
            "diagnostics": false,
        ]) + Data([0x0a])
        let transport = self.transport
        let data = try await Task.detached { try transport(request) }.value
        let parsed = try Self.parseResponse(data)
        guard parsed.version == Self.protocolVersion else {
            throw LocalControlError.protocolMismatch(
                client: Self.protocolVersion,
                server: parsed.version
            )
        }
        guard parsed.requestID == requestID else { throw LocalControlError.requestMismatch }
        return parsed
    }

    static func parseResponse(_ data: Data) throws -> ParsedControlResponse {
        guard data.count <= maximumResponseBytes else { throw LocalControlError.responseTooLarge }
        guard data.last == 0x0a, data.dropLast().firstIndex(of: 0x0a) == nil else {
            throw LocalControlError.invalidFraming
        }
        let object: [String: Any]
        do {
            object = try JSONSerialization.jsonObject(with: data.dropLast()) as? [String: Any] ?? [:]
        } catch {
            throw LocalControlError.decoding
        }
        guard let versionNumber = object["version"] as? NSNumber,
              CFGetTypeID(versionNumber) != CFBooleanGetTypeID(),
              versionNumber.doubleValue.isFinite,
              versionNumber.doubleValue.rounded(.towardZero) == versionNumber.doubleValue,
              versionNumber.doubleValue >= 0,
              versionNumber.doubleValue <= Double(UInt16.max),
              let requestID = object["request_id"] as? String,
              let resultObject = object["result"] as? [String: Any],
              let result = resultObject["result"] as? String else {
            throw LocalControlError.decoding
        }
        let payload = resultObject["payload"].flatMap { try? JSONSerialization.data(withJSONObject: $0) }
        let errorKind = (resultObject["payload"] as? [String: Any])?["kind"] as? String
        return ParsedControlResponse(
            version: versionNumber.uint16Value,
            requestID: requestID,
            result: result,
            payload: payload,
            errorKind: errorKind
        )
    }

    private static func exchange(
        _ request: Data,
        socketURL: URL,
        timeout: TimeInterval,
        expectedUID: uid_t
    ) throws -> Data {
        let path = socketURL.path
        let before = try validateSocket(path: path, expectedUID: expectedUID)
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw unavailable(errno) }
        defer { Darwin.close(descriptor) }

        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else {
            throw unavailable(errno)
        }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8CString)
        guard pathBytes.count <= MemoryLayout.size(ofValue: address.sun_path) else {
            throw LocalControlError.unavailable("local service socket path is too long")
        }
        withUnsafeMutableBytes(of: &address.sun_path) { destination in
            destination.copyBytes(from: pathBytes.map { UInt8(bitPattern: $0) })
        }
        let connected = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(descriptor, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if connected != 0 {
            guard errno == EINPROGRESS else { throw unavailable(errno) }
            try wait(descriptor: descriptor, events: Int16(POLLOUT), timeout: timeout)
            var socketError: Int32 = 0
            var length = socklen_t(MemoryLayout<Int32>.size)
            guard getsockopt(descriptor, SOL_SOCKET, SO_ERROR, &socketError, &length) == 0,
                  socketError == 0 else {
                throw unavailable(socketError == 0 ? errno : socketError)
            }
        }
        guard fcntl(descriptor, F_SETFL, flags) == 0 else { throw unavailable(errno) }

        var peerUID: uid_t = 0
        var peerGID: gid_t = 0
        guard getpeereid(descriptor, &peerUID, &peerGID) == 0 else { throw unavailable(errno) }
        guard peerUID == expectedUID else {
            throw LocalControlError.peerOwner(expected: expectedUID, actual: peerUID)
        }
        let after = try validateSocket(path: path, expectedUID: expectedUID)
        guard before.st_dev == after.st_dev, before.st_ino == after.st_ino else {
            throw LocalControlError.invalidSocket
        }

        try writeAll(request, descriptor: descriptor, timeout: timeout)
        return try readLine(descriptor: descriptor, timeout: timeout)
    }

    private static func validateSocket(path: String, expectedUID: uid_t) throws -> stat {
        var metadata = stat()
        guard lstat(path, &metadata) == 0 else { throw unavailable(errno) }
        guard metadata.st_mode & S_IFMT == S_IFSOCK else { throw LocalControlError.invalidSocket }
        guard metadata.st_uid == expectedUID else {
            throw LocalControlError.socketOwner(expected: expectedUID, actual: metadata.st_uid)
        }
        let mode = metadata.st_mode & 0o777
        guard mode == 0o600 else { throw LocalControlError.socketMode(mode) }
        return metadata
    }

    private static func writeAll(_ data: Data, descriptor: Int32, timeout: TimeInterval) throws {
        var offset = 0
        try data.withUnsafeBytes { bytes in
            while offset < data.count {
                try wait(descriptor: descriptor, events: Int16(POLLOUT), timeout: timeout)
                let written = Darwin.write(
                    descriptor,
                    bytes.baseAddress!.advanced(by: offset),
                    data.count - offset
                )
                if written < 0 {
                    if errno == EINTR { continue }
                    throw unavailable(errno)
                }
                offset += written
            }
        }
    }

    private static func readLine(descriptor: Int32, timeout: TimeInterval) throws -> Data {
        var response = Data()
        var buffer = [UInt8](repeating: 0, count: 8192)
        while true {
            try wait(descriptor: descriptor, events: Int16(POLLIN), timeout: timeout)
            let count = Darwin.read(descriptor, &buffer, buffer.count)
            if count < 0 {
                if errno == EINTR { continue }
                throw unavailable(errno)
            }
            guard count > 0 else { throw LocalControlError.invalidFraming }
            response.append(buffer, count: count)
            guard response.count <= maximumResponseBytes else {
                throw LocalControlError.responseTooLarge
            }
            if response.last == 0x0a { return response }
            if response.contains(0x0a) { throw LocalControlError.invalidFraming }
        }
    }

    private static func wait(descriptor: Int32, events: Int16, timeout: TimeInterval) throws {
        var item = pollfd(fd: descriptor, events: events, revents: 0)
        let milliseconds = Int32(max(1, min(timeout * 1000, Double(Int32.max))))
        while true {
            let result = Darwin.poll(&item, 1, milliseconds)
            if result > 0 {
                if item.revents & events != 0 { return }
                throw unavailable(ECONNRESET)
            }
            if result == 0 { throw LocalControlError.timeout }
            if errno != EINTR { throw unavailable(errno) }
        }
    }

    private static func unavailable(_ code: Int32) -> LocalControlError {
        LocalControlError.unavailable(String(cString: strerror(code)))
    }
}

struct ParsedControlResponse: Equatable {
    let version: UInt16
    let requestID: String
    let result: String
    let payload: Data?
    let errorKind: String?
}
