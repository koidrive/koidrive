// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI

/// What a history window shows: a load in progress, its answer, or why it failed.
public enum LoadState<Value> {
    case loading
    case loaded(Value)
    case failed(SessionError)
}

extension LoadState: Equatable where Value: Equatable {}
extension LoadState: Sendable where Value: Sendable {}

/// The day group of a time in the history windows.
public enum Day: Hashable, Sendable {
    case today
    case yesterday
    case earlier
}

/// The clock and calendar the history windows group days by.
public struct HistoryClock: Sendable {
    public var now: @Sendable () -> Date
    public var calendar: Calendar

    public init(now: @escaping @Sendable () -> Date = { Date() }, calendar: Calendar = .autoupdatingCurrent) {
        self.now = now
        self.calendar = calendar
    }

    public func day(of date: Date) -> Day {
        let today = calendar.startOfDay(for: now())
        let day = calendar.startOfDay(for: date)
        if day >= today { return .today }
        if let yesterday = calendar.date(byAdding: .day, value: -1, to: today), day >= yesterday { return .yesterday }
        return .earlier
    }
}

/// The device a deletion or a version came from.
public enum DeviceLabel: Equatable, Sendable {
    case thisMac
    case named(String)
}

/// The scope of the Deleted Files window: every linked folder, or one.
public enum FolderScope: Hashable, Sendable {
    case all
    case folder(spaceID: String)
}

extension AppModel {
    /// This Mac for this device's ID, else the device's name; nil when the name is unknown.
    public func deviceLabel(id: String?, name: String?) -> DeviceLabel? {
        if let id, id == state.account?.deviceId { return .thisMac }
        return name.map(DeviceLabel.named)
    }

    /// The linked folder that holds the real item `url` leads to, or is that item, once its symbolic links and Finder alias resolve.
    public func folder(containing url: URL) -> LinkedFolder? {
        location(of: url)?.folder
    }

    func location(of url: URL) -> (folder: LinkedFolder, below: [String])? {
        location(ofRealPath: url.realPath)
    }

    func location(ofRealPath path: String?) -> (folder: LinkedFolder, below: [String])? {
        guard let path else { return nil }
        return state.folders.flatMap { folder in
            let stored = plainPath(folder.url)
            return Set([stored, realRoot(stored)].compactMap { $0 }).compactMap { root in
                if path == root { return (folder: folder, root: root, below: [String]()) }
                guard path.hasPrefix(root + "/") else { return nil }
                return (folder: folder, root: root, below: path.dropFirst(root.count + 1).split(separator: "/").map(String.init))
            }
        }
        .max { $0.root.count < $1.root.count }
        .map { ($0.folder, $0.below) }
    }
}

extension URL {
    var realPath: String? {
        var path = plainPath(self)
        for _ in 0..<32 {
            let (existing, rest) = deepestExistingPart(of: path)
            if let name = rest.first, let destination = try? FileManager.default.destinationOfSymbolicLink(atPath: joined(existing, [name])) {
                path = joined(destination.hasPrefix("/") ? destination : joined(existing, [destination]), Array(rest.dropFirst()))
                continue
            }
            let item = URL(filePath: existing)
            guard (try? item.resourceValues(forKeys: [.isAliasFileKey]))?.isAliasFile == true,
                  let target = try? URL(resolvingAliasFileAt: item, options: .withoutUI) else {
                return joined(existing, rest)
            }
            path = joined(plainPath(target), rest)
        }
        return nil
    }
}

private func deepestExistingPart(of path: String) -> (real: String, rest: [String]) {
    var existing = path
    var rest: [String] = []
    while existing != "/" {
        if let resolved = realpath(existing, nil) {
            defer { free(resolved) }
            return (String(cString: resolved), rest)
        }
        rest.insert((existing as NSString).lastPathComponent, at: 0)
        existing = (existing as NSString).deletingLastPathComponent
    }
    return ("/", rest)
}

private func joined(_ path: String, _ names: [String]) -> String {
    ([path == "/" ? "" : path] + names).joined(separator: "/")
}

func plainPath(_ url: URL) -> String {
    var path = url.standardizedFileURL.path(percentEncoded: false)
    while path.count > 1, path.hasSuffix("/") { path.removeLast() }
    return path
}

/// A window a debug build opens at launch, for tests.
public enum LaunchWindow: Equatable, Sendable {
    case deletedFiles
    /// The Version History of the file at `path`, which starts with its linked folder's name.
    case versionHistory(path: String)
    /// The Share Link window of the item at `path`, which starts with its linked folder's name.
    case shareLink(path: String)
}

extension AppModel {
    /// The window that the `OpenDeletedFiles`, `OpenVersionHistory` or `OpenShareLink` preference names.
    /// Only a debug build reads them.
    public static func launchWindow(in defaults: UserDefaults) -> LaunchWindow? {
        #if DEBUG
        if let path = defaults.string(forKey: "OpenVersionHistory") { return .versionHistory(path: path) }
        if let path = defaults.string(forKey: "OpenShareLink") { return .shareLink(path: path) }
        if defaults.bool(forKey: "OpenDeletedFiles") { return .deletedFiles }
        #endif
        return nil
    }

    /// True when the `OpenSharedLinks` preference opens the Shared Links window, beside any launch window.
    /// Only a debug build reads it.
    public static func launchesSharedLinks(in defaults: UserDefaults) -> Bool {
        #if DEBUG
        return defaults.bool(forKey: "OpenSharedLinks")
        #else
        return false
        #endif
    }

    /// The file at `path` in a linked folder, where `path` starts with the folder's name.
    public func url(ofDisplayPath path: String) -> URL? {
        state.folders.lazy.compactMap { folder -> URL? in
            guard path.hasPrefix(folder.spaceName + "/") else { return nil }
            return folder.url.appending(path: String(path.dropFirst(folder.spaceName.count + 1)))
        }.first
    }
}

extension LinkedFolder {
    public var url: URL { URL(filePath: path, directoryHint: .isDirectory) }
}

extension Date {
    init(milliseconds: Int64) {
        self.init(timeIntervalSince1970: TimeInterval(milliseconds) / 1000)
    }
}

func sessionError(_ error: any Error) -> SessionError {
    error as? SessionError ?? .Failed(reason: error.localizedDescription)
}
