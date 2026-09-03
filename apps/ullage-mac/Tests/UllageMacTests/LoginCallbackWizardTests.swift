import Foundation
@testable import UllageKit
@testable import UllageMac
import XCTest

/// Covers the wizard end of the loopback callback: an arriving callback finishes
/// the login on its own, and a port that cannot be bound leaves the pre-existing
/// manual paste path exactly as it was.
final class LoginCallbackWizardTests: XCTestCase {
    private let redirectURI = "http://localhost:1455/auth/callback"

    @MainActor
    func testAnArrivingCallbackCompletesTheLoginWithoutPasting() async throws {
        let recorder = ControlRecorder()
        let listener = CallbackListenerFixture(
            redirectURI: redirectURI,
            outcome: .success(
                .authorized(callbackURL: "\(redirectURI)?code=the-code&state=flow-1")
            )
        )
        let model = try makeModel(recorder: recorder) { _ in listener }
        model.provider = "chatgpt"

        await model.begin()
        try await waitUntil { model.message.hasPrefix("Signed in, but setup is incomplete:") }
        XCTAssertTrue(model.authenticated)

        let start = try XCTUnwrap(recorder.request(for: "start_auth"))
        XCTAssertEqual(start["redirect_uri"] as? String, redirectURI)
        XCTAssertEqual(start["method"] as? String, "browser_o_auth")
        XCTAssertEqual(listener.observedStates, ["flow-1"])

        let complete = try XCTUnwrap(recorder.request(for: "complete_auth"))
        XCTAssertEqual(
            complete["authorization_code"] as? String,
            "\(redirectURI)?code=the-code&state=flow-1"
        )
        XCTAssertEqual(complete["redirect_uri"] as? String, redirectURI)
        XCTAssertEqual(model.input, "")
    }

    @MainActor
    func testABindFailureKeepsTheManualPastePathUnchanged() async throws {
        let recorder = ControlRecorder()
        let model = try makeModel(recorder: recorder) { _ in
            throw OAuthCallbackError.bindFailed("Address already in use")
        }
        model.provider = "chatgpt"

        await model.begin()

        let start = try XCTUnwrap(recorder.request(for: "start_auth"))
        XCTAssertTrue(start["redirect_uri"] is NSNull)
        XCTAssertTrue(start["method"] is NSNull)
        XCTAssertTrue(model.message.contains("Address already in use"), model.message)
        XCTAssertTrue(model.message.contains("paste"), model.message)

        model.input = "\(redirectURI)?code=the-code&state=flow-1"
        await model.submit()

        let complete = try XCTUnwrap(recorder.request(for: "complete_auth"))
        XCTAssertEqual(complete["authorization_code"] as? String, model.input)
        XCTAssertEqual(complete["redirect_uri"] as? String, callbackOrigin(model.input))
        XCTAssertTrue(model.authenticated)
    }

    @MainActor
    func testCancellingTheWizardClosesTheCallbackEndpoint() async throws {
        let recorder = ControlRecorder()
        let listener = CallbackListenerFixture(redirectURI: redirectURI, outcome: nil)
        let model = try makeModel(recorder: recorder) { _ in listener }
        model.provider = "chatgpt"

        await model.begin()
        try await waitUntil { listener.observedStates == ["flow-1"] }
        await model.cancel()

        XCTAssertGreaterThanOrEqual(listener.closeCount, 1)
    }

    @MainActor
    func testADeviceCodeProviderKeepsPollingAndReleasesTheEndpoint() async throws {
        let recorder = ControlRecorder()
        let listener = CallbackListenerFixture(redirectURI: redirectURI, outcome: nil)
        let model = try makeModel(
            recorder: recorder,
            provider: "grok",
            authenticationMethod: "device_code"
        ) { _ in listener }
        model.provider = "grok"

        await model.begin()

        XCTAssertEqual(listener.closeCount, 1)
        XCTAssertTrue(listener.observedStates.isEmpty)
        await model.cancel()
    }

    // MARK: - Helpers

