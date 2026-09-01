import Foundation

public enum DaemonError: Error, @unchecked Sendable, CustomStringConvertible {
    case unreachable(underlying: any Error)
    case unauthorized(kind: String?)
    case forbiddenHost(kind: String?)
    case notFound(kind: String?)
    case authenticationInvalid(kind: String?)
    case rateLimited(retryAfter: TimeInterval?, kind: String?)
    case timeout(kind: String?)
    case storage(kind: String?)
    case unexpectedStatus(Int, kind: String?)
    case protocolMismatch(client: UInt16, server: UInt16)
    case decoding(underlying: any Error)

    public var errorKind: String? {
        switch self {
        case .unreachable, .protocolMismatch, .decoding: nil
        case .unauthorized(let kind), .forbiddenHost(let kind), .notFound(let kind),
             .authenticationInvalid(let kind), .timeout(let kind), .storage(let kind),
             .unexpectedStatus(_, let kind), .rateLimited(_, let kind): kind
        }
    }

    public var description: String {
        switch self {
        case .unreachable: "daemon unreachable"
        case .unauthorized(let kind): kind ?? "unauthorized"
        case .forbiddenHost(let kind): kind ?? "forbidden"
        case .notFound(let kind): kind ?? "not_found"
        case .authenticationInvalid(let kind): kind ?? "authentication_invalid"
        case .rateLimited(let retryAfter, let kind):
            (kind ?? "rate_limited") + (retryAfter.map { " (retry after \($0)s)" } ?? "")
        case .timeout(let kind): kind ?? "timeout"
        case .storage(let kind): kind ?? "storage"
        case .unexpectedStatus(let status, let kind): kind ?? "unexpected HTTP status \(status)"
        case .protocolMismatch(let client, let server):
            "protocol mismatch (client \(client), server \(server))"
        case .decoding: "daemon response decoding failed"
        }
    }
}

public struct PairedDeviceCredential: Decodable, Equatable, Sendable {
    public let deviceId: String
    public let deviceName: String
    public let deviceToken: String

    public init(deviceId: String, deviceName: String, deviceToken: String) {
        self.deviceId = deviceId
        self.deviceName = deviceName
        self.deviceToken = deviceToken
    }

    enum CodingKeys: String, CodingKey {
        case deviceId = "device_id"
        case deviceName = "device_name"
        case deviceToken = "device_token"
    }
}

public final class DaemonClient: @unchecked Sendable {
    public static let protocolVersion: UInt16 = 9
    private let baseURL: URL
    private let token: String
    private let session: URLSession
    private let decoder: JSONDecoder

    public init(baseURL: URL, token: String, configuration: URLSessionConfiguration? = nil) {
        self.baseURL = baseURL
        self.token = token
        let configuration = configuration ?? URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 10
        configuration.waitsForConnectivity = false
        configuration.httpShouldSetCookies = false
        let delegate = RedirectRejectingDelegate()
        self.session = URLSession(configuration: configuration, delegate: delegate, delegateQueue: nil)
        self.decoder = UllageJSON.makeDecoder()
    }

    public func status() async throws -> DaemonStatusPayload {
        let envelope: ResponseEnvelope<DaemonStatusPayload> = try await send(
            path: ["v1", "status"], method: "GET", expectedResult: "daemon_status"
        )
        return DaemonStatusPayload(
            version: envelope.version,
            shuttingDown: envelope.payload.shuttingDown,
            accounts: envelope.payload.accounts,
            credentialBackend: envelope.payload.credentialBackend
        )
    }

    public static func pair(
        baseURL: URL,
        pairCode: String,
        deviceName: String,
        configuration: URLSessionConfiguration? = nil
    ) async throws -> PairedDeviceCredential {
        let client = DaemonClient(baseURL: baseURL, token: "", configuration: configuration)
        let requestBody: Data
        do {
            requestBody = try JSONEncoder().encode(PairRequest(
                pairCode: pairCode,
                deviceName: deviceName
            ))
        } catch {
            throw DaemonError.decoding(underlying: error)
        }
        let (data, _) = try await client.perform(
            path: ["v1", "pair"],
            method: "POST",
            body: requestBody,
            authenticated: false
        )
        do {
            return try client.decoder.decode(PairedDeviceCredential.self, from: data)
        } catch {
            throw DaemonError.decoding(underlying: error)
        }
    }

    public func accounts() async throws -> [Account] {
        let envelope: ResponseEnvelope<[Account]> = try await send(
            path: ["v1", "accounts"], method: "GET", expectedResult: "accounts"
        )
        return envelope.payload
    }

    public func usage() async throws -> [SnapshotPayload] {
        let envelope: ResponseEnvelope<[SnapshotPayload]> = try await send(
            path: ["v1", "usage"], method: "GET", expectedResult: "snapshots"
        )
        return envelope.payload
    }

    public func usage(accountId: String) async throws -> [SnapshotPayload] {
        let envelope: ResponseEnvelope<[SnapshotPayload]> = try await send(
            path: ["v1", "usage"],
            method: "GET",
            expectedResult: "snapshots",
            queryItems: [URLQueryItem(name: "account", value: accountId)]
        )
        return envelope.payload
    }

