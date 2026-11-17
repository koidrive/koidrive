// SPDX-License-Identifier: Apache-2.0

import Foundation

/// About how long the transfers have left, rounded the way the popover says it.
public enum RemainingTime: Equatable, Comparable, Sendable {
    case minutes(Int)
    case hours(Int)

    init?(seconds: Double) {
        let minutes = seconds / 60
        guard minutes > 1 else { return nil }
        switch minutes {
        case ..<5: self = .minutes(Int(minutes.rounded()))
        case ..<57.5: self = .minutes(max(5, Int((minutes / 5).rounded()) * 5))
        case ..<90: self = .hours(1)
        default: self = .hours(Int((minutes / 60).rounded()))
        }
    }

    var minutes: Int {
        switch self {
        case .minutes(let minutes): minutes
        case .hours(let hours): hours * 60
        }
    }

    public static func < (lhs: Self, rhs: Self) -> Bool { lhs.minutes < rhs.minutes }
}

/// Estimates the time left from the bytes the folders moved over the network. It shows an estimate
/// only when the rate over the last 30 seconds is within 20% of the 30 seconds before, and starts
/// over when the daemon restarts or the backlog grows by more than 10%.
public struct TransferEstimator: Sendable {
    static let window: TimeInterval = 30
    static let steadiness = 0.2
    static let growth = 1.1

    private struct Sample {
        var at: Date
        var moved: UInt64
    }

    private var samples: [Sample] = []
    private var lastMoved: UInt64 = 0
    private var offset: UInt64 = 0
    private var rebaselinedAt: Date?
    private var baselineTotal: UInt64?
    private var rising: Date?
    public private(set) var shown: RemainingTime?

    public init() {}

    /// Records the folders' `bytesMoved` sum and the transfers' totals at `now`, and returns the
    /// estimate to show.
    public mutating func update(at now: Date, moved: UInt64, done: UInt64, total: UInt64) -> RemainingTime? {
        record(at: now, moved: moved, total: total)
        let estimate = estimate(at: now, left: total - min(done, total))
        guard let estimate, let current = shown else {
            shown = estimate
            rising = nil
            return shown
        }
        if estimate <= current {
            shown = estimate
            rising = nil
        } else if let since = rising, now.timeIntervalSince(since) >= Self.window {
            shown = estimate
            rising = nil
        } else if rising == nil {
            rising = now
        }
        return shown
    }

    private mutating func record(at now: Date, moved: UInt64, total: UInt64) {
        if moved < lastMoved {
            offset += lastMoved
            rebaselinedAt = now
        }
        lastMoved = moved
        if let baseline = baselineTotal, Double(total) > Double(baseline) * Self.growth {
            rebaselinedAt = now
            baselineTotal = total
        } else {
            baselineTotal = min(baselineTotal ?? total, total)
        }
        samples.append(Sample(at: now, moved: offset + moved))
        let horizon = now.addingTimeInterval(-2 * Self.window)
        if let keep = samples.lastIndex(where: { $0.at <= horizon }) {
            samples.removeFirst(keep)
        }
    }

    private func estimate(at now: Date, left: UInt64) -> RemainingTime? {
        let recentStart = now.addingTimeInterval(-Self.window)
        if let rebaselinedAt, rebaselinedAt > recentStart { return nil }
        guard let first = samples.first, first.at <= now.addingTimeInterval(-2 * Self.window) else { return nil }
        let movedBy = { (date: Date) in samples.last { $0.at <= date }?.moved ?? first.moved }
        let start = movedBy(now.addingTimeInterval(-2 * Self.window))
        let middle = movedBy(recentStart)
        let end = movedBy(now)
        let earlier = Double(middle - start) / Self.window
        let recent = Double(end - middle) / Self.window
        guard earlier > 0, recent > 0, abs(recent - earlier) <= earlier * Self.steadiness else { return nil }
        return RemainingTime(seconds: Double(left) / recent)
    }
}
