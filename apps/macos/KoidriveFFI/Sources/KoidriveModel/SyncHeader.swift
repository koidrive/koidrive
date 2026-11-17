// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI

/// What the popover shows under a Syncing title, top to bottom.
public struct SyncHeader: Equatable, Sendable {
    /// The number of folders the title counts: syncing folders that do more than wait for answers.
    public var folders: Int
    /// The only busy folder's name.
    public var folder: String?
    /// What the only busy folder is doing, above the meter.
    public var activity: ActivityLine?
    public var meter: Meter
    /// Whether the meter shows the transfers rather than the only folder's comparison.
    public var meterShowsTransfers = false
    public var uploads: UInt64
    public var downloads: UInt64
    public var current: FolderPath?
    /// A scan, a comparison or a question in another folder, below the transfers.
    public var note: ActivityLine?
}

public enum ActivityLine: Equatable, Sendable {
    /// `files` is nil when the meter shows the count.
    case scanning(folder: String, files: UInt64?)
    case listing(folder: String)
    case comparing(folder: String, files: UInt64)
    case applying(folder: String, changes: UInt64)
    case finishingUp(folder: String)
    case waitingForAnswer(FolderNames)
}

public enum FolderNames: Equatable, Sendable {
    case one(String)
    case two(String, String)
    case several(Int)

    init?(_ names: [String]) {
        switch names.count {
        case 0: return nil
        case 1: self = .one(names[0])
        case 2: self = .two(names[0], names[1])
        default: self = .several(names.count)
        }
    }
}

public struct Meter: Equatable, Sendable {
    public enum Fill: Equatable, Sendable {
        case fraction(Double)
        case sliding
    }

    public enum Caption: Equatable, Sendable {
        case filesFound(UInt64)
        case bytes(done: UInt64, total: UInt64)
        case left(bytes: UInt64, files: UInt64)
    }

    public var fill: Fill
    /// Beside the bar.
    public var caption: Caption?
    /// On its own row under a full-width bar, with the time left on the right.
    public var backlog: Caption?
    public var remaining: RemainingTime?

    public init(fill: Fill, caption: Caption? = nil, backlog: Caption? = nil, remaining: RemainingTime? = nil) {
        self.fill = fill
        self.caption = caption
        self.backlog = backlog
        self.remaining = remaining
    }
}

extension LinkedFolder {
    var awaitsAnswers: Bool {
        guard state == .syncing, case .awaitingAnswers = activity else { return false }
        return true
    }

    var isBusy: Bool { state == .syncing && !awaitsAnswers }
}

extension SyncHeader {
    public static let backlogFrom: UInt64 = 1_000_000_000

    init(state: SessionState, checkingTotals: [String: UInt64], remaining: RemainingTime?) {
        let busy = state.folders.filter(\.isBusy)
        let progress = state.progress
        let only = busy.count == 1 ? busy[0] : nil
        let transfers = progress != nil
        folders = busy.count
        folder = only?.spaceName
        uploads = progress?.filesToUpload ?? 0
        downloads = progress?.filesToDownload ?? 0
        current = progress?.current

        if let only, case .checking(_, let bytes) = only.activity, bytes > 0 {
            let total = max(bytes, checkingTotals[only.spaceId] ?? 0)
            meter = Meter(fill: .fraction(Double(total - bytes) / Double(total)), caption: .bytes(done: total - bytes, total: total))
        } else if let progress, progress.bytesTotal > 0 {
            let done = min(progress.bytesDone, progress.bytesTotal)
            let left = progress.bytesTotal - done
            let fill = Meter.Fill.fraction(Double(done) / Double(progress.bytesTotal))
            let bytes = Meter.Caption.bytes(done: done, total: progress.bytesTotal)
            meterShowsTransfers = true
            if left >= Self.backlogFrom {
                meter = Meter(fill: fill, backlog: .left(bytes: left, files: uploads + downloads), remaining: remaining)
            } else if remaining != nil {
                meter = Meter(fill: fill, backlog: bytes, remaining: remaining)
            } else {
                meter = Meter(fill: fill, caption: bytes)
            }
        } else if !transfers, let only, case .scanning(let files) = only.activity {
            meter = Meter(fill: .sliding, caption: .filesFound(files))
        } else {
            meter = Meter(fill: .sliding)
        }

        activity = only.flatMap { folder in
            switch folder.activity {
            case nil, .awaitingAnswers: nil
            case .scanning: transfers ? nil : .scanning(folder: folder.spaceName, files: nil)
            case .listing: .listing(folder: folder.spaceName)
            case .checking(let files, _): .comparing(folder: folder.spaceName, files: files)
            case .applying(let changes): .applying(folder: folder.spaceName, changes: changes)
            case .catchingUp: .finishingUp(folder: folder.spaceName)
            }
        }
        let busyNote = busy.lazy.compactMap { folder -> ActivityLine? in
            switch folder.activity {
            case .scanning(let files) where only == nil || transfers: .scanning(folder: folder.spaceName, files: files)
            case .checking(let files, _) where only == nil: .comparing(folder: folder.spaceName, files: files)
            default: nil
            }
        }.first
        note = busyNote ?? FolderNames(state.folders.filter(\.awaitsAnswers).map(\.spaceName)).map(ActivityLine.waitingForAnswer)
    }
}
