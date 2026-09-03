import Darwin
import Foundation

/// What a browser delivered to the loopback callback endpoint.
enum OAuthCallbackOutcome: Equatable, Sendable {
    /// The callback carried a matching `state` and an authorization code. The
    /// associated value is the callback URL the provider expects to receive
    /// back; it holds the authorization code and must never be logged.
    case authorized(callbackURL: String)
    /// The authorization server reported a failure instead of a code.
    case declined(reason: String)
}

enum OAuthCallbackError: Error, Equatable, CustomStringConvertible, LocalizedError {
    case unsupportedRedirectURI
    case bindFailed(String)
    case timedOut
    case cancelled

    var description: String {
        switch self {
        case .unsupportedRedirectURI:
            "the callback address is not a loopback HTTP URL"
        case .bindFailed(let reason):
            "the callback port could not be opened: \(reason)"
        case .timedOut:
            "the browser did not return to the callback address in time"
        case .cancelled:
            "the callback listener was closed"
        }
    }

    var errorDescription: String? { description }
}

/// A one-shot loopback HTTP endpoint that receives a browser OAuth callback.
protocol OAuthCallbackListening: AnyObject, Sendable {
    /// The exact redirect URI the listener is bound to, as sent to the daemon.
    var redirectURI: String { get }
    /// Waits for one callback that carries `state`. The listener is closed when
    /// this returns or throws, so it never serves a second authorization.
    func waitForCallback(state: String, deadline: Date) async throws -> OAuthCallbackOutcome
    /// Closes the endpoint. Safe to call more than once and from any thread.
    func close()
}

/// Registered loopback redirect URIs, mirroring the provider defaults the Rust
/// crates use when a client sends no `redirect_uri` (`crates/ullage-app`).
/// A provider absent from this table has no loopback callback and keeps the
/// manual paste flow.
func registeredLoopbackRedirectURI(forProvider provider: String) -> String? {
    switch provider {
    case "chatgpt": "http://localhost:1455/auth/callback"
    case "grok": "http://127.0.0.1:1456/auth/callback"
    default: nil
    }
}

final class OAuthCallbackListener: OAuthCallbackListening, @unchecked Sendable {
    /// Whole request head, including the request line and every header.
    static let maximumRequestBytes = 8 * 1024
    /// Request line on its own, which is where a callback carries its query.
    static let maximumRequestLineBytes = 2_048
    /// How long one connection may take to deliver its request head.
    static let connectionTimeout: TimeInterval = 5
    /// How often the accept loop rechecks the deadline and the close signal.
    private static let pollSliceMilliseconds: Int32 = 250

    let redirectURI: String
    private let path: String
    private let listeners: [Int32]
    private let wakeupRead: Int32
    private let wakeupWrite: Int32
    private let lock = NSLock()
    private enum Phase { case idle, running, finished }
    private var phase: Phase = .idle

    /// Binds the loopback port named by `redirectURI`. Binding happens here, so
    /// a caller can only start an authorization once it owns the port.
    init(redirectURI: String) throws {
        guard let components = URLComponents(string: redirectURI),
              components.scheme?.lowercased() == "http",
              let host = components.host?.lowercased(),
              let port = components.port,
              port > 0, port <= Int(UInt16.max),
              !components.path.isEmpty,
              components.query == nil,
              components.fragment == nil,
              components.user == nil,
              components.password == nil else {
            throw OAuthCallbackError.unsupportedRedirectURI
        }
        let families = try Self.loopbackFamilies(forHost: host)
        self.redirectURI = redirectURI
        path = components.path

        var pipeDescriptors: [Int32] = [-1, -1]
        guard pipe(&pipeDescriptors) == 0 else {
            throw OAuthCallbackError.bindFailed(Self.reason(errno))
        }
        wakeupRead = pipeDescriptors[0]
        wakeupWrite = pipeDescriptors[1]

        var bound: [Int32] = []
        do {
            for family in families {
                do {
                    bound.append(try Self.bindLoopback(family: family, port: UInt16(port)))
                } catch let failure as BindFailure {
                    // A host without IPv6 support is not a squatted port: keep
                    // the IPv4 endpoint rather than dropping to manual paste.
                    guard family == AF_INET6,
                          !bound.isEmpty,
                          failure.code == EAFNOSUPPORT || failure.code == EADDRNOTAVAIL else {
                        throw failure.reported
                    }
                }
            }
        } catch {
            for descriptor in bound { Darwin.close(descriptor) }
            Darwin.close(wakeupRead)
            Darwin.close(wakeupWrite)
            throw error
        }
        listeners = bound
    }

