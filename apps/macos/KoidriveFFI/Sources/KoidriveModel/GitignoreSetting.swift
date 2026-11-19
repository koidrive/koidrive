// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import Observation

/// The files that only `.gitignore` files leave out of the linked folders.
public struct GitignoreCount: Equatable, Sendable {
    public let files: UInt64
    public let bytes: UInt64
    /// The folders that hold at least one of the files.
    public let folderNames: [String]
    public let countedAtMs: UInt64

    public init(files: UInt64, bytes: UInt64, folderNames: [String], countedAtMs: UInt64) {
        self.files = files
        self.bytes = bytes
        self.folderNames = folderNames
        self.countedAtMs = countedAtMs
    }
}

/// The Follow .gitignore files switch: turning it off asks first, and turning it on reports how many files stopped
/// syncing once they are counted.
@MainActor
@Observable
public final class GitignoreSettingModel {
    private enum Phase: Equatable {
        case idle
        case countingBeforeAsking
        case confirmingOff
        case applying(Bool)
    }

    private final class Walk: @unchecked Sendable {
        let startedAtMs: UInt64
        let task: Task<GitignoreCount?, Never>
        private let lock = NSLock()
        private var waiters = 0

        init(startedAtMs: UInt64, task: Task<GitignoreCount?, Never>) {
            self.startedAtMs = startedAtMs
            self.task = task
        }

        func join() -> Bool {
            lock.withLock {
                guard !task.isCancelled else { return false }
                waiters += 1
                return true
            }
        }

        func leave() {
            lock.withLock {
                waiters -= 1
                if waiters == 0 { task.cancel() }
            }
        }

        func wait() async -> GitignoreCount? {
            await withTaskCancellationHandler { await task.value } onCancel: { leave() }
        }
    }

    private var phase = Phase.idle
    public private(set) var count: GitignoreCount?
    private var stoppedSyncing: UInt64?
    public private(set) var failure: SessionError?
    @ObservationIgnored private var walk: Walk?
    @ObservationIgnored private var changes = 0
    private let app: AppModel
    private let countWait: Duration

    /// Turning the switch off waits up to `countWait` for the count before it asks, so the question can name it.
    public init(app: AppModel, countWait: Duration = .seconds(1)) {
        self.app = app
        self.countWait = countWait
    }

    public var confirmingOff: Bool { phase == .confirmingOff }

    /// The value being saved while the change applies.
    public var applying: Bool? {
        if case .applying(let follow) = phase { follow } else { nil }
    }

    /// How many files stopped syncing when the switch was last turned on, while the setting stays on.
    public var noLongerSyncing: UInt64? { app.followsGitignore ? stoppedSyncing : nil }

    public var follows: Bool { applying ?? app.followsGitignore }

    public func set(follow: Bool) async {
        guard phase == .idle, follow != app.followsGitignore else { return }
        changes += 1
        failure = nil
        stoppedSyncing = nil
        if follow {
            await apply(true)
        } else {
            phase = .countingBeforeAsking
            await recount(within: countWait)
            phase = .confirmingOff
        }
    }

    /// Starts turning the switch off. The switch shows off when this returns.
    @discardableResult
    public func confirmOff() -> Task<Void, Never>? {
        guard phase == .confirmingOff else { return nil }
        phase = .applying(false)
        return Task { await apply(false) }
    }

    public func cancelOff() {
        guard phase == .confirmingOff else { return }
        phase = .idle
    }

    /// Counts again, joining a count already running, and keeps the result unless a newer count already arrived.
    /// Cancelling the caller stops the count before its next folder.
    public func recount() async {
        _ = await counted(startedAfterMs: 0)
    }

    private func recount(within limit: Duration) async {
        let counting = Task { await recount() }
        let first = FirstOf()
        await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in
            first.continuation = done
            Task {
                await counting.value
                first.resume()
            }
            Task {
                try? await Task.sleep(for: limit)
                first.resume()
            }
        }
    }

    private func counted(startedAfterMs: UInt64) async -> GitignoreCount? {
        let current: Walk
        if let walk, walk.startedAtMs >= startedAfterMs, walk.join() {
            current = walk
        } else {
            current = Walk(startedAtMs: Self.nowMs(), task: Task { [app] in await app.gitignoreOnly() })
            _ = current.join()
            walk = current
        }
        let fresh = await current.wait()
        if walk === current { walk = nil }
        if let fresh, fresh.countedAtMs >= count?.countedAtMs ?? 0 {
            count = fresh
        }
        return fresh
    }

    private func apply(_ follow: Bool) async {
        phase = .applying(follow)
        do {
            try await app.setGitignore(follow: follow)
        } catch {
            failure = error as? SessionError ?? .Failed(reason: error.localizedDescription)
        }
        phase = .idle
        guard follow, failure == nil else { return }
        let change = changes
        let fresh = await counted(startedAfterMs: Self.nowMs())
        if changes == change, let fresh {
            stoppedSyncing = fresh.files
        }
    }

    private static func nowMs() -> UInt64 {
        UInt64(Date.now.timeIntervalSince1970 * 1000)
    }
}

@MainActor
private final class FirstOf {
    var continuation: CheckedContinuation<Void, Never>?

    func resume() {
        continuation?.resume()
        continuation = nil
    }
}
