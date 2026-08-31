import Foundation
import XCTest
@testable import UllageKit

final class DaemonClientIntegrationTests: XCTestCase {
    func testStatusAndAccounts() async throws {
        let configuration = try integrationConfiguration()
        let client = DaemonClient(baseURL: configuration.baseURL, token: configuration.token)

        let status = try await client.status()
        XCTAssertEqual(status.version, DaemonClient.protocolVersion)

        let accounts = try await client.accounts()
        XCTAssertFalse(accounts.isEmpty)
    }

    func testUsageProjections() async throws {
        let configuration = try integrationConfiguration()
        let client = DaemonClient(baseURL: configuration.baseURL, token: configuration.token)
        let accounts = try await client.accounts()
        let snapshots: [SnapshotPayload] = try await decodeChecked("snapshot collection") {
            try await client.usage()
        }
        XCTAssertFalse(snapshots.isEmpty)

        for (index, account) in accounts.enumerated() {
            let accountSnapshots: [SnapshotPayload] = try await decodeChecked("account \(index) usage") {
                try await client.usage(accountId: account.id)
            }
            XCTAssertEqual(accountSnapshots.count, 1)
            let payload = try XCTUnwrap(accountSnapshots.first?.usage.data)
            let rows = overviewRows(for: payload)
            XCTAssertFalse(rows.isEmpty)
            XCTAssertTrue(rows.allSatisfy { $0.remainingRatio != nil || hasNumericValue($0.value) })
        }
    }

    func testBothLoopbackHostsAndRejectedToken() async throws {
        let configuration = try integrationConfiguration()
        let localhostURL = try XCTUnwrap(loopbackURL(configuration.baseURL, host: "localhost"))
        _ = try await DaemonClient(baseURL: localhostURL, token: configuration.token).status()

        do {
            _ = try await DaemonClient(
                baseURL: configuration.baseURL,
                token: configuration.token + "-invalid"
            ).status()
            XCTFail("Expected a rejected token")
        } catch let error as DaemonError {
            guard case .unauthorized = error else {
                return XCTFail("Expected unauthorized, got \(error)")
            }
        }
    }

    /// The Settings panel maps `DaemonError.unreachable` to "unreachable".
    /// A closed loopback port on the same host reproduces that state against
    /// a real network stack without touching the daemon.
    func testClosedPortIsUnreachable() async throws {
        let configuration = try integrationConfiguration()
        let closedPortURL = try XCTUnwrap(loopbackURL(configuration.baseURL, host: "127.0.0.1", port: 9))
        do {
            _ = try await DaemonClient(baseURL: closedPortURL, token: configuration.token).status()
            XCTFail("Expected an unreachable endpoint")
        } catch let error as DaemonError {
            guard case .unreachable = error else {
                return XCTFail("Expected unreachable, got \(error)")
            }
        }
    }

    /// The Settings panel maps `DaemonError.forbiddenHost` to "host rejected".
    /// The daemon only allows `127.0.0.1:<port>`, `localhost:<port>`, and its
    /// bind address as `Host`, so any other loopback name that still reaches
    /// the daemon (for example `[::1]` behind an SSH tunnel) must be refused
    /// with 403 before authentication. The URL is opt-in because it depends
    /// on how the daemon or tunnel is bound on the test machine.
    func testNonAllowlistedHostIsRejected() async throws {
        let configuration = try integrationConfiguration()
        guard let rawURL = ProcessInfo.processInfo.environment["ULLAGE_INTEGRATION_FORBIDDEN_HOST_URL"],
              let forbiddenHostURL = URL(string: rawURL) else {
            throw XCTSkip("Set ULLAGE_INTEGRATION_FORBIDDEN_HOST_URL to a reachable non-allowlisted daemon URL")
        }
        do {
            _ = try await DaemonClient(baseURL: forbiddenHostURL, token: configuration.token).status()
            XCTFail("Expected the daemon to reject the Host header")
        } catch let error as DaemonError {
            guard case .forbiddenHost = error else {
                return XCTFail("Expected forbiddenHost, got \(error)")
            }
        }
    }

    func testUnknownAccountIsNotFound() async throws {
        let configuration = try integrationConfiguration()
        let client = DaemonClient(baseURL: configuration.baseURL, token: configuration.token)
        do {
            _ = try await client.usage(accountId: "ullage-integration-missing-account")
            XCTFail("Expected an unknown account to fail")
        } catch let error as DaemonError {
            guard case .notFound = error else {
                return XCTFail("Expected notFound, got \(error)")
            }
        }
    }
}

private struct IntegrationConfiguration {
    let baseURL: URL
    let token: String
}

private func integrationConfiguration() throws -> IntegrationConfiguration {
    let environment = ProcessInfo.processInfo.environment
    guard let rawBaseURL = environment["ULLAGE_INTEGRATION_BASE_URL"],
          let tokenFile = environment["ULLAGE_INTEGRATION_TOKEN_FILE"] else {
        throw XCTSkip("Set ULLAGE_INTEGRATION_BASE_URL and ULLAGE_INTEGRATION_TOKEN_FILE to run")
    }
    guard let baseURL = URL(string: rawBaseURL) else {
        throw IntegrationConfigurationError.invalidBaseURL
    }
    let token = try String(contentsOfFile: tokenFile, encoding: .utf8)
        .trimmingCharacters(in: .whitespacesAndNewlines)
    guard !token.isEmpty else { throw IntegrationConfigurationError.emptyToken }
    return IntegrationConfiguration(baseURL: baseURL, token: token)
}

private func loopbackURL(_ url: URL, host: String, port: Int? = nil) -> URL? {
    guard var components = URLComponents(url: url, resolvingAgainstBaseURL: false) else { return nil }
    components.host = host
    if let port { components.port = port }
    return components.url
}

private func hasNumericValue(_ value: SummaryValue) -> Bool {
    switch value {
    case .remains, .used, .balance, .spent, .credits, .counted: true
    case .creditsUnlimited, .disabled: false
    }
}

private enum IntegrationConfigurationError: Error {
    case invalidBaseURL
    case emptyToken
}

private func decodeChecked<Value>(
    _ context: String,
    operation: () async throws -> Value
) async throws -> Value {
    do {
        return try await operation()
    } catch let error as DaemonError {
        if case .decoding(let underlying) = error {
            XCTFail("\(context) decoding failed: \(underlying)")
        }
        throw error
    }
}