    deinit {
        lock.lock()
        if phase == .idle {
            phase = .finished
            closeDescriptors()
        }
        lock.unlock()
    }

    /// The lock is held across the wakeup write so the accept loop cannot close
    /// the pipe, and have its descriptor number reused, while this writes to it.
    func close() {
        lock.lock()
        defer { lock.unlock() }
        switch phase {
        case .idle:
            phase = .finished
            closeDescriptors()
        case .running:
            var signal: UInt8 = 1
            _ = withUnsafeBytes(of: &signal) { Darwin.write(wakeupWrite, $0.baseAddress, 1) }
        case .finished:
            break
        }
    }

    func waitForCallback(state: String, deadline: Date) async throws -> OAuthCallbackOutcome {
        try await withTaskCancellationHandler {
            try await Task.detached(priority: .userInitiated) { [self] in
                try serve(state: state, deadline: deadline)
            }.value
        } onCancel: {
            close()
        }
    }

    // MARK: - Accept loop

    private func serve(state: String, deadline: Date) throws -> OAuthCallbackOutcome {
        lock.lock()
        guard phase == .idle else {
            lock.unlock()
            throw OAuthCallbackError.cancelled
        }
        phase = .running
        lock.unlock()
        defer {
            lock.lock()
            phase = .finished
            closeDescriptors()
            lock.unlock()
        }

        var descriptors = listeners.map { pollfd(fd: $0, events: Int16(POLLIN), revents: 0) }
        descriptors.append(pollfd(fd: wakeupRead, events: Int16(POLLIN), revents: 0))
        while true {
            let remaining = deadline.timeIntervalSinceNow
            guard remaining > 0 else { throw OAuthCallbackError.timedOut }
            let slice = Int32(min(Double(Self.pollSliceMilliseconds), max(1, remaining * 1_000)))
            for index in descriptors.indices { descriptors[index].revents = 0 }
            let ready = Darwin.poll(&descriptors, nfds_t(descriptors.count), slice)
            if ready < 0 {
                if errno == EINTR { continue }
                throw OAuthCallbackError.bindFailed(Self.reason(errno))
            }
            guard ready > 0 else { continue }
            if descriptors[descriptors.count - 1].revents != 0 {
                throw OAuthCallbackError.cancelled
            }
            for descriptor in descriptors.dropLast() where descriptor.revents & Int16(POLLIN) != 0 {
                let connection = Darwin.accept(descriptor.fd, nil, nil)
                guard connection >= 0 else { continue }
                defer { Darwin.close(connection) }
                let connectionDeadline = min(
                    Date().addingTimeInterval(Self.connectionTimeout),
                    deadline
                )
                if let outcome = handle(
                    connection: connection,
                    state: state,
                    deadline: connectionDeadline
                ) {
                    return outcome
                }
            }
        }
    }

    /// Serves one connection. Returns `nil` for anything that is not this
    /// flow's callback, so a stray request cannot end the authorization.
    private func handle(
        connection: Int32,
        state: String,
        deadline: Date
    ) -> OAuthCallbackOutcome? {
        let flags = fcntl(connection, F_GETFL)
        guard flags >= 0, fcntl(connection, F_SETFL, flags | O_NONBLOCK) == 0 else { return nil }
        guard let requestLine = readHead(connection: connection, deadline: deadline) else {
            respond(connection, "400 Bad Request", Self.rejectedBody, deadline)
            return nil
        }
        guard let request = OAuthCallbackRequest(requestLine: requestLine),
              request.method == "GET",
              request.path == path else {
            respond(connection, "404 Not Found", Self.notFoundBody, deadline)
            return nil
        }
        guard let carried = request.value(of: "state"), carried == state else {
            respond(connection, "400 Bad Request", Self.rejectedBody, deadline)
            return nil
        }
        if let failure = request.value(of: "error") {
            respond(connection, "200 OK", Self.declinedBody, deadline)
            return .declined(reason: Self.readableReason(failure))
        }
        guard request.value(of: "code")?.isEmpty == false else {
            respond(connection, "400 Bad Request", Self.rejectedBody, deadline)
            return nil
        }
        respond(connection, "200 OK", Self.successBody, deadline)
        return .authorized(callbackURL: redirectURI + "?" + request.query)
    }

