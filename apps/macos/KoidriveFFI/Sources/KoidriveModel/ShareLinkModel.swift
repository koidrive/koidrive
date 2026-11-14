// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import Observation

/// The Share Link window of one file or folder: how long the link works, its password, and the link once it's made.
@MainActor
@Observable
public final class ShareLinkModel {
    public enum Step: Equatable, Sendable {
        case choosing
        case checking
        /// The only place the link's URL is kept. It goes when the window closes.
        case created(NewShareLink)
        /// The server doesn't have the item yet.
        case notSyncedYet
        case failed(SessionError)
    }

    public enum PasswordIssue: Equatable, Sendable {
        case tooShort
        case tooLong
    }

    /// Posted with the `AppModel` as its object when a link is made, with the new `ShareLink` under `linkKey`.
    public static let linkCreated = Notification.Name("ShareLinkModel.linkCreated")
    public static let linkKey = "link"

    /// The password lengths the server takes, in Unicode scalars.
    public static let passwordLengths = 8...256

    public let item: URL
    public var expiry: ShareExpiry = .week
    public var requiresPassword = false {
        didSet { passwordIssue = nil }
    }
    /// The password, kept on one line as `koi` takes it.
    public var password = "" {
        didSet {
            if password.contains(where: \.isNewline) { password.removeAll(where: \.isNewline) }
            passwordIssue = nil
        }
    }
    public private(set) var passwordIssue: PasswordIssue?
    public private(set) var step: Step = .choosing
    private let app: AppModel

    public init(app: AppModel, item: URL) {
        self.app = app
        self.item = item
    }

    #if DEBUG
    /// A window that shows `step` as it opens, for the snapshot harness.
    public convenience init(app: AppModel, item: URL, showing step: Step) {
        self.init(app: app, item: item)
        self.step = step
    }
    #endif

    public var name: String { item.lastPathComponent }

    public var isFolder: Bool {
        (try? item.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) ?? item.hasDirectoryPath
    }

    /// The linked folder that holds the item.
    public var folder: LinkedFolder? { app.folder(containing: item) }

    /// True while the session can't reach the server and no link was made.
    public var isOffline: Bool { app.state.status == .offline && link == nil }

    public var link: NewShareLink? {
        if case .created(let link) = step { link } else { nil }
    }

    /// True while the options can change and a link can be asked for.
    public var canCreate: Bool {
        switch step {
        case .choosing, .failed: !isOffline
        case .checking, .created, .notSyncedYet: false
        }
    }

    /// Asks the server for the link, after checking the password's length.
    public func create() async {
        guard canCreate else { return }
        let password = requiresPassword ? password : nil
        if let length = password?.unicodeScalars.count, !Self.passwordLengths.contains(length) {
            passwordIssue = length < Self.passwordLengths.lowerBound ? .tooShort : .tooLong
            return
        }
        step = .checking
        do {
            let made = try await app.createShareLink(for: item, expiry: expiry, password: password)
            step = .created(made)
            NotificationCenter.default.post(name: Self.linkCreated, object: app, userInfo: [Self.linkKey: made.link])
        } catch {
            let error = sessionError(error)
            step = error == .NotOnServer ? .notSyncedYet : .failed(error)
        }
    }

    /// Catches up after the session's status changed: an item that wasn't synced yet can be shared again.
    public func sessionStatusChanged() {
        if step == .notSyncedYet { step = .choosing }
    }
}
