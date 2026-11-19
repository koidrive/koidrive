// SPDX-License-Identifier: Apache-2.0

import XCTest

final class UITestBundle: NSObject, XCTestObservation {
    override init() {
        super.init()
        XCTestObservationCenter.shared.addTestObserver(self)
    }

    func testBundleWillStart(_ testBundle: Bundle) {
        MainActor.assumeIsolated { _ = XCUIScreen.main.screenshot() }
    }
}
