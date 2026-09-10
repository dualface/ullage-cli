import Foundation
import ServiceManagement
@testable import UllageKit
@testable import UllageMac
import XCTest

final class LocalFeatureTests: XCTestCase {
    func testCallbackOriginKeepsOnlyTheHTTPOrigin() {
        XCTAssertEqual(
            callbackOrigin("http://127.0.0.1:1455/callback?code=secret#fragment"),
            "http://127.0.0.1:1455"
        )
        XCTAssertEqual(callbackOrigin("https://example.test/callback"), "https://example.test")
        XCTAssertNil(callbackOrigin("not a callback URL"))
        XCTAssertNil(callbackOrigin("file:///tmp/callback"))
    }

    @MainActor
    func testTransportDefaultsLocalButMigratesPairedInstallationsToRemote() {
        withDefaults { defaults in
            XCTAssertEqual(AppSettings(defaults: defaults).transportMode, .local)
            XCTAssertEqual(defaults.string(forKey: "transportMode"), "local")
        }
        withDefaults { defaults in
            defaults.set("existing Mac", forKey: "pairedDeviceName")
            defaults.set(Date(timeIntervalSince1970: 1_777_777_777), forKey: "pairedAt")
            XCTAssertEqual(AppSettings(defaults: defaults).transportMode, .remote)
            XCTAssertEqual(defaults.string(forKey: "transportMode"), "remote")
        }
        withDefaults { defaults in
            defaults.set("existing Mac", forKey: "pairedDeviceName")
            defaults.set(Date(timeIntervalSince1970: 1_777_777_777), forKey: "pairedAt")
            defaults.set("local", forKey: "transportMode")
            XCTAssertEqual(AppSettings(defaults: defaults).transportMode, .local)
        }
    }

    @MainActor
    func testTransportMigrationRemovesRetiredServiceFlags() {
        withDefaults { defaults in
            defaults.set(true, forKey: "localServiceChoiceMade")
            defaults.set(false, forKey: "backgroundServiceEnabled")

            _ = AppSettings(defaults: defaults)

            XCTAssertNil(defaults.object(forKey: "localServiceChoiceMade"))
            XCTAssertNil(defaults.object(forKey: "backgroundServiceEnabled"))
        }
    }

    @MainActor
    func testLocalTransportRegistersTheServiceAutomatically() async {
        let defaults = temporaryDefaults()
        defer { defaults.removePersistentDomain(forName: defaultsSuite(defaults)) }
        let settings = AppSettings(defaults: defaults)
        let registration = LocalServiceRegistrationFixture(status: .notRegistered)
        let service = LocalServiceManager(
            settings: settings,
            service: registration,
            applicationBundleURL: URL(fileURLWithPath: "/Applications/Ullage.app")
        )

        await service.reconcileWithTransportMode()

        XCTAssertEqual(registration.registerCount, 1)
        XCTAssertEqual(registration.unregisterCount, 0)
        XCTAssertEqual(service.state, .enabled)
    }

    @MainActor
    func testRemoteTransportUnregistersTheServiceAutomatically() async {
        let defaults = temporaryDefaults()
        defer { defaults.removePersistentDomain(forName: defaultsSuite(defaults)) }
        let settings = AppSettings(defaults: defaults)
        settings.transportMode = .remote
        let registration = LocalServiceRegistrationFixture(status: .enabled)
        let service = LocalServiceManager(
            settings: settings,
            service: registration,
            applicationBundleURL: URL(fileURLWithPath: "/Applications/Ullage.app")
        )

        await service.reconcileWithTransportMode()

        XCTAssertEqual(registration.registerCount, 0)
        XCTAssertEqual(registration.unregisterCount, 1)
        XCTAssertEqual(service.state, .disabled)
    }