    /// Reads the whole request head and returns its first line. The head is read
    /// to its blank line so the peer's send buffer is drained before the reply,
    /// and both the request line and the head as a whole are bounded so a peer
    /// cannot hold the flow open with an endless stream of headers.
    private func readHead(connection: Int32, deadline: Date) -> String? {
        var head = [UInt8]()
        var buffer = [UInt8](repeating: 0, count: 1_024)
        while !Self.headIsComplete(head) {
            let lineEnd = head.firstIndex(of: 0x0a) ?? head.count
            guard lineEnd <= Self.maximumRequestLineBytes,
                  head.count < Self.maximumRequestBytes,
                  wait(descriptor: connection, events: Int16(POLLIN), deadline: deadline) else {
                return nil
            }
            let count = Darwin.read(connection, &buffer, buffer.count)
            if count < 0 {
                if errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK { continue }
                return nil
            }
            guard count > 0 else { return nil }
            head.append(contentsOf: buffer[..<count])
        }
        guard let lineEnd = head.firstIndex(of: 0x0a), lineEnd <= Self.maximumRequestLineBytes
        else { return nil }
        var line = Array(head[..<lineEnd])
        if line.last == 0x0d { line.removeLast() }
        return String(decoding: line, as: UTF8.self)
    }

    private static func headIsComplete(_ head: [UInt8]) -> Bool {
        guard head.count >= 2 else { return false }
        for index in 0...(head.count - 2) where head[index] == 0x0a {
            if head[index + 1] == 0x0a { return true }
            if head[index + 1] == 0x0d, index + 2 < head.count, head[index + 2] == 0x0a {
                return true
            }
        }
        return false
    }

    /// Writes one response within the connection's own budget, so a peer that
    /// stops reading cannot hold the endpoint past the flow's deadline.
    private func respond(_ connection: Int32, _ status: String, _ body: String, _ deadline: Date) {
        let payload = Array(body.utf8)
        let head = """
        HTTP/1.1 \(status)\r
        Content-Type: text/html; charset=utf-8\r
        Content-Length: \(payload.count)\r
        Cache-Control: no-store\r
        Connection: close\r
        \r

        """
        var response = Array(head.utf8)
        response.append(contentsOf: payload)
        var offset = 0
        while offset < response.count {
            guard wait(descriptor: connection, events: Int16(POLLOUT), deadline: deadline) else {
                return
            }
            let written = response[offset...].withUnsafeBytes { bytes in
                Darwin.write(connection, bytes.baseAddress, bytes.count)
            }
            if written < 0 {
                if errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK { continue }
                return
            }
            guard written > 0 else { return }
            offset += written
        }
    }

    private func wait(descriptor: Int32, events: Int16, deadline: Date) -> Bool {
        var item = pollfd(fd: descriptor, events: events, revents: 0)
        while true {
            let remaining = deadline.timeIntervalSinceNow
            guard remaining > 0 else { return false }
            let milliseconds = Int32(min(Double(Int32.max), max(1, remaining * 1_000)))
            let ready = Darwin.poll(&item, 1, milliseconds)
            if ready > 0 { return item.revents & events != 0 }
            if ready == 0 { return false }
            if errno != EINTR { return false }
        }
    }

    private func closeDescriptors() {
        for descriptor in listeners { Darwin.close(descriptor) }
        Darwin.close(wakeupRead)
        Darwin.close(wakeupWrite)
    }

    // MARK: - Binding

    private static func loopbackFamilies(forHost host: String) throws -> [Int32] {
        switch host.trimmingCharacters(in: CharacterSet(charactersIn: "[]")) {
        // `localhost` resolves to both loopback addresses on macOS and browsers
        // often prefer `::1`, so this spelling has to answer on both.
        case "localhost": [AF_INET, AF_INET6]
        case "127.0.0.1": [AF_INET]
        case "::1": [AF_INET6]
        default: throw OAuthCallbackError.unsupportedRedirectURI
        }
    }

    /// A failed bind together with the `errno` that caused it, so the caller can
    /// tell "this host has no IPv6" from "somebody else holds this port".
    private struct BindFailure: Error {
        let code: Int32
        var reported: OAuthCallbackError { .bindFailed(reason(code)) }
    }

