import Foundation
import AppKit

/// Manages the Rust proxy binary as a child process.
@MainActor
class ProxyService: ObservableObject {
    @Published var isRunning = false
    @Published var lastError: String?

    private var process: Process?
    private let dashboardPort: Int = 9091

    init() {
        // Auto-detect running proxy on launch
        Task { @MainActor [weak self] in
            await self?.detectRunningProxy()
        }
    }

    private func detectRunningProxy() async {
        // Check PID file first
        if let existingPid = readPidFile(), isProcessAlive(existingPid) {
            isRunning = true
            return
        }
        // Also try health endpoint in case PID file is missing
        if let url = URL(string: "http://127.0.0.1:\(dashboardPort)/api/health"),
           let (_, response) = try? await URLSession.shared.data(from: url),
           let http = response as? HTTPURLResponse,
           http.statusCode == 200 {
            isRunning = true
        }
    }

    /// Path to the proxy binary. Looks in the app bundle first, then PATH.
    private var proxyBinaryPath: String? {
        // Check inside the app bundle
        if let bundled = Bundle.main.path(forAuxiliaryExecutable: "mac-proxy-cache") {
            return bundled
        }
        // Check common build locations
        let candidates = [
            "\(NSHomeDirectory())/github/mac-proxy-cache/target/release/mac-proxy-cache",
            "\(NSHomeDirectory())/github/mac-proxy-cache/target/debug/mac-proxy-cache",
            "/usr/local/bin/mac-proxy-cache",
        ]
        for path in candidates {
            if FileManager.default.fileExists(atPath: path) {
                return path
            }
        }
        return nil
    }

    private var pidFilePath: String {
        "\(NSHomeDirectory())/mac-proxy-cache/proxy.pid"
    }

    func start() {
        // Check if already running via PID file
        if let existingPid = readPidFile(), isProcessAlive(existingPid) {
            print("Proxy already running (PID \(existingPid)), adopting")
            isRunning = true
            return
        }

        guard let binaryPath = proxyBinaryPath else {
            lastError = "Could not find mac-proxy-cache binary"
            return
        }

        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: binaryPath)
        proc.arguments = ["start", "--foreground"]

        let errorPipe = Pipe()
        proc.standardError = errorPipe

        proc.terminationHandler = { [weak self] process in
            DispatchQueue.main.async {
                self?.isRunning = false
                if process.terminationStatus != 0 && process.terminationStatus != 15 {
                    // Read stderr for error details
                    let data = errorPipe.fileHandleForReading.readDataToEndOfFile()
                    let stderr = String(data: data, encoding: .utf8) ?? ""
                    self?.lastError = "Proxy exited with code \(process.terminationStatus): \(stderr.prefix(200))"
                }
            }
        }

        do {
            try proc.run()
            process = proc
            // Poll for health
            Task {
                await waitForHealth()
            }
        } catch {
            lastError = "Failed to start proxy: \(error.localizedDescription)"
        }
    }

    func stop() {
        if let proc = process, proc.isRunning {
            proc.terminate() // sends SIGTERM
            process = nil
        } else if let pid = readPidFile(), isProcessAlive(pid) {
            kill(pid_t(pid), SIGTERM)
        }
        isRunning = false
    }

    private func waitForHealth() async {
        let url = URL(string: "http://127.0.0.1:\(dashboardPort)/api/health")!
        for _ in 0..<25 { // 5 seconds, 200ms intervals
            try? await Task.sleep(nanoseconds: 200_000_000)
            if let (_, response) = try? await URLSession.shared.data(from: url),
               let http = response as? HTTPURLResponse,
               http.statusCode == 200 {
                await MainActor.run {
                    self.isRunning = true
                    self.lastError = nil
                }
                return
            }
        }
        await MainActor.run {
            self.lastError = "Proxy failed to start within 5 seconds"
        }
    }

    private func readPidFile() -> Int? {
        guard let content = try? String(contentsOfFile: pidFilePath, encoding: .utf8) else {
            return nil
        }
        return Int(content.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private func isProcessAlive(_ pid: Int) -> Bool {
        kill(pid_t(pid), 0) == 0
    }
}
