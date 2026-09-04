import Darwin
import Foundation
@testable import UllageKit
@testable import UllageMac
import XCTest

final class OAuthCallbackListenerTests: XCTestCase {
    func testRegisteredRedirectURIsMatchTheProviderDefaults() {
        XCTAssertEqual(
            registeredLoopbackRedirectURI(forProvider: "chatgpt"),
            "http://localhost:1455/auth/callback"
        )
        XCTAssertNil(registeredLoopbackRedirectURI(forProvider: "grok"))
        XCTAssertNil(registeredLoopbackRedirectURI(forProvider: "claude"))
        XCTAssertNil(registeredLoopbackRedirectURI(forProvider: "cursor"))
    }

    func testOnlyLoopbackHTTPRedirectURIsAreAccepted() {
        for redirectURI in [
            "https://localhost:1455/auth/callback",
            "http://example.test:1455/auth/callback",
            "http://192.168.1.4:1455/auth/callback",
            "http://localhost/auth/callback",
            "http://localhost:1455",
            "http://user@localhost:1455/auth/callback",
            "http://localhost:1455/auth/callback?code=x",
            "http://localhost:1455/auth/callback#fragment",
        ] {
            XCTAssertThrowsError(try OAuthCallbackListener(redirectURI: redirectURI), redirectURI) {
                XCTAssertEqual(
                    $0 as? OAuthCallbackError,
                    .unsupportedRedirectURI,
                    redirectURI
                )
            }
        }
    }

    func testAnOccupiedPortFailsToBindSoTheWizardCanFallBack() throws {
        let (listener, port) = try makeListener()
        defer { listener.close() }
        XCTAssertThrowsError(
            try OAuthCallbackListener(redirectURI: "http://localhost:\(port)/auth/callback")
        ) { error in
            guard case .bindFailed = error as? OAuthCallbackError else {
                return XCTFail("expected a bind failure, got \(error)")
            }
        }
    }

    func testCallbackOverIPv4Completes() async throws {
        try await assertCallbackCompletes(over: "127.0.0.1")
    }

    // `localhost` resolves to `::1` as well and browsers often prefer it, so the
    // same endpoint has to answer there.
    func testCallbackOverIPv6Completes() async throws {
        try await assertCallbackCompletes(over: "::1")
    }

    func testGrokLiteralAddressBindsIPv4() async throws {
        let (listener, port) = try makeListener(host: "127.0.0.1")
        defer { listener.close() }
        let deadline = Date().addingTimeInterval(10)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()
        _ = try request(host: "127.0.0.1", port: port, raw: get("/auth/callback?code=c&state=s"))
        let outcome = try await waiting.value
        XCTAssertEqual(
            outcome,
            .authorized(callbackURL: "http://127.0.0.1:\(port)/auth/callback?code=c&state=s")
        )
    }

    func testOtherMethodsAndPathsAnswer404WithoutEndingTheFlow() async throws {
        let (listener, port) = try makeListener()
        defer { listener.close() }
        let deadline = Date().addingTimeInterval(10)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()

        let posted = try request(
            host: "127.0.0.1",
            port: port,
            raw: """
            POST /auth/callback?code=c&state=s HTTP/1.1\r
            Host: localhost\r
            Content-Length: 0\r
            \r

            """
        )
        XCTAssertTrue(posted.hasPrefix("HTTP/1.1 404"), posted)
        let elsewhere = try request(host: "127.0.0.1", port: port, raw: get("/elsewhere"))
        XCTAssertTrue(elsewhere.hasPrefix("HTTP/1.1 404"), elsewhere)

        _ = try request(host: "127.0.0.1", port: port, raw: get("/auth/callback?code=c&state=s"))
        let outcome = try await waiting.value
        XCTAssertEqual(
            outcome,
            .authorized(callbackURL: "http://localhost:\(port)/auth/callback?code=c&state=s")
        )
    }

    func testAMismatchedStateIsRejectedAndDoesNotComplete() async throws {
        let (listener, port) = try makeListener()
        defer { listener.close() }
        let deadline = Date().addingTimeInterval(10)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()

        for target in [
            "/auth/callback?code=c&state=other",
            "/auth/callback?code=c",
            "/auth/callback?state=s",
        ] {
            let rejected = try request(host: "127.0.0.1", port: port, raw: get(target))
            XCTAssertTrue(rejected.hasPrefix("HTTP/1.1 400"), "\(target): \(rejected)")
        }

        _ = try request(host: "127.0.0.1", port: port, raw: get("/auth/callback?code=c&state=s"))
        let outcome = try await waiting.value
        XCTAssertEqual(
            outcome,
            .authorized(callbackURL: "http://localhost:\(port)/auth/callback?code=c&state=s")
        )
    }

