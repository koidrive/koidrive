// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import Foundation

public struct UpdateFailure: Equatable, Sendable {
    public let version: String
    public let reason: String
    public let logPath: String?
    public let fallbackCommand: String?
}

/// What the app says about updating Koidrive, from the daemon's last check and the person's last update.
public enum UpdateNotice: Equatable, Sendable {
    case checking
    case offline
    case off
    case upToDate(checkedAtMs: Int64)
    case available(version: String, releaseURL: URL?)
    case updating(version: String)
    case updated(version: String)
    case failed(UpdateFailure)
}

extension AppModel {
    /// The update line in Settings.
    public var updateNotice: UpdateNotice {
        if updating, case .available(let version, _, _) = state.update { return .updating(version: version) }
        if let updateFailure { return .failed(updateFailure) }
        switch state.update {
        case .available(let version, let releaseUrl, _):
            return .available(version: version, releaseURL: URL(string: releaseUrl))
        case _ where state.status == .offline: return .offline
        case .off: return .off
        case .unknown: return .checking
        case .upToDate(let checkedAtMs): return .upToDate(checkedAtMs: checkedAtMs)
        }
    }

    /// The update notice in the menu bar popover, which shows only what the person can act on or just did.
    public var popoverUpdateNotice: UpdateNotice? {
        let notice = updateNotice
        switch notice {
        case .updating, .failed: return notice
        case .available where justUpdated == nil: return notice
        default: return justUpdated.map { .updated(version: $0) }
        }
    }

    /// Installs the available update. Returns the update when it left this app running, so the caller relaunches it.
    public func installUpdate() async -> UpdateResult? {
        guard case .available(let version, _, _) = state.update, !updating else { return nil }
        updateFailure = nil
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

