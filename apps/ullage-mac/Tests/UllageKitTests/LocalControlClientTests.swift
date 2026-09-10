import Darwin
import Foundation
import Testing
@testable import UllageKit

@Suite(.serialized)
struct LocalControlClientTests {
    @Test func sendsVersionedNewlineFramedRequestsAndChecksRequestIDs() async throws {
        let client = LocalControlClient { request in
            #expect(request.last == 0x0a)
            let object = try #require(
                JSONSerialization.jsonObject(with: request.dropLast()) as? [String: Any]
            )
            #expect((object["version"] as? NSNumber)?.uint16Value == 10)
            let requestID = try #require(object["request_id"] as? String)
            let command = try #require(object["command"] as? [String: Any])
            #expect(command["command"] as? String == "daemon_status")
            return try response(
                requestID: requestID,
                result: "daemon_status",
                payload: [
                    "shutting_down": false,
                    "accounts": [],
                    "credential_backend": "macos_keychain",
                ]
            )
        }
        let status = try await client.status()
        #expect(LocalControlClient.protocolVersion == 10)
        #expect(status.version == 10)
        #expect(status.credentialBackend == .macOSKeychain)
    }

    @Test func supportsAccountCRUDAndKeepsAuthInputInsideTheRequestBody() async throws {
        let recorder = RequestRecorder()
        let client = LocalControlClient { request in
            let object = try #require(
                JSONSerialization.jsonObject(with: request.dropLast()) as? [String: Any]
            )
            recorder.append(object)
            let requestID = try #require(object["request_id"] as? String)
            let command = try #require(object["command"] as? [String: Any])
            switch command["command"] as? String {
            case "add_account":
                return try response(
                    requestID: requestID,
                    result: "account",
                    payload: ["id": "account-1", "provider": "cursor", "enabled": true]
                )
            case "start_auth":
                return try response(
                    requestID: requestID,
                    result: "auth_challenge",
                    payload: [
                        "flow_id": "flow-1",
                        "method": "api_token",
                        "verification_uri": "https://cursor.com/settings",
                        "input": ["prompt": "API key", "secret": true],
                    ]
                )
            case "complete_auth":
                return try response(
                    requestID: requestID,
                    result: "auth_state",
                    payload: ["state": "authenticated", "account_label": "Personal"]
                )
            default:
                return try response(requestID: requestID, result: "ack", payload: nil)
            }
        }

        let account = try await client.addAccount(provider: "cursor", label: nil)
        let challenge = try await client.startAuthentication(provider: "cursor", account: account.id)
        #expect(challenge.method == .apiToken)
        let state = try await client.completeAuthentication(
            provider: "cursor",
            account: account.id,
            flowId: challenge.flowId,
            input: "secret-fixture"
        )
        #expect(state == .authenticated(accountLabel: "Personal", expiresAt: nil))

        let complete = try #require(recorder.values.last?["command"] as? [String: Any])
        let input = try #require(complete["request"] as? [String: Any])
        #expect(input["authorization_code"] as? String == "secret-fixture")
    }

    @Test func rejectsBadFramingOversizedResponsesAndMismatchedHeaders() throws {
        #expect(throws: LocalControlError.invalidFraming) {
            _ = try LocalControlClient.parseResponse(Data("{}".utf8))
        }
        #expect(throws: LocalControlError.invalidFraming) {
            _ = try LocalControlClient.parseResponse(Data("{}\n{}\n".utf8))
        }
        #expect(throws: LocalControlError.responseTooLarge) {
            _ = try LocalControlClient.parseResponse(
                Data(repeating: 0x20, count: LocalControlClient.maximumResponseBytes + 1)
            )
        }
        for invalidVersion in ["-1", "9.5", "65545", "true"] {
            let response = Data(
                "{\"version\":\(invalidVersion),\"request_id\":\"id\",\"result\":{\"result\":\"ack\"}}\n".utf8
            )
            #expect(throws: LocalControlError.decoding) {
                _ = try LocalControlClient.parseResponse(response)
            }
        }
    }

    @Test func exposesActionableLocalizedErrors() {
        let error = LocalControlError.socketMode(0o666)
        #expect(error.localizedDescription == error.description)
    }

    @Test func rejectsProtocolAndRequestIDMismatchesAndMapsServerErrors() async throws {
        let wrongVersion = LocalControlClient { request in
            let object = try #require(
                JSONSerialization.jsonObject(with: request.dropLast()) as? [String: Any]
            )
            return try response(
                version: 11,
                requestID: try #require(object["request_id"] as? String),
                result: "accounts",
                payload: []
            )
        }
        await #expect(throws: LocalControlError.protocolMismatch(client: 10, server: 11)) {
            _ = try await wrongVersion.accounts()
        }

        let wrongID = LocalControlClient { _ in
            try response(requestID: "other", result: "accounts", payload: [])
        }
        await #expect(throws: LocalControlError.requestMismatch) {
            _ = try await wrongID.accounts()
        }

        let serverError = LocalControlClient { request in
            let object = try #require(
                JSONSerialization.jsonObject(with: request.dropLast()) as? [String: Any]
            )
            return try response(
                requestID: try #require(object["request_id"] as? String),
                result: "error",
                payload: ["kind": "storage"]
            )
        }
        await #expect(throws: LocalControlError.server("storage")) {
            _ = try await serverError.accounts()
        }
    }

    @Test func persistsAccountMetricsAndMapsValidationErrors() async throws {
        let recorder = RequestRecorder()
        let client = LocalControlClient { request in
            let object = try #require(
                JSONSerialization.jsonObject(with: request.dropLast()) as? [String: Any]
            )
            recorder.append(object)
            let requestID = try #require(object["request_id"] as? String)
            let command = try #require(object["command"] as? [String: Any])
            let metrics = try #require(command["metrics"] as? [String])
            if metrics.isEmpty {
                return try response(
                    requestID: requestID,
                    result: "error",
                    payload: ["kind": "invalid_account_metrics"]
                )
            }
            return try response(
                requestID: requestID,
                result: "account",
                payload: [
                    "id": "account-1",
                    "provider": "cursor",
                    "enabled": true,
                    "metrics": metrics,
                ]
            )
        }

        let updated = try await client.setAccountMetrics("account-1", metrics: ["usage", "Codex"])
        #expect(updated.metrics == ["usage", "Codex"])
        let recorded = try #require(recorder.values.first)
        let recordedCommand = try #require(recorded["command"] as? [String: Any])
        #expect(recordedCommand["command"] as? String == "set_account_metrics")
        #expect(recordedCommand["account"] as? String == "account-1")
        #expect(recordedCommand["metrics"] as? [String] == ["usage", "Codex"])

        await #expect(throws: LocalControlError.server("invalid_account_metrics")) {
            _ = try await client.setAccountMetrics("account-1", metrics: [])
        }
    }

    @Test func exchangesWithAPrivateSameUserUnixSocket() async throws {
        let server = try UnixTestServer { request in
            let object = try #require(
                JSONSerialization.jsonObject(with: request.dropLast()) as? [String: Any]
            )
            return try response(
                requestID: try #require(object["request_id"] as? String),
                result: "accounts",
                payload: []
            )
        }
        defer { server.stop() }

        let accounts = try await LocalControlClient(
            socketURL: server.socketURL,
            timeout: 1
        ).accounts()
        #expect(accounts.isEmpty)
    }

    @Test func rejectsNonSocketsAndNonPrivateSocketModes() async throws {
        let directory = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let regularFile = directory.appendingPathComponent("regular")
        try Data().write(to: regularFile)
        await #expect(throws: LocalControlError.invalidSocket) {
            _ = try await LocalControlClient(socketURL: regularFile).accounts()
        }

        let server = try UnixTestServer(mode: 0o666) { _ in Data() }
        defer { server.stop() }
        await #expect(throws: LocalControlError.socketMode(0o666)) {
            _ = try await LocalControlClient(socketURL: server.socketURL).accounts()
        }
    }

    @Test func timesOutWhenThePeerDoesNotReturnAFrame() async throws {
        let server = try UnixTestServer { _ in
            usleep(200_000)
            return Data()
        }
        defer { server.stop() }
        await #expect(throws: LocalControlError.timeout) {
            _ = try await LocalControlClient(socketURL: server.socketURL, timeout: 0.05).accounts()
        }
    }

    @Test func timeoutIsAbsoluteAcrossTrickledResponseBytes() async throws {
        let server = try UnixTestServer(replyByteDelay: 20_000) { _ in
            Data(repeating: 0x20, count: 20)
        }
        defer { server.stop() }
        let clock = ContinuousClock()
        let start = clock.now

        await #expect(throws: LocalControlError.timeout) {
            _ = try await LocalControlClient(socketURL: server.socketURL, timeout: 0.05).accounts()
        }

        #expect(start.duration(to: clock.now) < .milliseconds(200))
    }

    @Test func timeoutBoundsLargeWritesWhenThePeerDoesNotRead() async throws {
        let server = try UnixTestServer(readsRequest: false) { _ in Data() }
        defer { server.stop() }
        let client = LocalControlClient(socketURL: server.socketURL, timeout: 0.05)
        let clock = ContinuousClock()
        let start = clock.now

        await #expect(throws: LocalControlError.timeout) {
            _ = try await client.completeAuthentication(
                provider: "cursor",
                account: "account-1",
                flowId: "flow-1",
                input: String(repeating: "x", count: 4 * 1024 * 1024)
            )
        }

        #expect(start.duration(to: clock.now) < .milliseconds(200))
    }
}