    @MainActor
    func testRestartCannotEnableTheServiceInRemoteMode() async {
        let defaults = temporaryDefaults()
        defer { defaults.removePersistentDomain(forName: defaultsSuite(defaults)) }
        let settings = AppSettings(defaults: defaults)
        settings.transportMode = .remote
        let registration = LocalServiceRegistrationFixture(status: .notRegistered)
        let service = LocalServiceManager(
            settings: settings,
            service: registration,
            applicationBundleURL: URL(fileURLWithPath: "/Applications/Ullage.app")
        )

        await service.restart()

        XCTAssertEqual(registration.registerCount, 0)
        XCTAssertEqual(registration.unregisterCount, 0)
        XCTAssertEqual(service.state, .disabled)
    }

    @MainActor
    func testLocalProtocolMismatchReachesConnectionState() async throws {
        let source = LocalMismatchDataSource()
        let store = UsageStore(dataSourceFactory: { source })
        store.start()
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(store.connectionState, .protocolMismatch(client: "10", server: "11"))
        store.stop()
    }

    @MainActor
    func testPostAuthenticationProbeFailurePreservesAccount() async throws {
        let recorder = LocalRequestRecorder()
        let client = LocalControlClient { request in
            let object = try XCTUnwrap(
                JSONSerialization.jsonObject(with: request.dropLast()) as? [String: Any]
            )
            let requestID = try XCTUnwrap(object["request_id"] as? String)
            let command = try XCTUnwrap(object["command"] as? [String: Any])
            let name = try XCTUnwrap(command["command"] as? String)
            recorder.append(name)
            return try localResponse(requestID: requestID, command: name)
        }
        let suite = "LocalFeatureTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let settings = AppSettings(defaults: defaults)
        let service = LocalServiceManager(settings: settings)
        let manager = LocalAccountManager(
            localService: service,
            dataChanged: {},
            clientFactory: { _ in client }
        )
        let model = LoginWizardModel(manager: manager)
        model.provider = "cursor"

        await model.begin()
        model.input = "secret-fixture"
        await model.submit()

        XCTAssertTrue(model.authenticated)
        XCTAssertFalse(model.setupCompleted)
        XCTAssertTrue(model.message.hasPrefix("Signed in, but setup is incomplete:"))
        await model.cancel()
        XCTAssertFalse(recorder.values.contains("logout"))
        XCTAssertFalse(recorder.values.contains("remove_account"))
    }

    @MainActor
    func testDeviceCodePollingReportsPostAuthenticationSetupFailure() async throws {
        let recorder = LocalRequestRecorder()
        let client = LocalControlClient { request in
            let object = try XCTUnwrap(
                JSONSerialization.jsonObject(with: request.dropLast()) as? [String: Any]
            )
            let requestID = try XCTUnwrap(object["request_id"] as? String)
            let command = try XCTUnwrap(object["command"] as? [String: Any])
            let name = try XCTUnwrap(command["command"] as? String)
            recorder.append(name)
            return try localResponse(
                requestID: requestID,
                command: name,
                authenticationMethod: "device_code"
            )
        }
        let suite = "LocalFeatureTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let settings = AppSettings(defaults: defaults)
        let service = LocalServiceManager(settings: settings)
        let manager = LocalAccountManager(
            localService: service,
            dataChanged: {},
            clientFactory: { _ in client }
        )
        let model = LoginWizardModel(manager: manager, pollingInterval: .milliseconds(1))
        model.provider = "grok"

        await model.begin()
        for _ in 0..<100 where !model.message.hasPrefix("Signed in, but setup is incomplete:") {
            try await Task.sleep(for: .milliseconds(10))
        }

        XCTAssertTrue(model.authenticated)
        XCTAssertFalse(model.setupCompleted)
        XCTAssertTrue(model.message.hasPrefix("Signed in, but setup is incomplete:"))
        XCTAssertEqual(recorder.values.filter { $0 == "complete_auth" }.count, 1)
        XCTAssertEqual(recorder.values.filter { $0 == "probe" }.count, 1)
        try await Task.sleep(for: .milliseconds(20))
        XCTAssertEqual(recorder.values.filter { $0 == "complete_auth" }.count, 1)
        XCTAssertEqual(recorder.values.filter { $0 == "probe" }.count, 1)
        await model.cancel()
        XCTAssertFalse(recorder.values.contains("logout"))
        XCTAssertFalse(recorder.values.contains("remove_account"))
    }

