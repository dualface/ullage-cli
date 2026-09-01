import Foundation
import Testing
@testable import UllageKit

@Suite(.serialized)
struct DaemonClientTests {
    @Test func sendsExpectedRequestsAndAuthorization() async throws {
        StubURLProtocol.handler = { request in
            #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer fixture-token")
            let path = request.url?.path
            switch path {
            case "/v1/status":
                #expect(request.httpMethod == "GET")
                return response(request, body: #"{"version":9,"result":"daemon_status","payload":{"shutting_down":false,"accounts":[],"credential_backend":"macos_keychain"}}"#)
            case "/v1/accounts":
                #expect(request.httpMethod == "GET")
                return response(request, body: #"{"version":9,"result":"accounts","payload":[]}"#)
            case "/v1/usage":
                #expect(request.httpMethod == "GET")
                return response(request, body: #"{"version":9,"result":"snapshots","payload":[]}"#)
            case "/v1/accounts/account fixture/probe":
                #expect(request.httpMethod == "POST")
                #expect(request.url?.query == "wait=true")
                return response(request, body: #"{"version":9,"result":"probe","payload":{"account_id":"account fixture","usage":{"outcome":"future"}}}"#)
            default:
                Issue.record("Unexpected path: \(path ?? "nil")")
                return response(request, status: 404, body: "{}")
            }
        }

        let client = makeClient()
        let status = try await client.status()
        #expect(status.version == 9)
        #expect(status.credentialBackend == .macOSKeychain)
        #expect(try await client.accounts().isEmpty)
        #expect(try await client.usage().isEmpty)
        let result = try await client.probe(accountId: "account fixture", wait: true)
        guard case .completed(let payload) = result else {
            Issue.record("Expected a completed probe")
            return
        }
        #expect(payload.accountId == "account fixture")
    }

    @Test func pairingSendsJSONWithoutBearerAndDecodesCredential() async throws {
        StubURLProtocol.handler = { request in
            #expect(request.url?.path == "/v1/pair")
            #expect(request.httpMethod == "POST")
            #expect(request.value(forHTTPHeaderField: "Authorization") == nil)
            #expect(request.value(forHTTPHeaderField: "Content-Type") == "application/json")
            let body = try requestBody(request)
            let json = try #require(JSONSerialization.jsonObject(with: body) as? [String: String])
            #expect(json == ["pair_code": "abc-def", "device_name": "pro2026"])
            return response(
                request,
                body: #"{"device_id":"ABCD2345EFGH","device_name":"pro2026","device_token":"device-secret"}"#
            )
        }

        let credential = try await DaemonClient.pair(
            baseURL: URL(string: "http://127.0.0.1:48937")!,
            pairCode: "abc-def",
            deviceName: "pro2026",
            configuration: stubConfiguration()
        )
        #expect(credential == PairedDeviceCredential(
            deviceId: "ABCD2345EFGH",
            deviceName: "pro2026",
            deviceToken: "device-secret"
        ))
    }

    @Test func pairingPreservesDaemonErrorCodesAndRejectsMalformedResponses() async throws {
        StubURLProtocol.handler = {
            response($0, status: 401, body: #"{"error":"pair_code_invalid"}"#)
        }
        do {
            _ = try await DaemonClient.pair(
                baseURL: URL(string: "http://127.0.0.1:48937")!,
                pairCode: "ABC-DEF",
                deviceName: "fixture",
                configuration: stubConfiguration()
            )
            Issue.record("Expected an invalid pair code")
        } catch let error as DaemonError {
            guard case .unauthorized(kind: "pair_code_invalid") = error else {
                Issue.record("Expected pair_code_invalid, got \(error)")
                return
            }
        }

        StubURLProtocol.handler = { response($0, body: #"{"device_id":"missing-fields"}"#) }
        do {
            _ = try await DaemonClient.pair(
                baseURL: URL(string: "http://127.0.0.1:48937")!,
                pairCode: "ABC-DEF",
                deviceName: "fixture",
                configuration: stubConfiguration()
            )
            Issue.record("Expected malformed pairing response")
        } catch let error as DaemonError {
            guard case .decoding = error else {
                Issue.record("Expected decoding, got \(error)")
                return
            }
        }
    }

    @Test func rejectsBarePayloadsAndAcceptsOnlyEnvelopes() async throws {
        StubURLProtocol.handler = { response($0, body: "[]") }
        do {
            _ = try await makeClient().usage()
            Issue.record("Expected a bare payload to fail")
        } catch let error as DaemonError {
            guard case .decoding = error else {
                Issue.record("Expected decoding, got \(error)")
                return
            }
        }

        StubURLProtocol.handler = {
            response($0, body: #"{"version":9,"result":"snapshots","payload":[]}"#)
        }
        #expect(try await makeClient().usage().isEmpty)
    }

    @Test func mapsSynchronousAndAsynchronousProbeResponses() async throws {
        StubURLProtocol.handler = { request in
            if request.url?.query == "wait=false" {
                return response(request, status: 202, body: #"{"version":9,"result":"ack"}"#)
            }
            return response(
                request,
                body: #"{"version":9,"result":"probe","payload":{"account_id":"fixture","usage":{"outcome":"future"}}}"#
            )
        }

        #expect(try await makeClient().probe(accountId: "fixture", wait: false) == .accepted)
        let completed = try await makeClient().probe(accountId: "fixture", wait: true)
        guard case .completed(let payload) = completed else {
            Issue.record("Expected a completed probe")
            return
        }
        #expect(payload.accountId == "fixture")
    }

    @Test func requestsUsageForOneAccount() async throws {
        StubURLProtocol.handler = { request in
            #expect(request.url?.path == "/v1/usage")
            #expect(request.url?.query == "account=fixture")
            return response(request, body: #"{"version":9,"result":"snapshots","payload":[]}"#)
        }
        #expect(try await makeClient().usage(accountId: "fixture").isEmpty)
    }

    @Test func rejectsMismatchedProtocolVersions() async throws {
        StubURLProtocol.handler = {
            response($0, body: #"{"version":10,"result":"snapshots","payload":{"future":true}}"#)
        }
        do {
            _ = try await makeClient().usage()
            Issue.record("Expected success-envelope mismatch to fail")
        } catch let error as DaemonError {
            guard case .protocolMismatch(client: 9, server: 10) = error else {
                Issue.record("Expected protocol mismatch, got \(error)")
                return
            }
        }


        StubURLProtocol.handler = {
            response($0, body: #"{"result":"snapshots","payload":[]}"#)
        }
        do {
            _ = try await makeClient().usage()
            Issue.record("Expected an unversioned envelope to fail")
        } catch let error as DaemonError {
            guard case .decoding = error else {
                Issue.record("Expected decoding failure, got \(error)")
                return
            }
        }

        StubURLProtocol.handler = {
            response(
                $0,
                status: 400,
                body: #"{"version":10,"error":"protocol_mismatch","supported_version":10}"#
            )
        }
        do {
            _ = try await makeClient().status()
            Issue.record("Expected error-document mismatch to fail")
        } catch let error as DaemonError {
            guard case .protocolMismatch(client: 9, server: 10) = error else {
                Issue.record("Expected protocol mismatch, got \(error)")
                return
            }
        }
    }

    @Test func rejectsACompatiblePayloadWithTheWrongEnvelopeTag() async throws {
        StubURLProtocol.handler = {
            response($0, body: #"{"version":9,"result":"accounts","payload":[]}"#)
        }
        do {
            _ = try await makeClient().usage()
            Issue.record("Expected wrong result tag to fail")
        } catch let error as DaemonError {
            guard case .decoding = error else {
                Issue.record("Expected decoding, got \(error)")
                return
            }
        }
    }

    @Test func mapsRedirectResponsesWithoutFollowingThem() async throws {
        let recorder = RequestRecorder()
        StubURLProtocol.handler = { request in
            recorder.record(request)
            if request.url?.path == "/v1/status" {
                return response(request, status: 302, headers: ["Location": "/login"], body: "")
            }
            return response(request, body: "{}")
        }
        do {
            _ = try await makeClient().status()
            Issue.record("Expected redirect to fail")
        } catch let error as DaemonError {
            guard case .unexpectedStatus(302, _) = error else {
                Issue.record("Expected unexpectedStatus(302), got \(error)")
                return
            }
        }
        #expect(recorder.paths == ["/v1/status"])
    }

    @Test func mapsHTTPStatusCodesAndPreservesSanitizedKind() async throws {
        let expectations = [401, 403, 404, 409, 429, 504, 500, 418]
        for status in expectations {
            StubURLProtocol.handler = {
                response(
                    $0,
                    status: status,
                    headers: status == 429 ? ["Retry-After": "17.5"] : [:],
                    body: status == 401
                        ? #"{"error":"fixture_kind","diagnostic":"must be ignored"}"#
                        : #"{"version":9,"kind":"fixture_kind","detail":{"diagnostic":"nested value"},"diagnostic":"must be ignored"}"#
                )
            }
            do {
                _ = try await makeClient().status()
                Issue.record("Expected status \(status) to fail")
            } catch let error as DaemonError {
                #expect(error.errorKind == "fixture_kind")
                switch (status, error) {
                case (401, .unauthorized), (403, .forbiddenHost), (404, .notFound),
                     (409, .authenticationInvalid), (504, .timeout), (500, .storage),
                     (418, .unexpectedStatus(418, _)):
                    break
                case (429, .rateLimited(let retryAfter, _)):
                    #expect(retryAfter == 17.5)
                default:
                    Issue.record("Wrong mapping for status \(status): \(error)")
                }
            }
        }
    }

    @Test func mapsRealStringErrorsAndRetryAfter() async throws {
        StubURLProtocol.handler = {
            response(
                $0,
                status: 429,
                headers: ["Retry-After": "59"],
                body: #"{"error":"rate_limited"}"#
            )
        }
        do {
            _ = try await makeClient().probe(accountId: "fixture", wait: true)
            Issue.record("Expected rate limiting")
        } catch let error as DaemonError {
            guard case .rateLimited(retryAfter: 59, kind: "rate_limited") = error else {
                Issue.record("Expected real rate-limit mapping, got \(error)")
                return
            }
        }

        StubURLProtocol.handler = {
            response(
                $0,
                status: 401,
                headers: ["WWW-Authenticate": "Bearer"],
                body: #"{"error":"unauthorized"}"#
            )
        }
        do {
            _ = try await makeClient().status()
            Issue.record("Expected unauthorized")
        } catch let error as DaemonError {
            guard case .unauthorized(kind: "unauthorized") = error else {
                Issue.record("Expected unauthorized mapping, got \(error)")
                return
            }
        }
    }

    @Test func separatesTransportAndDecodingFailures() async throws {
        StubURLProtocol.handler = { _ in throw URLError(.timedOut) }
        do {
            _ = try await makeClient().status()
        } catch let error as DaemonError {
            guard case .unreachable = error else {
                Issue.record("Expected unreachable, got \(error)")
                return
            }
        }

        StubURLProtocol.handler = { response($0, body: "not-json") }
        do {
            _ = try await makeClient().status()
            Issue.record("Expected decoding failure")
        } catch let error as DaemonError {
            guard case .decoding = error else {
                Issue.record("Expected decoding, got \(error)")
                return
            }
        }
    }

    @Test func preservesCancellationSemantics() async throws {
        StubURLProtocol.handler = { _ in throw URLError(.cancelled) }
        do {
            _ = try await makeClient().status()
            Issue.record("Expected cancellation")
        } catch is CancellationError {
            // Cancellation is intentionally not translated to a daemon outage.
        }
    }

    private func makeClient() -> DaemonClient {
        return DaemonClient(
            baseURL: URL(string: "http://127.0.0.1:48937")!,
            token: "fixture-token",
            configuration: stubConfiguration()
        )
    }
}

private func stubConfiguration() -> URLSessionConfiguration {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [StubURLProtocol.self]
    return configuration
}

private func requestBody(_ request: URLRequest) throws -> Data {
    if let body = request.httpBody { return body }
    let stream = try #require(request.httpBodyStream)
    stream.open()
    defer { stream.close() }
    var body = Data()
    var buffer = [UInt8](repeating: 0, count: 1024)
    while stream.hasBytesAvailable {
        let count = stream.read(&buffer, maxLength: buffer.count)
        guard count >= 0 else { throw stream.streamError ?? URLError(.cannotDecodeRawData) }
        if count == 0 { break }
        body.append(buffer, count: count)
    }
    return body
}

private final class StubURLProtocol: URLProtocol, @unchecked Sendable {
    nonisolated(unsafe) static var handler: @Sendable (URLRequest) throws -> (HTTPURLResponse, Data) = { _ in
        throw URLError(.badServerResponse)
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        do {
            let (response, data) = try Self.handler(request)
            if (300..<400).contains(response.statusCode),
               let location = response.value(forHTTPHeaderField: "Location"),
               let url = URL(string: location, relativeTo: request.url)?.absoluteURL {
                client?.urlProtocol(
                    self,
                    wasRedirectedTo: URLRequest(url: url),
                    redirectResponse: response
                )
                client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
                client?.urlProtocol(self, didLoad: data)
                client?.urlProtocolDidFinishLoading(self)
                return
            }
            client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: data)
            client?.urlProtocolDidFinishLoading(self)
        } catch {
            client?.urlProtocol(self, didFailWithError: error)
        }
    }

    override func stopLoading() {}
}

private final class RequestRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var recordedPaths: [String] = []

    var paths: [String] {
        lock.withLock { recordedPaths }
    }

    func record(_ request: URLRequest) {
        lock.withLock { recordedPaths.append(request.url?.path ?? "") }
    }
}

private func response(
    _ request: URLRequest,
    status: Int = 200,
    headers: [String: String] = [:],
    body: String
) -> (HTTPURLResponse, Data) {
    let response = HTTPURLResponse(
        url: request.url!,
        statusCode: status,
        httpVersion: "HTTP/1.1",
        headerFields: headers
    )!
    return (response, Data(body.utf8))
}