    func testAnOversizedRequestLineIsRejectedWithoutEndingTheFlow() async throws {
        let (listener, port) = try makeListener()
        defer { listener.close() }
        let deadline = Date().addingTimeInterval(10)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()

        let padding = String(repeating: "a", count: OAuthCallbackListener.maximumRequestLineBytes)
        let oversized = try request(
            host: "127.0.0.1",
            port: port,
            raw: get("/auth/callback?code=c&state=s&padding=\(padding)")
        )
        XCTAssertTrue(oversized.hasPrefix("HTTP/1.1 400"), oversized)

        _ = try request(host: "127.0.0.1", port: port, raw: get("/auth/callback?code=c&state=s"))
        let outcome = try await waiting.value
        XCTAssertEqual(
            outcome,
            .authorized(callbackURL: "http://localhost:\(port)/auth/callback?code=c&state=s")
        )
    }

    func testAnErrorCallbackIsReportedWithoutAnAuthorizationCode() async throws {
        let (listener, port) = try makeListener()
        defer { listener.close() }
        let deadline = Date().addingTimeInterval(10)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()
        _ = try request(
            host: "127.0.0.1",
            port: port,
            raw: get("/auth/callback?error=access_denied&state=s")
        )
        let outcome = try await waiting.value
        XCTAssertEqual(outcome, .declined(reason: "access_denied"))
    }

    func testTheEndpointIsOneShotAndReleasesThePortWhenItCompletes() async throws {
        let (listener, port) = try makeListener()
        let deadline = Date().addingTimeInterval(10)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()
        _ = try request(host: "127.0.0.1", port: port, raw: get("/auth/callback?code=c&state=s"))
        _ = try await waiting.value

        try await rebind(port: port)
    }

    func testTheEndpointClosesWhenTheFlowDeadlinePasses() async throws {
        let (listener, port) = try makeListener()
        let deadline = Date().addingTimeInterval(0.2)
        do {
            _ = try await listener.waitForCallback(state: "s", deadline: deadline)
            XCTFail("the wait should have timed out")
        } catch {
            XCTAssertEqual(error as? OAuthCallbackError, .timedOut)
        }
        try await rebind(port: port)
    }

    func testClosingTheListenerEndsTheWaitAndReleasesThePort() async throws {
        let (listener, port) = try makeListener()
        let deadline = Date().addingTimeInterval(30)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()
        listener.close()
        do {
            _ = try await waiting.value
            XCTFail("the wait should have been cancelled")
        } catch {
            XCTAssertEqual(error as? OAuthCallbackError, .cancelled)
        }
        try await rebind(port: port)
    }

    func testCancellingTheWaitingTaskReleasesThePort() async throws {
        let (listener, port) = try makeListener()
        let deadline = Date().addingTimeInterval(30)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()
        waiting.cancel()
        _ = try? await waiting.value
        try await rebind(port: port)
    }

    /// Exercises replying into a connection the peer has already reset. The
    /// endpoint has to survive it and stay usable; without `SO_NOSIGPIPE` such a
    /// write raises `SIGPIPE`, which by default kills the whole process (the
    /// disposition stays `SIG_DFL` in an AppKit app). The race is timing
    /// dependent, so this guards the behavior rather than proving the option.
    func testAPeerThatResetsBeforeTheReplyDoesNotKillTheProcess() async throws {
        let (listener, port) = try makeListener()
        defer { listener.close() }
        let deadline = Date().addingTimeInterval(10)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()

        for _ in 0..<32 {
            try sendAndReset(host: "127.0.0.1", port: port, raw: get("/auth/callback?state=x"))
        }

        _ = try request(host: "127.0.0.1", port: port, raw: get("/auth/callback?code=c&state=s"))
        let outcome = try await waiting.value
        XCTAssertEqual(
            outcome,
            .authorized(callbackURL: "http://localhost:\(port)/auth/callback?code=c&state=s")
        )
    }

    func testRequestLineParsingKeepsTheQueryVerbatim() {
        let request = OAuthCallbackRequest(
            requestLine: "GET /auth/callback?code=a%2Fb&state=s HTTP/1.1"
        )
        XCTAssertEqual(request?.method, "GET")
        XCTAssertEqual(request?.path, "/auth/callback")
        XCTAssertEqual(request?.query, "code=a%2Fb&state=s")
        XCTAssertEqual(request?.value(of: "code"), "a/b")
        XCTAssertEqual(request?.value(of: "state"), "s")
        XCTAssertNil(request?.value(of: "error"))

        XCTAssertEqual(
            OAuthCallbackRequest(requestLine: "GET /auth/callback HTTP/1.1")?.query,
            ""
        )
        XCTAssertNil(OAuthCallbackRequest(requestLine: "GET /auth/callback"))
        XCTAssertNil(OAuthCallbackRequest(requestLine: "GET /auth/callback RTSP/1.0"))
        XCTAssertNil(OAuthCallbackRequest(requestLine: ""))
    }