    @MainActor
    func testReadinessRetryUsesAnAbsoluteDeadlineAndCapsClientTimeouts() async {
        let suite = "LocalFeatureTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let settings = AppSettings(defaults: defaults)
        let service = LocalServiceManager(settings: settings)
        var timeouts: [TimeInterval] = []
        let manager = LocalAccountManager(
            localService: service,
            dataChanged: {},
            clientFactory: { timeout in
                timeouts.append(timeout)
                throw LocalControlError.timeout
            },
            readinessWindow: .milliseconds(30),
            readinessRetryDelay: .milliseconds(5)
        )
        let clock = ContinuousClock()
        let start = clock.now

        await manager.refresh(waitForService: true)

        XCTAssertFalse(timeouts.isEmpty)
        XCTAssertTrue(timeouts.allSatisfy { $0 > 0 && $0 <= 0.03 })
        XCTAssertLessThan(start.duration(to: clock.now), .seconds(1))
        XCTAssertEqual(manager.message, LocalControlError.timeout.localizedDescription)
    }

    private func withDefaults(_ body: (UserDefaults) -> Void) {
        let defaults = temporaryDefaults()
        defer { defaults.removePersistentDomain(forName: defaultsSuite(defaults)) }
        body(defaults)
    }

    private func temporaryDefaults() -> UserDefaults {
        let suite = "LocalFeatureTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.set(suite, forKey: "testSuiteName")
        return defaults
    }

    private func defaultsSuite(_ defaults: UserDefaults) -> String {
        defaults.string(forKey: "testSuiteName")!
    }
}

@MainActor
private final class LocalServiceRegistrationFixture: LocalServiceRegistration {
    var status: SMAppService.Status
    private(set) var registerCount = 0
    private(set) var unregisterCount = 0

    init(status: SMAppService.Status) {
        self.status = status
    }

    func register() throws {
        registerCount += 1
        status = .enabled
    }

    func unregister() async throws {
        unregisterCount += 1
        status = .notRegistered
    }
}

private struct LocalMismatchDataSource: UsageDataSource {
    func status() async throws -> DaemonStatusPayload { throw mismatch }
    func accounts() async throws -> [Account] { throw mismatch }
    func usage() async throws -> [SnapshotPayload] { throw mismatch }
    func probe(accountId: String, wait: Bool) async throws -> ProbeResult { throw mismatch }

    private var mismatch: LocalControlError { .protocolMismatch(client: 10, server: 11) }
}

private final class LocalRequestRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: [String] = []

    var values: [String] { lock.withLock { storage } }
    func append(_ value: String) { lock.withLock { storage.append(value) } }
}

private func localResponse(
    requestID: String,
    command: String,
    authenticationMethod: String = "api_token"
) throws -> Data {
    let result: [String: Any]
    switch command {
    case "add_account", "set_account_label":
        result = [
            "result": "account",
            "payload": ["id": "account-1", "provider": "cursor", "enabled": true],
        ]
    case "start_auth":
        result = [
            "result": "auth_challenge",
            "payload": [
                "flow_id": "flow-1",
                "method": authenticationMethod,
                "input": ["prompt": "API key", "secret": true],
            ],
        ]
    case "complete_auth", "auth_status":
        result = [
            "result": "auth_state",
            "payload": ["state": "authenticated", "account_label": "Personal"],
        ]
    case "list_providers":
        result = ["result": "providers", "payload": []]
    case "list_accounts":
        result = [
            "result": "accounts",
            "payload": [["id": "account-1", "provider": "cursor", "enabled": true]],
        ]
    case "probe":
        result = ["result": "error", "payload": ["kind": "timeout"]]
    default:
        result = ["result": "ack"]
    }
    return try JSONSerialization.data(withJSONObject: [
        "version": 10,
        "request_id": requestID,
        "result": result,
    ]) + Data([0x0a])
}
