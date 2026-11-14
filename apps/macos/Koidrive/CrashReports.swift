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
        KSCrash.shared.setUserInfo(KoidriveFFI.telemetrySetting(helper: helper.path).enabled, forKey: "reports_on")
    }

    @MainActor static func forward(from model: AppModel) {
        model.telemetryChanged = { KSCrash.shared.setUserInfo($0.enabled, forKey: "reports_on") }
        KSCrash.shared.setUserInfo(model.telemetry.enabled, forKey: "reports_on")
        let session = model.session
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
