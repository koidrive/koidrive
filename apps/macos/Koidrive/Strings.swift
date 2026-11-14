// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import KoidriveModel
import Foundation

extension SyncStatus {
    var title: String {
        switch self {
        case .stopped: String(localized: "Sync is stopped")
        case .needsSetup: String(localized: "Set up Koidrive")
        case .connecting: String(localized: "Connecting…")
        case .syncing: String(localized: "Syncing…")
        case .idle: String(localized: "Up to date")
        case .paused: String(localized: "Sync is paused")
        case .offline: String(localized: "Can't reach the server")
        case .halted: String(localized: "A folder stopped syncing")
        }
    }
}

extension FolderState {
    var title: String {
        switch self {
        case .waiting: String(localized: "Waiting")
        case .syncing: String(localized: "Syncing")
        case .upToDate: String(localized: "Up to date")
        case .halted: String(localized: "Stopped")
        case .recovering: String(localized: "Recovering")
        case .readOnly: String(localized: "Download only")
        }
    }
}

extension Question {
    var title: String {
        switch kind {
        case .massDelete: String(localized: "Delete many files?")
        case .ambiguousDisappearance: String(localized: "Files disappeared")
        case .recovery(_, let group): group.title
        }
    }

    var listTitle: String {
        switch kind {
        case .massDelete: String(localized: "Delete many files")
        default: title
        }
    }

    var explanation: String {
        switch kind {
        case .massDelete(let deletions, let affectedFiles, let spaceFiles, _):
            String(localized: "\(deletionCount(deletions)) would remove \(affectedFiles.formatted()) of \(fileCount(spaceFiles)).")
        case .ambiguousDisappearance:
            String(localized: "These went missing while Koidrive wasn't running. Other devices keep them until you decide.")
        case .recovery(_, let group):
            group.explanation
        }
    }

    func title(for answer: Answer) -> String {
        switch (answer, kind) {
        case (.delete, .recovery): String(localized: "Delete All on Server")
        case (.delete, _): String(localized: "Delete All Everywhere")
        case (.restore, .recovery): String(localized: "Download All Again")
        case (.restore, _): String(localized: "Restore All Here")
        case (.keep, _): String(localized: "Put All Back on Server")
        case (.discard, _): String(localized: "Move All to Local Trash")
        case (.publish, .recovery(_, .changedOnThisDevice)): String(localized: "Upload This Mac's Versions")
        case (.publish, _): String(localized: "Upload All")
        case (.repeat, _): String(localized: "Repeat All Requests")
        case (.cancel, _): String(localized: "Cancel All Requests")
        }
    }

    var counts: String {
        String(localized: "\(itemCount(items.count)), \(fileCount(affectedFiles))")
    }

    var scope: String {
        String(localized: "Applies to \(itemCount(items.count)), \(fileCount(affectedFiles))")
    }

    func confirmation(for answer: Answer) -> String {
        switch (answer, kind) {
        case (.delete, .recovery): String(localized: "Delete \(itemCount(items.count)) on the server?")
        case (.delete, _): items.count == 1
            ? String(localized: "Delete 1 item everywhere?")
            : String(localized: "Delete all \(itemCount(items.count)) everywhere?")
        default: String(localized: "\(title(for: answer))?")
        }
    }

    func confirmationMessage(for answer: Answer) -> String {
        switch (answer, kind) {
        case (.delete, .recovery): String(localized: "This deletes \(fileCount(affectedFiles)) on the server and every device. The server keeps them in Deleted Files.")
        case (.delete, _): String(localized: "This deletes \(fileCount(affectedFiles)) on every device. The server keeps them in Deleted Files.")
        default: String(localized: "This applies to \(itemCount(items.count)), \(fileCount(affectedFiles)).")
        }
    }

    var itemsHeading: String {
        switch kind {
        case .massDelete: String(localized: "Held deletions")
        default: String(localized: "Held files")
        }
    }

    var symbolName: String {
        switch kind {
        case .massDelete: "trash"
        case .ambiguousDisappearance: "questionmark.circle"
        case .recovery: "clock"
        }
    }
}