    func testAnErrorValueIsReducedToAShortPrintableToken() {
        XCTAssertEqual(OAuthCallbackListener.readableReason("access_denied"), "access_denied")
        XCTAssertEqual(OAuthCallbackListener.readableReason("a\nb"), "ab")
        XCTAssertEqual(OAuthCallbackListener.readableReason("  "), "an unspecified error")
        XCTAssertEqual(
            OAuthCallbackListener.readableReason(String(repeating: "x", count: 200)).count,
            64
        )
    }

    // MARK: - Helpers

    private func assertCallbackCompletes(over host: String) async throws {
        let (listener, port) = try makeListener()
        defer { listener.close() }
        let deadline = Date().addingTimeInterval(10)
        let waiting = Task { try await listener.waitForCallback(state: "s", deadline: deadline) }
        try await settle()
        let response = try request(
            host: host,
            port: port,
            raw: get("/auth/callback?code=the-code&state=s")
        )
        XCTAssertTrue(response.hasPrefix("HTTP/1.1 200"), response)
        let outcome = try await waiting.value
        XCTAssertEqual(
            outcome,
            .authorized(callbackURL: "http://localhost:\(port)/auth/callback?code=the-code&state=s")
        )
    }

    private func makeListener(
        host: String = "localhost"
    ) throws -> (OAuthCallbackListener, Int) {
        var lastError: Error = OAuthCallbackError.bindFailed("no port was tried")
        for _ in 0..<32 {
            let port = Int.random(in: 49_152...65_000)
            do {
                let redirectURI = "http://\(host):\(port)/auth/callback"
                return (try OAuthCallbackListener(redirectURI: redirectURI), port)
            } catch {
                lastError = error
            }
        }
        throw lastError
    }

    /// Confirms the endpoint really let go of the port.
    private func rebind(port: Int, host: String = "localhost") async throws {
        let replacement = try OAuthCallbackListener(
            redirectURI: "http://\(host):\(port)/auth/callback"
        )
        replacement.close()
    }

    /// Gives the accept loop a moment to reach `poll` before a request arrives.
    private func settle() async throws { try await Task.sleep(for: .milliseconds(50)) }

    private func get(_ target: String) -> String {
        """
        GET \(target) HTTP/1.1\r
        Host: localhost\r
        \r

        """
    }

    /// Sends one raw request and immediately resets the connection, so the reply
    /// is written to a stream the peer has already torn down.
    private func sendAndReset(host: String, port: Int, raw: String) throws {
        let descriptor = try connect(host: host, port: port)
        var linger = linger(l_onoff: 1, l_linger: 0)
        setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_LINGER,
            &linger,
            socklen_t(MemoryLayout<linger>.size)
        )
        try writeAll(raw, to: descriptor)
        Darwin.close(descriptor)
    }

    /// Sends one raw request and returns everything the endpoint wrote back.
    private func request(host: String, port: Int, raw: String) throws -> String {
        let descriptor = try connect(host: host, port: port)
        defer { Darwin.close(descriptor) }
        var timeout = timeval(tv_sec: 5, tv_usec: 0)
        setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_RCVTIMEO,
            &timeout,
            socklen_t(MemoryLayout<timeval>.size)
        )
        try writeAll(raw, to: descriptor)
        var response = [UInt8]()
        var buffer = [UInt8](repeating: 0, count: 4_096)
        while true {
            let count = Darwin.read(descriptor, &buffer, buffer.count)
            guard count > 0 else { break }
            response.append(contentsOf: buffer[..<count])
        }
        return String(decoding: response, as: UTF8.self)
    }

    private func writeAll(_ raw: String, to descriptor: Int32) throws {
        let payload = Array(raw.utf8)
        var offset = 0
        while offset < payload.count {
            let written = payload[offset...].withUnsafeBytes {
                Darwin.write(descriptor, $0.baseAddress, $0.count)
            }
            guard written > 0 else {
                throw OAuthCallbackError.bindFailed(String(cString: strerror(errno)))
            }
            offset += written
        }
    }

    private func connect(host: String, port: Int) throws -> Int32 {
        var hints = addrinfo()
        hints.ai_flags = AI_NUMERICHOST | AI_NUMERICSERV
        hints.ai_family = AF_UNSPEC
        hints.ai_socktype = SOCK_STREAM
        var resolved: UnsafeMutablePointer<addrinfo>?
        guard getaddrinfo(host, String(port), &hints, &resolved) == 0,
              let address = resolved else {
            throw OAuthCallbackError.bindFailed("could not resolve \(host)")
        }
        defer { freeaddrinfo(resolved) }
        let descriptor = socket(
            address.pointee.ai_family,
            address.pointee.ai_socktype,
            address.pointee.ai_protocol
        )
        guard descriptor >= 0 else {
            throw OAuthCallbackError.bindFailed(String(cString: strerror(errno)))
        }
        guard Darwin.connect(descriptor, address.pointee.ai_addr, address.pointee.ai_addrlen) == 0
        else {
            Darwin.close(descriptor)
            throw OAuthCallbackError.bindFailed(String(cString: strerror(errno)))
        }
        return descriptor
    }
}
