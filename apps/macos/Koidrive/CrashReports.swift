// SPDX-License-Identifier: Apache-2.0

import Foundation
import KSCrashRecording
import KoidriveFFI
import KoidriveModel

enum CrashReports {
    static func install(helper: URL) {
        let configuration = KSCrashConfiguration()
        configuration.monitors = [.machException, .signal, .cppException, .nsException]
        configuration.enableMemoryIntrospection = false
        configuration.enableQueueNameSearch = false
        configuration.addConsoleLogToReport = false
        try? KSCrash.shared.install(with: configuration)
        KSCrash.shared.setUserInfo(KoidriveFFI.version(), forKey: "koidrive_version")
        stamp(KoidriveFFI.appCrashStamp(helper: helper.path))
    }

    @MainActor static func forward(from model: AppModel) {
        let session = model.session
        model.telemetryChanged = { _ in stamp(session.appCrashStamp()) }
        stamp(session.appCrashStamp())
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

    private static func stamp(_ stamp: AppCrashStamp) {
        KSCrash.shared.setUserInfo(stamp.reportsOn, forKey: "reports_on")
        KSCrash.shared.setUserInfo(stamp.generation, forKey: "reports_generation")
    }
}
