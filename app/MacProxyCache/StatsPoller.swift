import Foundation
import Combine

/// Polls the dashboard API for stats and publishes updates.
@MainActor
class StatsPoller: ObservableObject {
    @Published var requests: Int = 0
    @Published var cacheHits: Int = 0
    @Published var cacheMisses: Int = 0
    @Published var bandwidthSaved: String = "0 B"
    @Published var hitRate: Double = 0.0
    @Published var bypassEnabled: Bool = false
    @Published var activeEntries: Int = 0
    @Published var staleEntries: Int = 0
    @Published var activeSizeHuman: String = "0 B"
    @Published var totalSizeHuman: String = "0 B"
    @Published var imageCount: Int = 0
    @Published var videoCount: Int = 0
    @Published var audioCount: Int = 0
    @Published var maxCacheSizeHuman: String = "1.0 GB"
    @Published var systemProxyEnabled: Bool = false
    @Published var menuBarTitle: String = "⇅"

    private var timer: Timer?
    private let dashboardPort: Int = 9091

    init() {
        startPolling()
    }

    var hitRateFormatted: String {
        String(format: "%.0f%%", hitRate)
    }

    func startPolling() {
        Task { await fetchStats() }
        timer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in
                await self?.fetchStats()
            }
        }
    }

    func stopPolling() {
        timer?.invalidate()
        timer = nil
    }

    private func fetchStats() async {
        guard let url = URL(string: "http://127.0.0.1:\(dashboardPort)/api/stats") else { return }

        do {
            let (data, _) = try await URLSession.shared.data(from: url)
            let stats = try JSONDecoder().decode(StatsResponse.self, from: data)

            requests = stats.requests
            cacheHits = stats.cache_hits
            cacheMisses = stats.cache_misses
            bandwidthSaved = stats.bandwidth_saved_human
            hitRate = stats.hit_rate
            bypassEnabled = stats.bypass_enabled
            activeEntries = stats.active_entries
            staleEntries = stats.stale_entries
            activeSizeHuman = stats.active_size_human
            totalSizeHuman = stats.total_size_human
            imageCount = stats.image_count
            videoCount = stats.video_count
            audioCount = stats.audio_count
            maxCacheSizeHuman = stats.max_cache_size_human
            systemProxyEnabled = stats.system_proxy_enabled

            // Update menu bar title
            if requests > 0 {
                menuBarTitle = "\(hitRateFormatted) · \(activeSizeHuman)"
            } else {
                menuBarTitle = "⇅"
            }
        } catch {
            // Silently ignore polling errors
        }
    }
}

private struct StatsResponse: Decodable {
    let requests: Int
    let cache_hits: Int
    let cache_misses: Int
    let bandwidth_saved: Int
    let bandwidth_saved_human: String
    let hit_rate: Double
    let bypass_enabled: Bool
    let system_proxy_enabled: Bool
    let active_entries: Int
    let stale_entries: Int
    let active_size: Int
    let active_size_human: String
    let total_size: Int
    let total_size_human: String
    let image_count: Int
    let video_count: Int
    let audio_count: Int
    let max_cache_size: Int
    let max_cache_size_human: String
}
