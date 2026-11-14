// SPDX-License-Identifier: Apache-2.0

import Foundation
import KSCrashRecording
import KoidriveFFI

enum CrashReports {
    static func start(forwardingTo session: SyncSession) {
        let configuration = KSCrashConfiguration()
        configuration.monitors = [.machException, .signal, .cppException, .nsException]
        configuration.enableMemoryIntrospection = false
        configuration.enableQueueNameSearch = false
        configuration.addConsoleLogToReport = false
        try? KSCrash.shared.install(with: configuration)
        guard let store = KSCrash.shared.reportStore, let executable = Bundle.main.executablePath else { return }
        for id in store.reportIDs.map(\.int64Value) {
            if let data = store.reportData(for: id)?.value {
                session.recordAppCrash(report: String(decoding: data, as: UTF8.self), executable: executable)
            }
            store.deleteReport(with: id)
        }
    }
}