extension RecoveryGroup {
    var title: String {
        switch self {
        case .createdAfterRestorePoint: String(localized: "Created after the restore point")
        case .changedOnThisDevice: String(localized: "Changed on this Mac")
        case .createdOnThisDevice: String(localized: "Created on this Mac")
        case .deletedOnThisDevice: String(localized: "Deleted on this Mac")
        case .requestedBeforeReset: String(localized: "Requested before the reset")
        }
    }

    var explanation: String {
        switch self {
        case .createdAfterRestorePoint:
            String(localized: "Removed by the restore. This Mac still has them.")
        case .changedOnThisDevice:
            String(localized: "Edited or moved on this Mac since the last sync. Upload them, or trash them here and keep the server's.")
        case .createdOnThisDevice:
            String(localized: "Only this Mac has these files. Upload them or move them to the local trash.")
        case .deletedOnThisDevice:
            String(localized: "Deleted on this Mac but still on the server. Delete them there too, or download them again.")
        case .requestedBeforeReset:
            String(localized: "The server lost these restore requests.")
        }
    }
}

extension RecoveryCause {
    var text: String {
        if trigger == .spaceRewound, let ms = restorePointMs {
            let date = Date(milliseconds: ms).formatted(.dateTime.day().month(.abbreviated).year().hour().minute())
            return String(localized: "Restored on the server to \(date).")
        }
        let cause = switch trigger {
        case .spaceRewound: String(localized: "This folder was restored on the server.")
        case .storeRolledBack: String(localized: "This Mac's sync state is older than the server's, for example after a backup restore.")
        case .storeOlderThanDisk: String(localized: "This Mac's sync state is older than the folder's files.")
        case .spaceImported: String(localized: "This folder is linked to an imported space.")
        case .noSyncState: String(localized: "The folder had files when linked, and Koidrive couldn't match them all.")
        case .unknown: String(localized: "Some files need your decision before Koidrive can sync them.")
        }
        guard let ms = restorePointMs else { return cause }
        let date = Date(milliseconds: ms).formatted(.dateTime.day().month(.abbreviated).year().hour().minute())
        return cause + " " + String(localized: "Restored to \(date).")
    }

    var shortText: String {
        guard let ms = restorePointMs else {
            return switch trigger {
            case .spaceRewound: String(localized: "Restored on the server")
            case .storeRolledBack: String(localized: "Sync state from a backup")
            case .storeOlderThanDisk: String(localized: "Sync state older than the folder")
            case .spaceImported: String(localized: "Imported space")
            case .noSyncState: String(localized: "Linked with files")
            case .unknown: String(localized: "Recovery needs a decision")
            }
        }
        return String(localized: "Restored to \(Date(milliseconds: ms).formatted(.dateTime.day().month(.abbreviated).hour().minute()))")
    }
}

extension Notice {
    func title(spaceName: String?) -> String {
        switch kind {
        case .recovery:
            spaceName.map { String(localized: "\($0) was recovered") } ?? String(localized: "Folder recovered")
        case .remoteMassDelete:
            spaceName.map { String(localized: "Many files deleted from \($0) on another device") }
                ?? String(localized: "Many files deleted on another device")
        }
    }

    var text: String {
        switch kind {
        case .recovery(let cause, let heldFiles):
            return heldFiles == 0
                ? String(localized: "\(cause.shortText). Nothing needs your answer.")
                : String(localized: "\(cause.shortText). Answer in Needs You.")
        case .remoteMassDelete(_, let affectedFiles, let spaceFiles, _):
            return String(localized: "\(affectedFiles.formatted()) of \(fileCount(spaceFiles)) removed. Copies are in the folder's local trash.")
        }
    }
}

extension ItemDetail {
    var text: String? {
        switch self {
        case .plain: nil
        case .edited: String(localized: "Edited")
        case .moved(let from): String(localized: "Moved from \(from)")
        case .editedAndMoved(let from): String(localized: "Edited and moved from \(from)")
        case .restoreFromTrash: String(localized: "Restore from Deleted Files")
        case .restoreVersion(let ms):
            String(localized: "Restore version from \(Date(milliseconds: ms).formatted(date: .abbreviated, time: .shortened))")
        }
    }
}