    @MainActor
    private func makeModel(
        recorder: ControlRecorder,
        provider: String = "chatgpt",
        authenticationMethod: String = "browser_o_auth",
        callbackListenerFactory: @escaping (String) throws -> OAuthCallbackListening
    ) throws -> LoginWizardModel {
        let client = LocalControlClient { data in
            let object = try XCTUnwrap(
                JSONSerialization.jsonObject(with: data.dropLast()) as? [String: Any]
            )
            let requestID = try XCTUnwrap(object["request_id"] as? String)
            let command = try XCTUnwrap(object["command"] as? [String: Any])
            let name = try XCTUnwrap(command["command"] as? String)
            recorder.record(name, command["request"] as? [String: Any] ?? [:])
            return try controlResponse(
                requestID: requestID,
                command: name,
                provider: provider,
                authenticationMethod: authenticationMethod
            )
        }
        let suite = "LoginCallbackWizardTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        addTeardownBlock { UserDefaults.standard.removePersistentDomain(forName: suite) }
        let manager = LocalAccountManager(
            localService: LocalServiceManager(settings: AppSettings(defaults: defaults)),
            dataChanged: {},
            clientFactory: { _ in client }
        )
        return LoginWizardModel(
            manager: manager,
            pollingInterval: .milliseconds(5),
            callbackListenerFactory: callbackListenerFactory
        )
    }

    @MainActor
    private func waitUntil(
        _ condition: () -> Bool,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async throws {
        for _ in 0..<200 {
            if condition() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("the expected state was never reached", file: file, line: line)
    }
}

/// A callback endpoint whose outcome the test decides. `outcome` of `nil` means
/// the browser never comes back, so the wait only ends when it is closed.
private final class CallbackListenerFixture: OAuthCallbackListening, @unchecked Sendable {
    let redirectURI: String
    private let outcome: Result<OAuthCallbackOutcome, Error>?
    private let lock = NSLock()
    private var states: [String] = []
    private var closes = 0

    var observedStates: [String] { lock.withLock { states } }
    var closeCount: Int { lock.withLock { closes } }

    init(redirectURI: String, outcome: Result<OAuthCallbackOutcome, Error>?) {
        self.redirectURI = redirectURI
        self.outcome = outcome
    }

    func waitForCallback(state: String, deadline: Date) async throws -> OAuthCallbackOutcome {
        lock.withLock { states.append(state) }
        guard let outcome else {
            try await Task.sleep(for: .seconds(60))
            throw OAuthCallbackError.timedOut
        }
        return try outcome.get()
    }

    func close() { lock.withLock { closes += 1 } }
}

private final class ControlRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: [(String, [String: Any])] = []

    func record(_ command: String, _ request: [String: Any]) {
        lock.withLock { storage.append((command, request)) }
    }

    func request(for command: String) -> [String: Any]? {
        lock.withLock { storage.first { $0.0 == command }?.1 }
    }
}

private func controlResponse(
    requestID: String,
    command: String,
    provider: String,
    authenticationMethod: String
) throws -> Data {
    let result: [String: Any]
    switch command {
    case "add_account", "set_account_label":
        result = [
            "result": "account",
            "payload": ["id": "account-1", "provider": provider, "enabled": true],
        ]
    case "start_auth":
        result = [
            "result": "auth_challenge",
            "payload": [
                "flow_id": "flow-1",
                "method": authenticationMethod,
                "input": ["prompt": "the full callback URL", "secret": false],
            ],
        ]
    case "complete_auth", "auth_status":
        result = [
            "result": "auth_state",
            "payload": ["state": "authenticated", "account_label": "Personal"],
        ]
    case "list_accounts":
        result = [
            "result": "accounts",
            "payload": [["id": "account-1", "provider": provider, "enabled": true]],
        ]
    case "list_providers":
        result = ["result": "providers", "payload": []]
    // The post-authentication probe is left failing on purpose: these tests are
    // about how the login is completed, not about the setup that follows it.
    case "probe":
        result = ["result": "error", "payload": ["kind": "timeout"]]
    default:
        result = ["result": "ack"]
    }
    return try JSONSerialization.data(withJSONObject: [
        "version": 9,
        "request_id": requestID,
        "result": result,
    ]) + Data([0x0a])
}