private final class UnixTestServer: @unchecked Sendable {
    let socketURL: URL
    private let descriptor: Int32
    private let task: Task<Void, Never>

    init(
        mode: mode_t = 0o600,
        readsRequest: Bool = true,
        replyByteDelay: useconds_t = 0,
        reply: @escaping @Sendable (Data) throws -> Data
    ) throws {
        let directory = try temporaryDirectory()
        let serverURL = directory.appendingPathComponent("control.sock")
        let serverDescriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard serverDescriptor >= 0 else { throw POSIXError(.EIO) }

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(serverURL.path.utf8CString)
        guard pathBytes.count <= MemoryLayout.size(ofValue: address.sun_path) else {
            Darwin.close(serverDescriptor)
            throw POSIXError(.ENAMETOOLONG)
        }
        withUnsafeMutableBytes(of: &address.sun_path) { destination in
            destination.copyBytes(from: pathBytes.map { UInt8(bitPattern: $0) })
        }
        let bound = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(serverDescriptor, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bound == 0, chmod(serverURL.path, mode) == 0, listen(serverDescriptor, 1) == 0 else {
            Darwin.close(serverDescriptor)
            throw POSIXError(.EIO)
        }
        socketURL = serverURL
        descriptor = serverDescriptor
        task = Task.detached {
            defer { try? FileManager.default.removeItem(at: serverURL.deletingLastPathComponent()) }
            let peer = Darwin.accept(serverDescriptor, nil, nil)
            guard peer >= 0 else { return }
            defer { Darwin.close(peer) }
            if !readsRequest {
                usleep(200_000)
                return
            }
            var request = Data()
            var byte: UInt8 = 0
            while Darwin.read(peer, &byte, 1) == 1 {
                request.append(byte)
                if byte == 0x0a { break }
            }
            guard let payload = try? reply(request), !payload.isEmpty else { return }
            var noSigPipe: Int32 = 1
            _ = setsockopt(
                peer,
                SOL_SOCKET,
                SO_NOSIGPIPE,
                &noSigPipe,
                socklen_t(MemoryLayout<Int32>.size)
            )
            payload.withUnsafeBytes { bytes in
                if replyByteDelay == 0 {
                    _ = Darwin.write(peer, bytes.baseAddress, payload.count)
                    return
                }
                for offset in 0..<payload.count {
                    usleep(replyByteDelay)
                    if Darwin.write(peer, bytes.baseAddress!.advanced(by: offset), 1) != 1 {
                        break
                    }
                }
            }
        }
    }

    func stop() {
        Darwin.close(descriptor)
        task.cancel()
    }
}

private func temporaryDirectory() throws -> URL {
    let directory = URL(fileURLWithPath: "/tmp", isDirectory: true)
        .appendingPathComponent("ullage-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
    return directory
}

private final class RequestRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private(set) var values: [[String: Any]] = []

    func append(_ value: [String: Any]) {
        lock.lock()
        values.append(value)
        lock.unlock()
    }
}

private func response(
    version: UInt16 = 10,
    requestID: String,
    result: String,
    payload: Any?
) throws -> Data {
    var resultObject: [String: Any] = ["result": result]
    if let payload { resultObject["payload"] = payload }
    var data = try JSONSerialization.data(withJSONObject: [
        "version": version,
        "request_id": requestID,
        "result": resultObject,
    ])
    data.append(0x0a)
    return data
}
