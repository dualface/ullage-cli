import Foundation

public enum RFC3339 {
    public static func date(from value: String) -> Date? {
        let pattern = #"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?(Z|[+-]\d{2}:\d{2})$"#
        guard let expression = try? NSRegularExpression(pattern: pattern) else { return nil }
        let range = NSRange(value.startIndex..<value.endIndex, in: value)
        guard let match = expression.firstMatch(in: value, range: range), match.range == range else {
            return nil
        }

        func capture(_ index: Int) -> String? {
            let captureRange = match.range(at: index)
            guard captureRange.location != NSNotFound,
                  let swiftRange = Range(captureRange, in: value) else { return nil }
            return String(value[swiftRange])
        }

        guard let year = capture(1).flatMap(Int.init),
              let month = capture(2).flatMap(Int.init),
              let day = capture(3).flatMap(Int.init),
              let hour = capture(4).flatMap(Int.init),
              let minute = capture(5).flatMap(Int.init),
              let second = capture(6).flatMap(Int.init),
              let zone = capture(8),
              let timeZone = timeZone(from: zone) else { return nil }

        let fraction = capture(7) ?? ""
        let nanosecond = Int(fraction.padding(toLength: 9, withPad: "0", startingAt: 0)) ?? 0
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = timeZone
        var components = DateComponents()
        components.calendar = calendar
        components.timeZone = timeZone
        components.year = year
        components.month = month
        components.day = day
        components.hour = hour
        components.minute = minute
        components.second = min(second, 59)
        components.nanosecond = second == 60 ? 0 : nanosecond
        guard let date = calendar.date(from: components) else { return nil }

        let roundTrip = calendar.dateComponents(
            [.year, .month, .day, .hour, .minute, .second],
            from: date
        )
        guard roundTrip.year == year, roundTrip.month == month, roundTrip.day == day,
              roundTrip.hour == hour, roundTrip.minute == minute,
              roundTrip.second == min(second, 59) else {
            return nil
        }
        if second == 60 {
            return date.addingTimeInterval(1 + Double(nanosecond) / 1_000_000_000)
        }
        return date
    }

    private static func timeZone(from value: String) -> TimeZone? {
        if value == "Z" { return TimeZone(secondsFromGMT: 0) }
        guard value.count == 6,
              let hours = Int(value.dropFirst().prefix(2)),
              let minutes = Int(value.suffix(2)),
              hours <= 23, minutes <= 59 else { return nil }
        let sign = value.first == "-" ? -1 : 1
        return TimeZone(secondsFromGMT: sign * (hours * 3_600 + minutes * 60))
    }
}

public enum UllageJSON {
    public static func makeDecoder() -> JSONDecoder {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .custom { decoder in
            let container = try decoder.singleValueContainer()
            let value = try container.decode(String.self)
            guard let date = RFC3339.date(from: value) else {
                throw DecodingError.dataCorruptedError(
                    in: container,
                    debugDescription: "Invalid RFC 3339 timestamp"
                )
            }
            return date
        }
        return decoder
    }
}
