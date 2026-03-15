import SwiftUI
import AppKit

struct MenuBarView: View {
    @ObservedObject var proxyService: ProxyService
    @ObservedObject var stats: StatsPoller
    var openMonitor: () -> Void

    var body: some View {
        // Status
        Button {
            if proxyService.isRunning {
                proxyService.stop()
                stats.stopPolling()
            } else {
                proxyService.start()
                stats.startPolling()
            }
        } label: {
            Text(proxyService.isRunning ? "● Proxy Active (port 9090)" : "○ Proxy Stopped")
        }

        if proxyService.isRunning {
            Divider()

            Text("Requests: \(stats.requests)  —  Hit rate: \(stats.hitRateFormatted)")
            Text("Bandwidth saved: \(stats.bandwidthSaved)")
            Text("Cache: \(stats.activeSizeHuman) / \(stats.maxCacheSizeHuman)")

            if stats.imageCount > 0 || stats.videoCount > 0 || stats.audioCount > 0 {
                Divider()
                let parts = [
                    stats.imageCount > 0 ? "\(stats.imageCount) images" : nil,
                    stats.videoCount > 0 ? "\(stats.videoCount) videos" : nil,
                    stats.audioCount > 0 ? "\(stats.audioCount) audio" : nil,
                ].compactMap { $0 }.joined(separator: "  ·  ")
                Text(parts)
            }
        }

        Divider()

        // Toggles
        Button {
            toggleSystemProxy(enabled: !stats.systemProxyEnabled)
        } label: {
            Text(stats.systemProxyEnabled ? "✓ System Proxy" : "   System Proxy")
        }

        if proxyService.isRunning {
            Button {
                toggleBypass()
            } label: {
                Text(stats.bypassEnabled ? "✓ Bypass Cache" : "   Bypass Cache")
            }
        }

        Divider()

        // Cache Size
        if proxyService.isRunning {
            Menu("Max Cache Size: \(stats.maxCacheSizeHuman)") {
                Button("100 MB") { setCacheSize(104_857_600) }
                Button("500 MB") { setCacheSize(524_288_000) }
                Button("1 GB") { setCacheSize(1_073_741_824) }
                Button("2 GB") { setCacheSize(2_147_483_648) }
                Button("5 GB") { setCacheSize(5_368_709_120) }
                Button("10 GB") { setCacheSize(10_737_418_240) }
                Button("Unlimited (100 GB)") { setCacheSize(107_374_182_400) }
            }

            Divider()
        }

        // Actions
        Button("Open Monitor...") {
            openMonitor()
        }

        Button("Browse Cache in Finder...") {
            let cacheDir = "\(NSHomeDirectory())/mac-proxy-cache/cache"
            NSWorkspace.shared.open(URL(fileURLWithPath: cacheDir))
        }

        Button("Clear Cache...") {
            clearCache()
        }

        Divider()

        if let error = proxyService.lastError {
            Text(error)
            Divider()
        }

        Button("Quit") {
            proxyService.stop()
            NSApplication.shared.terminate(nil)
        }
        .keyboardShortcut("q")
    }

    private func toggleSystemProxy(enabled: Bool) {
        Task {
            await postJSON("system-proxy", body: "{\"enabled\": \(enabled)}")
        }
    }

    private func setCacheSize(_ bytes: UInt64) {
        Task {
            await postJSON("config", body: "{\"max_cache_size\": \(bytes)}")
        }
    }

    private func toggleBypass() {
        Task { await postJSON("bypass", body: nil) }
    }

    private func clearCache() {
        Task { await postJSON("cache/clear", body: nil) }
    }

    private func postJSON(_ path: String, body: String?) async {
        guard let url = URL(string: "http://127.0.0.1:9091/api/\(path)") else { return }
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        if let body = body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = body.data(using: .utf8)
        }
        do {
            let (_, response) = try await URLSession.shared.data(for: request)
            if let http = response as? HTTPURLResponse, http.statusCode != 200 {
                print("[MenuBar] POST /api/\(path) returned \(http.statusCode)")
            }
        } catch {
            print("[MenuBar] POST /api/\(path) failed: \(error)")
        }
    }
}