    /// Binds one loopback address literally, never a wildcard, so the added
    /// `com.apple.security.network.server` entitlement cannot reach the network.
    private static func bindLoopback(family: Int32, port: UInt16) throws -> Int32 {
        let descriptor = Darwin.socket(family, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw BindFailure(code: errno) }
        var failure: BindFailure?
        // A previous flow leaves the port in TIME_WAIT; without this a second
        // login within a couple of minutes would drop to manual paste. It never
        // lets a second listener take this exact address and port.
        var enabled: Int32 = 1
        if setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_REUSEADDR,
            &enabled,
            socklen_t(MemoryLayout<Int32>.size)
        ) != 0 {
            failure = BindFailure(code: errno)
        }
        if failure == nil, family == AF_INET6 {
            var only: Int32 = 1
            if setsockopt(
                descriptor,
                Int32(IPPROTO_IPV6),
                IPV6_V6ONLY,
                &only,
                socklen_t(MemoryLayout<Int32>.size)
            ) != 0 {
                failure = BindFailure(code: errno)
            }
        }
        if failure == nil {
            let flags = fcntl(descriptor, F_GETFL)
            if flags < 0 || fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) != 0 {
                failure = BindFailure(code: errno)
            }
        }
        if failure == nil {
            let bound: Int32
            if family == AF_INET {
                var address = sockaddr_in()
                address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
                address.sin_family = sa_family_t(AF_INET)
                address.sin_port = port.bigEndian
                address.sin_addr.s_addr = INADDR_LOOPBACK.bigEndian
                bound = withUnsafePointer(to: &address) { pointer in
                    pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                        Darwin.bind(descriptor, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
                    }
                }
            } else {
                var address = sockaddr_in6()
                address.sin6_len = UInt8(MemoryLayout<sockaddr_in6>.size)
                address.sin6_family = sa_family_t(AF_INET6)
                address.sin6_port = port.bigEndian
                address.sin6_addr = in6addr_loopback
                bound = withUnsafePointer(to: &address) { pointer in
                    pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                        Darwin.bind(descriptor, $0, socklen_t(MemoryLayout<sockaddr_in6>.size))
                    }
                }
            }
            if bound != 0 { failure = BindFailure(code: errno) }
        }
        if failure == nil, Darwin.listen(descriptor, 8) != 0 {
            failure = BindFailure(code: errno)
        }
        if let failure {
            Darwin.close(descriptor)
            throw failure
        }
        return descriptor
    }

    private static func reason(_ code: Int32) -> String {
        String(cString: strerror(code))
    }

    /// Reduces an authorization server's `error` value to a short printable
    /// token, so a hostile callback cannot inject control characters or an
    /// unbounded string into the wizard's message.
    static func readableReason(_ value: String) -> String {
        let printable = value.unicodeScalars.filter {
            !CharacterSet.controlCharacters.contains($0)
        }
        let trimmed = String(String.UnicodeScalarView(printable.prefix(64)))
            .trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? "an unspecified error" : trimmed
    }

    private static func page(_ heading: String, _ detail: String) -> String {
        """
        <!doctype html><html lang="en"><head><meta charset="utf-8">\
        <title>Ullage</title></head><body style="font: 15px -apple-system, sans-serif; \
        margin: 4rem auto; max-width: 28rem; text-align: center">\
        <h1 style="font-size: 1.2rem">\(heading)</h1><p>\(detail)</p></body></html>
        """
    }

    private static let successBody = page(
        "Signed in",
        "Ullage received the authorization. You can close this tab."
    )
    private static let declinedBody = page(
        "Not signed in",
        "The authorization was not granted. Return to Ullage to try again."
    )
    private static let rejectedBody = page(
        "Request rejected",
        "This request does not belong to a sign-in Ullage started."
    )
    private static let notFoundBody = page("Not found", "There is nothing at this address.")
}

/// The parsed request line of a callback request. Only the method, path, and
/// query are read; the query is kept verbatim because it has to be handed back
/// to the provider exactly as the authorization server wrote it.
struct OAuthCallbackRequest: Equatable {
    let method: String
    let path: String
    let query: String

    init?(requestLine: String) {
        let fields = requestLine.split(separator: " ", omittingEmptySubsequences: false)
        guard fields.count == 3, fields[2].hasPrefix("HTTP/") else { return nil }
        method = String(fields[0])
        let target = fields[1]
        if let separator = target.firstIndex(of: "?") {
            path = String(target[..<separator])
            query = String(target[target.index(after: separator)...])
        } else {
            path = String(target)
            query = ""
        }
        guard !path.isEmpty else { return nil }
    }

    func value(of name: String) -> String? {
        for pair in query.split(separator: "&", omittingEmptySubsequences: true) {
            let separator = pair.firstIndex(of: "=")
            let key = separator.map { String(pair[..<$0]) } ?? String(pair)
            guard key == name else { continue }
            guard let separator else { return "" }
            let raw = String(pair[pair.index(after: separator)...]).replacingOccurrences(
                of: "+",
                with: " "
            )
            return raw.removingPercentEncoding ?? raw
        }
        return nil
    }
}
