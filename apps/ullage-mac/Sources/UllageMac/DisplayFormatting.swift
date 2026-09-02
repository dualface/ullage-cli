import Foundation
import UllageKit

func summaryValueText(_ value: SummaryValue) -> String {
    switch value {
    case .remains(let value): remainsText(value)
    case .used(let value): "used \(percentageText(value))"
    case .balance(let amount, let currency): "balance \(moneyText(amount, currency.code))"
    case .spent(let amount, let limit, let currency):
        "spent \(moneyText(amount, currency.code)) of \(moneyText(limit, currency.code))"
    case .credits(let used, let limit):
        "credits \(numberText(used))" + (limit.map { " of \(numberText($0))" } ?? "")
    case .creditsUnlimited: "credits unlimited"
    case .counted(let used, let limit):
        "used \(numberText(used))" + (limit.map { " of \(numberText($0))" } ?? "")
    case .disabled: "disabled"
    }
}

/// How much is left, in words when the number alone would mislead. "remains
/// 0%" reads like a measurement that failed rather than like quota that is
/// gone, and it also covers everything under half a percent, which is still
/// usable; those say how small they are instead.
func remainsText(_ value: Double) -> String {
    guard value.isFinite else { return "remains \(percentageText(value))" }
    if value <= 0 { return "used up" }
    if value < 0.5 { return "remains <1%" }
    return "remains \(percentageText(value))"
}

func numberText(_ value: Double) -> String {
    guard value.isFinite else { return "-" }
    if let integer = Int(exactly: value) { return String(integer) }
    return String(value)
}

func percentageText(_ value: Double) -> String {
    guard value.isFinite else { return "-" }
    return String(format: "%.0f%%", locale: Locale(identifier: "en_US_POSIX"), value.rounded())
}

func moneyText(_ value: Double, _ code: String) -> String {
    guard value.isFinite else { return "-" }
    let symbol: String? = switch code {
    case "USD": "$"
    case "EUR": "€"
    case "GBP": "£"
    default: nil
    }
    let amount = String(format: "%.2f", locale: Locale(identifier: "en_US_POSIX"), value)
    if let symbol { return symbol + amount }
    return code.isEmpty ? amount : code + " " + amount
}