extension ProblemKind {
    var title: String {
        switch self {
        case .folderMissing: String(localized: "The folder is missing")
        case .folderNotAFolder: String(localized: "The linked path isn't a folder")
        case .folderUnreadable: String(localized: "The folder can't be read")
        case .folderOnOtherVolume: String(localized: "Another disk is at the folder's path")
        case .markerMissing: String(localized: "The folder's .koidrive marker is missing")
        case .markerUnreadable: String(localized: "The folder's .koidrive marker can't be read")
        case .markerOfOtherSpace: String(localized: "The folder belongs to another space")
        case .diskOlderThanSyncState: String(localized: "The folder was restored from an older backup")
        case .unsupportedFileSystem: String(localized: "The folder's file system isn't supported")
        case .spaceFormatTooNew: String(localized: "This space needs a newer Koidrive")
        case .fileUnreadable: String(localized: "A file can't be read")
        case .fileNotWritable: String(localized: "A file can't be written")
        case .fileBusy: String(localized: "A file keeps changing")
        case .nameNotAllowed: String(localized: "A name isn't allowed on this Mac")
        case .nameTaken: String(localized: "A name is taken by an ignored item")
        case .changeRejected: String(localized: "The server rejected a change")
        case .ignoreRuleInvalid: String(localized: "An ignore rule can't be read")
        case .signInRequired: String(localized: "This Mac was removed")
        case .stateNotWritable: String(localized: "Koidrive can't save its sync state")
        case .folderHalted: String(localized: "Sync for this folder stopped")
        case .conflict: String(localized: "This file has a conflict copy")
        }
    }

    var advice: String {
        switch self {
        case .folderMissing: String(localized: "Connect its disk, put it back, or unlink it.")
        case .folderNotAFolder: String(localized: "Put the folder back or unlink it.")
        case .folderUnreadable(let reason): reason
        case .folderOnOtherVolume: String(localized: "Connect the original disk.")
        case .markerMissing: String(localized: "Put .koidrive back or unlink the folder.")
        case .markerUnreadable(let reason): reason
        case .markerOfOtherSpace: String(localized: "Unlink it and link the right folder.")
        case .diskOlderThanSyncState: String(localized: "Sync stopped so older files aren't uploaded as edits.")
        case .unsupportedFileSystem: String(localized: "Move the folder to your Mac's disk.")
        case .spaceFormatTooNew: String(localized: "Update Koidrive to upload again.")
        case .fileUnreadable(let reason): reason
        case .fileNotWritable(let reason): reason
        case .fileBusy: String(localized: "It changes too often to upload. If it's a log or database, add it to .koiignore.")
        case .nameNotAllowed: String(localized: "Rename the file on another device.")
        case .nameTaken: String(localized: "Rename or move the item, such as a symbolic link, that has its name.")
        case .changeRejected(let reason): reason
        case .ignoreRuleInvalid(let line): String(localized: "Fix or remove line \(line) of .koiignore.")
        case .signInRequired:
            String(localized: "Another device removed this Mac, or its credential stopped working. Sign out in Account settings, then sign in with a pairing code from another device.")
        case .stateNotWritable(let reason): String(localized: "Free up disk space. \(reason)")
        case .folderHalted(let reason): reason
        case .conflict(let resolveCommand):
            String(localized: "Both files are kept and continue syncing. Run \(resolveCommand) in Terminal to review the files and choose which to keep.")
        }
    }
}

extension RecentChange {
    var summary: String {
        let way = direction == .uploaded ? String(localized: "on this Mac") : String(localized: "on another device")
        return switch change {
        case .added: String(localized: "Added \(way)")
        case .edited: String(localized: "Edited \(way)")
        case .moved(let from): String(localized: "Moved from \(from) \(way)")
        case .deleted: String(localized: "Deleted \(way)")
        case .conflictCopy(let original): String(localized: "Conflicting copy of \(original)")
        }
    }
}

