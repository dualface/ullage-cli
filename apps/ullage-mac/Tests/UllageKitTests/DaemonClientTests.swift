import Foundation
import Testing
@testable import UllageKit

@Suite(.serialized)
struct DaemonClientTests {
    @Test func sendsExpectedRequestsAndAuthorization() async throws {
        StubURLProtocol.handler = { request in
            #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer fixture-token")
            #expect(request.url?.query == nil)
            let path = request.url?.path
            switch path {
            case "/v1/status":
                #expect(request.httpMethod == "GET")
                return response(request, body: #"{"shutting_down":false,"accounts":[],"credential_backend":"macos_keychain"}"#)
            case "/v1/accounts":
                #expect(request.httpMethod == "GET")
                return response(request, body: "[]")
            case "/v1/usage":
                #expect(request.httpMethod == "GET")
                return response(request, body: "[]")
            case "/v1/accounts/account fixture/probe":
                #expect(request.httpMethod == "POST")
                return response(request, body: #"{"account_id":"account fixture","usage":{"outcome":"future"}}"#)
            default:
                Issue.record("Unexpected path: \(path ?? "nil")")
                return response(request, status: 404, body: "{}")
            }
        }

        let client = makeClient()
        #expect(try await client.status().credentialBackend == .macOSKeychain)
        #expect(try await client.accounts().isEmpty)
        #expect(try await client.usage().isEmpty)
        #expect(try await client.probe(accountId: "account fixture").accountId == "account fixture")
    }

    @Test func acceptsBareAndEnvelopeResponseShapes() async throws {
        let client = makeClient()
        StubURLProtocol.handler = { response($0, body: "[]") }
        #expect(try await client.usage().isEmpty)

        StubURLProtocol.handler = {
            response($0, body: #"{"version":8,"result":"snapshots","payload":[]}"#)
        }
        #expect(try await client.usage().isEmpty)
    }

    @Test func rejectsMismatchedProtocolVersions() async throws {
        StubURLProtocol.handler = {
            response($0, body: #"{"version":9,"result":"snapshots","payload":{"future":true}}"#)
        }
        do {
            _ = try await makeClient().usage()
            Issue.record("Expected success-envelope mismatch to fail")
        } catch let error as DaemonError {
            guard case .protocolMismatch(client: 8, server: 9) = error else {
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
            guard case .protocolMismatch(client: 8, server: 10) = error else {
                Issue.record("Expected protocol mismatch, got \(error)")
                return
            }
        }
    }

    @Test func rejectsACompatiblePayloadWithTheWrongEnvelopeTag() async throws {
        StubURLProtocol.handler = {
            response($0, body: #"{"version":8,"result":"accounts","payload":[]}"#)
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
                    body: #"{"error":{"kind":"fixture_kind","diagnostic":"must be ignored"}}"#
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
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubURLProtocol.self]
        return DaemonClient(
            baseURL: URL(string: "http://127.0.0.1:48937")!,
            token: "fixture-token",
            configuration: configuration
        )
    }
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
