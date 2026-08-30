import Foundation

public func sortedAccounts(_ accounts: [Account]) -> [Account] {
    accounts.sorted { lhs, rhs in
        let left = (lhs.provider, lhs.label ?? "", lhs.id)
        let right = (rhs.provider, rhs.label ?? "", rhs.id)
        return left < right
    }
}

public func tabTitles(for accounts: [Account]) -> [String: String] {
    let counts = Dictionary(grouping: accounts, by: \.provider).mapValues(\.count)
    return Dictionary(uniqueKeysWithValues: accounts.map { account in
        let providerName = providerDisplayName(account.provider)
        guard counts[account.provider, default: 0] > 1 else {
            return (account.id, providerName)
        }
        let suffix = account.label?.trimmingCharacters(in: .whitespacesAndNewlines)
        let accountName = suffix.flatMap { $0.isEmpty ? nil : $0 } ?? account.id
        return (account.id, providerName + " · " + accountName)
    })
}

public func providerDisplayName(_ provider: String) -> String {
    return switch provider {
    case "chatgpt": "ChatGPT"
    case "claude": "Claude"
    case "cursor": "Cursor"
    case "grok": "Grok"
    default: provider.first.map { $0.uppercased() + provider.dropFirst() } ?? provider
    }
}