extension SessionError {
    var message: String {
        switch self {
        case .InvalidServerUrl: String(localized: "Enter an address starting with https://.")
        case .ServerUnreachable: String(localized: "Can't reach the server.")
        case .ServerIncompatible(let version): String(localized: "Update Koidrive to use this server (version \(version)).")
        case .NoServer: String(localized: "Connect to a server first.")
        case .AlreadyClaimed: String(localized: "This server has an owner. Sign in with a pairing code.")
        case .NotClaimed: String(localized: "This server has no owner yet. Claim it with its claim code.")
        case .InvalidCode: String(localized: "Wrong or expired code.")
        case .TooManyAttempts: String(localized: "Too many wrong codes. Try again in a few minutes.")
        case .CodeForOtherServer(let host, let server): String(localized: "This code is for another server (\(host)), not \(server).")
        case .EmptyName: String(localized: "Enter a name.")
        case .NotSignedIn: String(localized: "This Mac isn't signed in.")
        case .LastDevice: String(localized: "This is the last signed-in device.")
        case .NotRevoked(let reason): String(localized: "The server didn't confirm the removal: \(reason)")
        case .ThisDevice: String(localized: "To remove this Mac, sign out.")
        case .DeviceNotFound: String(localized: "This device was already removed.")
        case .SpaceNotFound: String(localized: "This space no longer exists.")
        case .FolderNotFound: String(localized: "Choose an existing folder.")
        case .SpaceAlreadyLinked: String(localized: "This space is already linked on this Mac.")
        case .FolderOverlaps(let other): String(localized: "This folder overlaps the linked folder \(other).")
        case .FolderNotLinked: String(localized: "This space isn't linked on this Mac.")
        case .QuestionNotFound: String(localized: "This question was answered or changed.")
        case .AnswerNotAllowed: String(localized: "This question doesn't take that answer.")
        case .NoticeNotFound: String(localized: "Already dismissed.")
        case .NotSaved(let reason): String(localized: "Change not saved: \(reason)")
        case .NotAFolder: String(localized: "Choose a folder, not a file.")
        case .FolderInUse: String(localized: "Another Koidrive process is syncing this folder.")
        case .FolderOfOtherDevice: String(localized: "Another device syncs this folder. Unlink it there first.")
        case .MarkerOfOtherSpace: String(localized: "This folder belongs to another space. Unlink it there first.")
        case .UnsupportedFileSystem: String(localized: "Choose a folder on your Mac's disk. This file system isn't supported.")
        case .PathNotInLinkedFolder(let path): String(localized: "\(path) isn't in a linked folder.")
        case .PathIsLinkedFolder: String(localized: "Choose a file or folder inside the linked folder.")
        case .NothingAtPath: String(localized: "This item isn't on the server.")
        case .PathIsFolder: String(localized: "Only files have versions.")
        case .NotDeleted: String(localized: "This item isn't deleted.")
        case .DeletedItemNotFound: String(localized: "This item is no longer in the trash.")
        case .VersionNotFound: String(localized: "The server no longer has this version.")
        case .VersionIsCurrent: String(localized: "This is already the current version.")
        case .NotOnServer: String(localized: "This item hasn't synced yet. Try again once it's uploaded.")
        case .PasswordLength: String(localized: "Use a password of 8 to 256 characters.")
        case .ShareLinkNotFound: String(localized: "This link no longer exists.")
        case .ShareLinkLimit: String(localized: "This space has as many active links as it can hold. Revoke one to make a new link.")
        case .Failed(let reason): reason
        }
    }

    var detail: String? {
        switch self {
        case .ServerUnreachable(let reason), .NotRevoked(let reason), .NotSaved(let reason): reason
        default: nil
        }
    }
}

func itemCount(_ count: Int) -> String {
    String(localized: "\(count) items")
}

func deletionCount(_ count: UInt64) -> String {
    String(localized: "\(count) deletions")
}

func fileCount(_ count: UInt64) -> String {
    String(localized: "\(count) files")
}

extension Date {
    init(milliseconds: Int64) {
        self.init(timeIntervalSince1970: TimeInterval(milliseconds) / 1000)
    }
}

func restoreCount(_ count: Int) -> String {
    String(localized: "\(count) restores")
}

func versionCount(_ count: Int) -> String {
    String(localized: "\(count) versions")
}

func byteSize(_ bytes: UInt64) -> String {
    Int64(clamping: bytes).formatted(.byteCount(style: .file))
}

