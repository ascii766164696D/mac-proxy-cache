import SwiftUI

@main
struct MacProxyCacheApp: App {
    @StateObject private var proxyService = ProxyService()
    @StateObject private var statsPoller = StatsPoller()

    var body: some Scene {
        MenuBarExtra {
            MenuBarView(proxyService: proxyService, stats: statsPoller)
        } label: {
            Text(statsPoller.menuBarTitle)
                .monospacedDigit()
        }
        .menuBarExtraStyle(.menu)
    }
}