    public func probe(accountId: String, wait: Bool) async throws -> ProbeResult {
        let (data, response) = try await perform(
            path: ["v1", "accounts", accountId, "probe"],
            method: "POST",
            queryItems: [URLQueryItem(name: "wait", value: wait ? "true" : "false")]
        )
        let expectedStatus = wait ? 200 : 202
        guard response.statusCode == expectedStatus else {
            throw DaemonError.unexpectedStatus(response.statusCode, kind: nil)
        }
        if wait {
            let envelope: ResponseEnvelope<ProbePayload> = try decodeEnvelope(
                data, expectedResult: "probe"
            )
            return .completed(envelope.payload)
        }
        _ = try decodeHeader(data, expectedResult: "ack")
        return .accepted
    }

    private func send<Payload: Decodable>(
        path: [String],
        method: String,
        expectedResult: String,
        queryItems: [URLQueryItem] = []
    ) async throws -> ResponseEnvelope<Payload> {
        let (data, _) = try await perform(path: path, method: method, queryItems: queryItems)
        return try decodeEnvelope(data, expectedResult: expectedResult)
    }

    private func perform(
        path: [String],
        method: String,
        queryItems: [URLQueryItem] = [],
        body: Data? = nil,
        authenticated: Bool = true
    ) async throws -> (Data, HTTPURLResponse) {
        var url = baseURL
        for component in path {
            url.appendPathComponent(component)
        }
        if !queryItems.isEmpty {
            guard var components = URLComponents(url: url, resolvingAgainstBaseURL: false) else {
                throw DaemonError.unreachable(underlying: URLError(.badURL))
            }
            components.queryItems = queryItems
            guard let queryURL = components.url else {
                throw DaemonError.unreachable(underlying: URLError(.badURL))
            }
            url = queryURL
        }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.httpBody = body
        if body != nil {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        if authenticated {
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }

        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: request)
        } catch is CancellationError {
            throw CancellationError()
        } catch let error as URLError where error.code == .cancelled {
            throw CancellationError()
        } catch {
            throw DaemonError.unreachable(underlying: error)
        }
        guard let httpResponse = response as? HTTPURLResponse else {
            throw DaemonError.unreachable(underlying: URLError(.badServerResponse))
        }
        guard (200..<300).contains(httpResponse.statusCode) else {
            if let mismatch = try? decoder.decode(ProtocolMismatchDocument.self, from: data),
               mismatch.error == "protocol_mismatch" {
                throw DaemonError.protocolMismatch(client: Self.protocolVersion, server: mismatch.version)
            }
            throw statusError(response: httpResponse, data: data)
        }

        return (data, httpResponse)
    }

    private func decodeEnvelope<Payload: Decodable>(
        _ data: Data,
        expectedResult: String
    ) throws -> ResponseEnvelope<Payload> {
        do {
            _ = try decodeHeader(data, expectedResult: expectedResult)
            return try decoder.decode(ResponseEnvelope<Payload>.self, from: data)
        } catch let error as DaemonError {
            throw error
        } catch {
            throw DaemonError.decoding(underlying: error)
        }
    }

    private func decodeHeader(_ data: Data, expectedResult: String) throws -> ResponseEnvelopeHeader {
        do {
            let header = try decoder.decode(ResponseEnvelopeHeader.self, from: data)
            if header.version != Self.protocolVersion {
                throw DaemonError.protocolMismatch(client: Self.protocolVersion, server: header.version)
            }
            guard header.result == expectedResult else {
                throw EnvelopeResultMismatch(expected: expectedResult, actual: header.result)
            }
            return header
        } catch let error as DaemonError {
            throw error
        } catch {
            throw DaemonError.decoding(underlying: error)
        }
    }

    private func statusError(response: HTTPURLResponse, data: Data) -> DaemonError {
        let kind = (try? JSONDecoder().decode(StringErrorDocument.self, from: data))?.error
            ?? (try? JSONDecoder().decode(ControlErrorDocument.self, from: data))?.kind
        return switch response.statusCode {
        case 401: .unauthorized(kind: kind)
        case 403: .forbiddenHost(kind: kind)
        case 404: .notFound(kind: kind)
        case 409: .authenticationInvalid(kind: kind)
        case 429:
            .rateLimited(
                retryAfter: response.value(forHTTPHeaderField: "Retry-After").flatMap(TimeInterval.init),
                kind: kind
            )
        case 408, 504: .timeout(kind: kind)
        case 500: .storage(kind: kind)
        default: .unexpectedStatus(response.statusCode, kind: kind)
        }
    }
}

final class RedirectRejectingDelegate: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        completionHandler(nil)
    }
}

private struct ResponseEnvelope<Payload: Decodable>: Decodable {
    let version: UInt16
    let result: String
    let payload: Payload
}

private struct ResponseEnvelopeHeader: Decodable {
    let version: UInt16
    let result: String
}

private struct ProtocolMismatchDocument: Decodable {
    let version: UInt16
    let error: String
}

private struct EnvelopeResultMismatch: Error {
    let expected: String
    let actual: String
}

private struct StringErrorDocument: Decodable { let error: String }

private struct ControlErrorDocument: Decodable { let kind: String }

private struct PairRequest: Encodable {
    let pairCode: String
    let deviceName: String

    enum CodingKeys: String, CodingKey {
        case pairCode = "pair_code"
        case deviceName = "device_name"
    }
}