func possessive(_ name: String) -> String {
    name.hasSuffix("s") ? name + "’" : name + "’s"
}

func restoreGoesOut(with folder: String) -> String {
    String(localized: "Restore requested. It goes out with \(possessive(folder)) next sync.")
}

extension Day {
    var title: String {
        switch self {
        case .today: String(localized: "Today")
        case .yesterday: String(localized: "Yesterday")
        case .earlier: String(localized: "Earlier")
        }
    }

    func time(of date: Date) -> String {
        switch self {
        case .today, .yesterday: date.formatted(.dateTime.hour().minute())
        case .earlier: date.formatted(.dateTime.day().month(.abbreviated).hour().minute())
        }
    }
}

extension Optional where Wrapped == DeviceLabel {
    var title: String {
        switch self {
        case .thisMac: String(localized: "This Mac")
        case .named(let name): name
        case nil: String(localized: "Another device")
        }
    }
}

extension Deletion {
    var text: String {
        let time = date.formatted(.dateTime.hour().minute())
        let day = date.formatted(.dateTime.day().month(.abbreviated))
        switch (self.day, device) {
        case (.today, nil): return String(localized: "Deleted today at \(time)")
        case (.yesterday, nil): return String(localized: "Deleted yesterday at \(time)")
        case (.earlier, nil): return String(localized: "Deleted \(day) at \(time)")
        case (.today, .thisMac): return String(localized: "Deleted today at \(time) on this Mac")
        case (.yesterday, .thisMac): return String(localized: "Deleted yesterday at \(time) on this Mac")
        case (.earlier, .thisMac): return String(localized: "Deleted \(day) at \(time) on this Mac")
        case (.today, .named(let name)): return String(localized: "Deleted today at \(time) on \(name)")
        case (.yesterday, .named(let name)): return String(localized: "Deleted yesterday at \(time) on \(name)")
        case (.earlier, .named(let name)): return String(localized: "Deleted \(day) at \(time) on \(name)")
        }
    }
}

extension RestoreSubject {
    var text: String {
        switch self {
        case .deletedItem:
            return String(localized: "Deleted item")
        case .earlierVersion:
            return String(localized: "Earlier version")
        case .version(let date, let day):
            let time = date.formatted(.dateTime.hour().minute())
            switch day {
            case .today: return String(localized: "Version from today at \(time)")
            case .yesterday: return String(localized: "Version from yesterday at \(time)")
            case .earlier: return String(localized: "Version from \(date.formatted(.dateTime.day().month(.abbreviated))) at \(time)")
            }
        }
    }
}

extension RestoreState {
    func text(folder: String) -> String {
        switch self {
        case .pending(heldByProblem: false): String(localized: "Waiting for \(folder) to sync")
        case .pending(heldByProblem: true): String(localized: "Waiting until a refused change to it is fixed")
        case .restored: String(localized: "Restored")
        case .restoredAs(let name): String(localized: "Restored as \(name). The name was taken.")
        case .restoredAsCopy: String(localized: "Restored as a copy. Another device changed it.")
        case .refused(let reason): String(localized: "Refused. \(reason.text)")
        case .tooLarge: String(localized: "Too many items for one restore.")
        }
    }

    var versionRefusal: String {
        switch self {
        case .refused(.gone): String(localized: "Couldn’t restore. The server no longer has this version.")
        case .refused(let reason): String(localized: "Couldn’t restore. \(reason.text)")
        default: String(localized: "Couldn’t restore. Too many items for one restore.")
        }
    }
}

extension RestoreRefusal {
    var text: String {
        switch self {
        case .gone: String(localized: "The server no longer has it.")
        case .notAFile: String(localized: "Only a file has versions.")
        case .invalidName: String(localized: "Its name isn't allowed.")
        case .destinationNotAFolder: String(localized: "The place it goes back to is a file now.")
        case .destinationInsideItself: String(localized: "The folder it goes back to is inside it.")
        case .root: String(localized: "The linked folder itself can't be restored.")
        case .readOnly: String(localized: "The server takes no changes from this Mac.")
        case .other(let reason): reason
        }
    }
}
