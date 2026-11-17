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
    public private(set) var confirmingOff = false
    /// The value being saved while the change applies.
    public private(set) var applying: Bool?
    public private(set) var count: GitignoreCount?
    /// How many files stopped syncing when the switch was last turned on.
    public private(set) var noLongerSyncing: UInt64?
    public private(set) var failure: SessionError?
    @ObservationIgnored private var turnedOnAtMs: UInt64?
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
    }

    public var follows: Bool { applying ?? app.followsGitignore }

    public func set(follow: Bool) async {
        guard applying == nil, !confirmingOff, follow != app.followsGitignore else { return }
        failure = nil
        noLongerSyncing = nil
        if follow {
            await apply(true)
        } else {
            confirmingOff = true
            await recount()
        }
    }

    public func confirmOff() async {
        confirmingOff = false
        await apply(false)
    }

    public func cancelOff() {
        confirmingOff = false
    }

    /// Counts again and keeps the result unless a newer count already arrived.
    public func recount() async {
        guard let fresh = await app.gitignoreOnly(), fresh.countedAtMs >= count?.countedAtMs ?? 0 else { return }
        count = fresh
        if let turnedOnAtMs, fresh.countedAtMs >= turnedOnAtMs {
            noLongerSyncing = fresh.files
            self.turnedOnAtMs = nil
        }
    }

    private func apply(_ follow: Bool) async {
        applying = follow
        do {
            try await app.setGitignore(follow: follow)
        } catch {
            failure = error as? SessionError ?? .Failed(reason: error.localizedDescription)
        }
        applying = nil
        guard follow, failure == nil else { return }
        turnedOnAtMs = UInt64(Date.now.timeIntervalSince1970 * 1000)
        await recount()
    }
}
