import SwiftUI
import NetworkExtension
import EasyTierShared

private func logFileURL() -> URL? {
    FileManager.default
        .containerURL(forSecurityApplicationGroupIdentifier: APP_GROUP_ID)?
        .appendingPathComponent(LOG_FILENAME)
}

/// Pause is kept for this process only (tab switches), not across app relaunch.
@MainActor
private final class LogPauseSession: ObservableObject {
    static let shared = LogPauseSession()
    @Published var userPaused = false
    private init() {}
}

struct LogView<Manager: NetworkExtensionManagerProtocol>: View {
    @Environment(\.scenePhase) private var scenePhase
    @ObservedObject var manager: Manager
    @StateObject private var tailer = LogTailer()
    @ObservedObject private var pauseSession = LogPauseSession.shared
    @Namespace private var bottomID
    @State private var wasWatchingBeforeBackground = false
#if os(iOS)
    @State private var exportURL: URL?
    @State private var isExportPresented = false
#endif
    @State private var exportErrorMessage: TextItem?
    @AppStorage("fileLogEnabled") private var fileLogEnabled = true
    
    var body: some View {
        NavigationStack {
            VStack(alignment: .leading) {
                // Log Content
                ScrollView {
                    ScrollViewReader { proxy in
                        LazyVStack(alignment: .leading) {
                            ForEach(tailer.logContent) { line in
                                Text(line.text)
                                    .font(.system(.footnote, design: .monospaced))
                                    .textSelection(.enabled)
                            }
                            Text("").id(bottomID)
                        }
                        .padding()
                        .onChange(of: tailer.logContent) { _ in
                            // Auto-scroll to bottom on update
                            withAnimation {
                                proxy.scrollTo(bottomID, anchor: .bottom)
                            }
                        }
                    }
                }
#if os(iOS)
                .background(Color(UIColor.systemGroupedBackground))
#endif
                .overlay {
                    if tailer.logContent.isEmpty {
                        VStack(spacing: 8) {
                            Image(systemName: "doc.text")
                                .font(.largeTitle)
                            Text(fileLogEnabled ? "log.empty" : "log.file_disabled")
                                .multilineTextAlignment(.center)
                        }
                        .foregroundStyle(.secondary)
                        .padding()
                    }
                }
            }
            .navigationTitle("logging")
            .adaptiveNavigationBarTitleInline()
            .toolbar {
                ToolbarItem(placement: ToolbarLeading) {
                    Button(action: {
                        Task {
                            await clearLog()
                        }
                    }) {
                        Image(systemName: "trash")
                    }.tint(.red)
                }
                ToolbarItem(placement: ToolbarTrailing) {
                    Button(action: {
                        presentExport()
                    }) {
                        Image(systemName: "square.and.arrow.up")
                    }
                }
                ToolbarItem(placement: ToolbarTrailing) {
                    Button(action: {
                        if tailer.isWatching {
                            pauseSession.userPaused = true
                            tailer.stop()
                            wasWatchingBeforeBackground = false
                        } else {
                            pauseSession.userPaused = false
                            tailer.startWatching(appGroupID: APP_GROUP_ID, filename: LOG_FILENAME, fromStart: false)
                        }
                    }) {
                        Image(systemName: tailer.isWatching ? "pause" : "play")
                    }
                }
            }
        }
        .onAppear {
            // Leaving the bottom tab stops watching to save resources, but if the user
            // paused deliberately, do not auto-resume when returning.
            if !tailer.isWatching && !pauseSession.userPaused {
                tailer.startWatching(appGroupID: APP_GROUP_ID, filename: LOG_FILENAME, fromStart: true)
            }
        }
        .onDisappear {
            if !pauseSession.userPaused {
                wasWatchingBeforeBackground = false
            }
            tailer.stop()
        }
        .onChange(of: scenePhase) { newPhase in
            switch newPhase {
            case .active:
                if wasWatchingBeforeBackground && !pauseSession.userPaused {
                    tailer.startWatching(appGroupID: APP_GROUP_ID, filename: LOG_FILENAME, fromStart: false)
                    wasWatchingBeforeBackground = false
                }
            case .inactive, .background:
                wasWatchingBeforeBackground = !pauseSession.userPaused && tailer.isWatching
                tailer.stop()
            @unknown default:
                break
            }
        }
        .alert(item: Binding(
            get: { tailer.errorMessage ?? exportErrorMessage },
            set: { _ in
                tailer.errorMessage = nil
                exportErrorMessage = nil
            }
        )) { msg in
            Alert(title: Text("common.error"), message: Text(msg.text))
        }
#if os(iOS)
        .sheet(isPresented: $isExportPresented) {
            if let url = exportURL {
                ShareSheet(activityItems: [url])
            }
        }
#endif
    }

    private func presentExport() {
        guard let url = logFileURL() else {
            exportErrorMessage = .init("Log file not found.")
            return
        }
        guard FileManager.default.fileExists(atPath: url.path) else {
            exportErrorMessage = .init("Log file not found.")
            return
        }
#if os(iOS)
        exportURL = url
        isExportPresented = true
#elseif os(macOS)
        do {
            try saveExportedFileToDisk(url)
        } catch {
            exportErrorMessage = .init(error.localizedDescription)
        }
#endif
    }

    private func clearLog() async {
        let providerClear: (() async throws -> Void)?
        if shouldUseProviderClear {
            providerClear = { try await manager.clearCoreLog() }
        } else {
            providerClear = nil
        }
        await tailer.clear(appGroupID: APP_GROUP_ID, filename: LOG_FILENAME, providerClear: providerClear)
    }

    private var shouldUseProviderClear: Bool {
        // Extension clear_logger only works while the tunnel session is connected.
        manager.status == .connected
    }
}

#if DEBUG
#Preview("Log") {
    LogView(manager: MockNEManager())
}
#endif
