import Foundation

public enum DaemonError: Error, @unchecked Sendable {
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
}

public final class DaemonClient: @unchecked Sendable {
    public static let protocolVersion: UInt16 = 8
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
        try await send(path: ["v1", "status"], method: "GET", expectedResult: "daemon_status")
    }

    public func accounts() async throws -> [Account] {
        try await send(path: ["v1", "accounts"], method: "GET", expectedResult: "accounts")
    }

    public func usage() async throws -> [SnapshotPayload] {
        try await send(path: ["v1", "usage"], method: "GET", expectedResult: "snapshots")
    }

    public func probe(accountId: String) async throws -> ProbePayload {
        try await send(path: ["v1", "accounts", accountId, "probe"], method: "POST", expectedResult: "probe")
    }

    private func send<Payload: Decodable>(
        path: [String],
        method: String,
        expectedResult: String
    ) async throws -> Payload {
        var url = baseURL
        for component in path {
            url.appendPathComponent(component)
        }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")

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

        do {
            if let envelope = try? decoder.decode(ResponseEnvelope<Payload>.self, from: data) {
                if let version = envelope.version, version != Self.protocolVersion {
                    throw DaemonError.protocolMismatch(client: Self.protocolVersion, server: version)
                }
                guard envelope.result == expectedResult else {
                    throw EnvelopeResultMismatch(expected: expectedResult, actual: envelope.result)
                }
                return envelope.payload
            }
            return try decoder.decode(Payload.self, from: data)
        } catch let error as DaemonError {
            throw error
        } catch {
            throw DaemonError.decoding(underlying: error)
        }
    }

    private func statusError(response: HTTPURLResponse, data: Data) -> DaemonError {
        let kind = (try? JSONDecoder().decode(ErrorEnvelope.self, from: data))?.error.kind
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
        case 504: .timeout(kind: kind)
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
    let version: UInt16?
    let result: String
    let payload: Payload
}

private struct ProtocolMismatchDocument: Decodable {
    let version: UInt16
    let error: String
}

private struct EnvelopeResultMismatch: Error {
    let expected: String
    let actual: String
}

private struct ErrorEnvelope: Decodable {
    struct Payload: Decodable {
        let kind: String?
    }

    let error: Payload
}
