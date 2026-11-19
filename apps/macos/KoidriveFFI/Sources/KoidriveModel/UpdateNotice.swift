// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import Foundation

public struct UpdateFailure: Equatable, Sendable {
    public let version: String
    public let reason: String
    public let logPath: String?
    public let fallbackCommand: String?

    /// Why the update failed, as one sentence: the first line of `reason`, which later lines only add detail or a command to.
    public var summary: String {
        let line = reason.split(whereSeparator: \.isNewline).first.map { $0.trimmingCharacters(in: .whitespaces) } ?? ""
        guard let last = line.last else { return "" }
        return ".!?".contains(last) ? line : line + "."
    }
}

public enum UpdateCheckFailure: Equatable, Sendable {
    case unreachable
    case refused
    case other(SessionError)
}

public struct FailedUpdateCheck: Equatable, Sendable {
    public let failure: UpdateCheckFailure
    let atMs: Int64
}

/// What the app says about updating Koidrive, from the daemon's last check and the person's last update.
public enum UpdateNotice: Equatable, Sendable {
    case checking
    case unchecked
    case checkFailed(UpdateCheckFailure)
    case offline
    case off
    case upToDate(checkedAtMs: Int64)
    case available(version: String, releaseURL: URL?)
    case updating(version: String)
    case updated(version: String)
    case failed(UpdateFailure)

    public var offersCheck: Bool {
        switch self {
        case .unchecked, .checkFailed, .offline, .off, .upToDate: true
        case .checking, .available, .updating, .updated, .failed: false
        }
    }
}

extension AppModel {
    /// The update line in Settings.
    public var updateNotice: UpdateNotice {
        if let installingVersion { return .updating(version: installingVersion) }
        if let updateFailure { return .failed(updateFailure) }
        if checkingForUpdates { return .checking }
        let newest = newestUpdate
        if case .available = newest { return Self.notice(newest) }
        if let failedUpdateCheck, failedUpdateCheck.atMs >= newest.checkedAtMs ?? 0 {
            return .checkFailed(failedUpdateCheck.failure)
        }
        if checkedUpdate == nil, state.status == .offline { return .offline }
        return Self.notice(newest)
    }

    /// The update notice in the menu bar popover, which shows only what the person can act on, just did or asked for.
    public var popoverUpdateNotice: UpdateNotice? {
        let notice = updateNotice
        switch notice {
        case .updating, .failed: return notice
        case .available where justUpdated == nil: return notice
        case _ where showsUpdateCheck: return notice
        default: return justUpdated.map { .updated(version: $0) }
        }
    }

    private var newestUpdate: UpdateStatus {
        guard let checkedUpdate, checkedUpdate.checkedAtMs ?? 0 >= state.update.checkedAtMs ?? 0 else { return state.update }
        return checkedUpdate
    }

    private static func notice(_ update: UpdateStatus) -> UpdateNotice {
        switch update {
        case .available(let version, let releaseUrl, _): .available(version: version, releaseURL: URL(string: releaseUrl))
        case .upToDate(let checkedAtMs): .upToDate(checkedAtMs: checkedAtMs)
        case .unknown: .unchecked
        case .off: .off
        }
    }

    /// Installs the available update. Returns the update when it left this app running, so the caller relaunches it.
    public func installUpdate() async -> UpdateResult? {
        guard case .available(let version, _, _) = newestUpdate, !updating else { return nil }
        updateFailure = nil
        installingVersion = version
        defer { installingVersion = nil }
        do {
            return try await update()
        } catch SessionError.UpdateUnavailable {
        } catch SessionError.UpdateFailed(let reason, let logPath, let fallbackCommand) {
            updateFailure = UpdateFailure(version: version, reason: reason, logPath: logPath, fallbackCommand: fallbackCommand)
        } catch {
            let reason = (error as? SessionError).map { "\($0)" } ?? error.localizedDescription
            updateFailure = UpdateFailure(version: version, reason: reason, logPath: nil, fallbackCommand: nil)
        }
        return nil
    }
}

extension UpdateStatus {
    var checkedAtMs: Int64? {
        switch self {
        case .available(_, _, let checkedAtMs), .upToDate(let checkedAtMs): checkedAtMs
        case .unknown, .off: nil
        }
    }
}
