import SwiftUI

@main
struct MacProxyCacheApp: App {
    @StateObject private var proxyService = ProxyService()
    @StateObject private var statsPoller = StatsPoller()
    @Environment(\.openWindow) private var openWindow

    var body: some Scene {
        MenuBarExtra {
            MenuBarView(proxyService: proxyService, stats: statsPoller, openMonitor: {
                NSApp.setActivationPolicy(.regular)
                openWindow(id: "monitor")
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) {
                    NSApp.activate(ignoringOtherApps: true)
                    NSApp.windows.first { $0.title.contains("Monitor") }?.makeKeyAndOrderFront(nil)
                }
            })
        } label: {
            Text(statsPoller.menuBarTitle)
                .monospacedDigit()
        }
        .menuBarExtraStyle(.menu)

        Window("Mac Proxy Cache — Monitor", id: "monitor") {
            MonitorWindow()
        }
        .defaultSize(width: 1000, height: 600)
    }
}
