import SwiftUI
import AppKit

struct RequestEntry: Identifiable, Decodable {
    let id: UInt64
    let timestamp: Int
    let method: String
    let url: String
    let status_code: UInt16
    let size: Int64
    let from_cache: Bool
    let content_type: String?
    let host: String
    let file_path: String?

    /// Sortable helpers (Int for KeyPathComparator compatibility)
    var onDiskSort: Int { file_path != nil ? 1 : 0 }
    var cacheHitSort: Int { from_cache ? 1 : 0 }
    var mimeType: String { content_type?.components(separatedBy: ";").first ?? "" }

    var timeString: String {
        let date = Date(timeIntervalSince1970: TimeInterval(timestamp))
        let fmt = DateFormatter()
        fmt.dateFormat = "HH:mm:ss"
        return fmt.string(from: date)
    }

    var sizeString: String {
        if size == 0 { return "" }
        if size >= 1_048_576 { return String(format: "%.1f MB", Double(size) / 1_048_576.0) }
        if size >= 1024 { return String(format: "%.1f KB", Double(size) / 1024.0) }
        return "\(size) B"
    }

    var isCachedOnDisk: Bool {
        file_path != nil
    }

    var fullFilePath: String? {
        guard let fp = file_path else { return nil }
        return "\(NSHomeDirectory())/mac-proxy-cache/cache/\(fp)"
    }
}

@MainActor
class MonitorViewModel: ObservableObject {
    @Published var entries: [RequestEntry] = []
    @Published var fromInternet: Int64 = 0
    @Published var fromCache: Int64 = 0

    private var timer: Timer?
    private var lastId: UInt64 = 0

    func startPolling() {
        Task { await fetchRequests() }
        timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in
                await self?.fetchRequests()
            }
        }
    }

    func stopPolling() {
        timer?.invalidate()
        timer = nil
    }

    func clear() {
        entries.removeAll()
        lastId = 0
        fromInternet = 0
        fromCache = 0
    }

    private func fetchRequests() async {
        guard let url = URL(string: "http://127.0.0.1:9091/api/requests?since=\(lastId)") else { return }
        do {
            let (data, _) = try await URLSession.shared.data(from: url)
            let newEntries = try JSONDecoder().decode([RequestEntry].self, from: data)
            if !newEntries.isEmpty {
                entries.append(contentsOf: newEntries)
                if entries.count > 2000 {
                    entries.removeFirst(entries.count - 2000)
                }
                lastId = newEntries.last?.id ?? lastId
                for e in newEntries {
                    if e.from_cache {
                        fromCache += e.size
                    } else {
                        fromInternet += e.size
                    }
                }
            }
        } catch {}
    }
}

struct MonitorWindow: View {
    @StateObject private var vm = MonitorViewModel()
    @State private var selection = Set<UInt64>()
    @State private var sortOrder = [KeyPathComparator(\RequestEntry.id, order: .reverse)]
    @State private var searchText = ""
    @State private var filterCached = false
    @State private var filterOnDisk = false
    @State private var filterType = "All"

    private let typeOptions = ["All", "image", "video", "audio", "javascript", "css", "json", "html", "font"]

    private var filteredEntries: [RequestEntry] {
        var result = vm.entries
        if !searchText.isEmpty {
            let q = searchText.lowercased()
            result = result.filter { $0.url.lowercased().contains(q) || $0.host.lowercased().contains(q) }
        }
        if filterCached {
            result = result.filter { $0.from_cache }
        }
        if filterOnDisk {
            result = result.filter { $0.isCachedOnDisk }
        }
        if filterType != "All" {
            let t = filterType.lowercased()
            result = result.filter { ($0.content_type ?? "").lowercased().contains(t) }
        }
        return result.sorted(using: sortOrder)
    }

