// SPDX-License-Identifier: Apache-2.0

import Foundation
import KSCrashRecording
import KoidriveFFI

enum CrashReports {
    static func install() {
        let configuration = KSCrashConfiguration()
        configuration.monitors = [.signal, .cppException, .nsException]
        configuration.enableMemoryIntrospection = false
        configuration.enableQueueNameSearch = false
        configuration.addConsoleLogToReport = false
        configuration.userInfoJSON = ["koidrive_version": KoidriveFFI.version()]
        try? KSCrash.shared.install(with: configuration)
    }

    static func forward(to session: SyncSession) {
        Task.detached(priority: .utility) {
            guard let store = KSCrash.shared.reportStore else { return }
            for id in store.reportIDs.map(\.int64Value) {
                if let data = store.reportData(for: id)?.value {
                    do {
                        try session.recordAppCrash(report: String(decoding: data, as: UTF8.self))
                    } catch {
                        continue
                    }
                }
                store.deleteReport(with: id)
            }
        }
    }
}
