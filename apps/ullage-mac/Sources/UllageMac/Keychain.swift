import Foundation
import Security

enum Keychain {
    static let deviceTokenStore = KeychainStore(
        service: "dev.ullage.mac.device-token",
        account: "daemon-device-token"
    )
    static let legacyTokenStore = KeychainStore(
        service: "dev.ullage.mac.http-token",
        account: "daemon-http-token"
    )

    static func replaceDeviceToken(
        _ token: String,
        deviceStore: KeychainStore = deviceTokenStore,
        legacyStore: KeychainStore = legacyTokenStore
    ) throws {
        try deviceStore.save(token)
        removeLegacyTokenIfPossible(from: legacyStore)
    }

    static func loadDeviceToken() throws -> String? {
        let token = try deviceTokenStore.load()
        if token != nil {
            removeLegacyTokenIfPossible(from: legacyTokenStore)
        }
        return token
    }

    private static func removeLegacyTokenIfPossible(from store: KeychainStore) {
        do {
            try store.delete()
        } catch {
            NSLog("Could not remove the retired shared-token Keychain item: \(error)")
        }
    }
}

struct KeychainStore {
    let service: String
    let account: String

    func save(_ token: String) throws {
        let data = Data(token.utf8)
        let query = baseQuery
        let update: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        let status = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if status == errSecItemNotFound {
            var add = query
            add[kSecValueData as String] = data
            add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
            try check(SecItemAdd(add as CFDictionary, nil))
        } else {
            try check(status)
        }
    }

    func load() throws -> String? {
        var query = baseQuery
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        try check(status)
        guard let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    func delete() throws {
        let status = SecItemDelete(baseQuery as CFDictionary)
        if status != errSecItemNotFound { try check(status) }
    }

    private var baseQuery: [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    private func check(_ status: OSStatus) throws {
        guard status == errSecSuccess else { throw KeychainError.status(status) }
    }
}

enum KeychainError: Error {
    case status(OSStatus)
}