    var body: some View {
        VStack(spacing: 0) {
            // Stats bar
            HStack(spacing: 16) {
                Label("Internet: \(formatBytes(vm.fromInternet))", systemImage: "arrow.down.circle")
                    .foregroundStyle(.secondary)
                Label("Cache: \(formatBytes(vm.fromCache))", systemImage: "internaldrive")
                    .foregroundStyle(.green)
                Text("\(vm.entries.count) requests")
                    .foregroundStyle(.secondary)

                Divider().frame(height: 14)

                Toggle("Cached", isOn: $filterCached)
                    .toggleStyle(.checkbox)
                Toggle("On Disk", isOn: $filterOnDisk)
                    .toggleStyle(.checkbox)
                Picker("", selection: $filterType) {
                    ForEach(typeOptions, id: \.self) { Text($0) }
                }
                .frame(width: 100)

                Spacer()

                TextField("Filter URL...", text: $searchText)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 200)

                Button("Clear") { vm.clear() }
                    .buttonStyle(.borderless)
            }
            .font(.system(size: 11))
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(.bar)

            // Request table
            Table(filteredEntries, selection: $selection, sortOrder: $sortOrder) {
                TableColumn("#", value: \.id) { entry in
                    Text("\(entry.id)")
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                }
                .width(min: 30, ideal: 45, max: 60)

                TableColumn("Time", value: \.timestamp) { entry in
                    Text(entry.timeString)
                        .monospacedDigit()
                }
                .width(min: 50, ideal: 60, max: 70)

                TableColumn("Host", value: \.host) { entry in
                    Text(entry.host)
                        .lineLimit(1)
                }
                .width(min: 80, ideal: 130, max: 200)

                TableColumn("URL", value: \.url) { entry in
                    Text(entry.url)
                        .lineLimit(1)
                        .help(entry.url)
                }
                .width(min: 200, ideal: 400)

                TableColumn("Size", value: \.size) { entry in
                    Text(entry.sizeString)
                        .monospacedDigit()
                        .frame(maxWidth: .infinity, alignment: .trailing)
                }
                .width(min: 50, ideal: 70, max: 90)

                TableColumn("Disk", value: \.onDiskSort) { entry in
                    if entry.isCachedOnDisk {
                        Image(systemName: "checkmark.circle.fill")
                            .foregroundStyle(.blue)
                            .help("Cached on disk: \(entry.file_path ?? "")")
                    }
                }
                .width(min: 30, ideal: 35, max: 40)

                TableColumn("Cache", value: \.cacheHitSort) { entry in
                    if entry.from_cache {
                        Text("HIT")
                            .font(.system(size: 10, weight: .bold))
                            .foregroundStyle(.green)
                    }
                }
                .width(min: 30, ideal: 40, max: 50)

                TableColumn("Status", value: \.status_code) { entry in
                    Text("\(entry.status_code)")
                        .monospacedDigit()
                        .foregroundStyle(entry.status_code >= 400 ? .red : .primary)
                }
                .width(min: 35, ideal: 45, max: 55)

                TableColumn("Type", value: \.mimeType) { entry in
                    Text(entry.mimeType)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                .width(min: 60, ideal: 100, max: 150)
            }
            .id("\(searchText)|\(filterCached)|\(filterOnDisk)|\(filterType)")
            .contextMenu(forSelectionType: UInt64.self) { selectedIds in
                let selected = vm.entries.filter { selectedIds.contains($0.id) }
                let withFile = selected.filter { $0.isCachedOnDisk }

                if !withFile.isEmpty {
                    Button("Reveal in Finder") {
                        for entry in withFile {
                            if let path = entry.fullFilePath {
                                NSWorkspace.shared.selectFile(path, inFileViewerRootedAtPath: "")
                            }
                        }
                    }
                }

                if let first = selected.first {
                    Button("Copy URL") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(first.url, forType: .string)
                    }
                }
            } primaryAction: { selectedIds in
                // Double-click: reveal in Finder if cached
                let selected = vm.entries.filter { selectedIds.contains($0.id) }
                for entry in selected {
                    if let path = entry.fullFilePath {
                        NSWorkspace.shared.selectFile(path, inFileViewerRootedAtPath: "")
                    }
                }
            }
            .font(.system(size: 11))
        }
        .frame(minWidth: 900, minHeight: 400)
        .onAppear { vm.startPolling() }
        .onDisappear {
            vm.stopPolling()
            NSApp.setActivationPolicy(.accessory)
        }
    }

    private func formatBytes(_ bytes: Int64) -> String {
        if bytes >= 1_073_741_824 { return String(format: "%.1f GB", Double(bytes) / 1_073_741_824.0) }
        if bytes >= 1_048_576 { return String(format: "%.1f MB", Double(bytes) / 1_048_576.0) }
        if bytes >= 1024 { return String(format: "%.1f KB", Double(bytes) / 1024.0) }
        if bytes > 0 { return "\(bytes) B" }
        return "0 B"
    }
}
