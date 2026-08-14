import Foundation
import SwiftUI
import Combine

import EasyTierShared

struct LogLine: Identifiable, Equatable {
    let id = UUID()
    let text: String
}

@MainActor
class LogTailer: ObservableObject {
    @Published var logContent: [LogLine] = []
    @Published var errorMessage: TextItem?
    @Published var isWatching: Bool = false
    
    @AppStorage("logPreservedLines") var logPreservedLines: Int = 1000
    
    private var fileHandle: FileHandle?
    private var source: DispatchSourceFileSystemObject?
    private var currentFileURL: URL?
    
    /// Starts watching a specific file in an App Group
    func startWatching(appGroupID: String, filename: String, fromStart: Bool) {
        guard let containerURL = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroupID) else {
            self.errorMessage = .init("Invalid App Group ID.")
            return
        }
        
        let fileURL = containerURL.appendingPathComponent(filename)
        currentFileURL = fileURL
        
        // Ensure file exists to avoid crash when opening; create if missing
        if !FileManager.default.fileExists(atPath: fileURL.path) {
            try? "".write(to: fileURL, atomically: true, encoding: .utf8)
        }
        
        do {
            let handle = try FileHandle(forReadingFrom: fileURL)
            self.fileHandle = handle
            
            // Initial Read
            if fromStart {
                let data = handle.readDataToEndOfFile()
                if let str = String(data: data, encoding: .utf8) {
                    updateLog(str, replaceAll: true)
                }
            } else {
                handle.seekToEndOfFile()
            }
            
            // Setup DispatchSource to watch for writes / size changes (incl. truncate).
            let fileDescriptor = handle.fileDescriptor
            let source = DispatchSource.makeFileSystemObjectSource(
                fileDescriptor: fileDescriptor,
                eventMask: [.write, .extend],
                queue: DispatchQueue.main
            )
            
            source.setEventHandler { [weak self] in
                self?.readNewData()
            }
            
            source.setCancelHandler {
                try? handle.close()
            }
            
            source.resume()
            self.source = source
            self.isWatching = true
            
        } catch {
            DispatchQueue.main.async {
                self.errorMessage = .init("Could not open file: \(error.localizedDescription)")
            }
        }
    }
    
    private func readNewData() {
        guard let handle = fileHandle else { return }

        // Writer may truncate when the size cap is hit; reset the read offset.
        if let url = currentFileURL,
           let attrs = try? FileManager.default.attributesOfItem(atPath: url.path),
           let fileSize = attrs[.size] as? UInt64,
           handle.offsetInFile > fileSize {
            handle.seek(toFileOffset: 0)
            if let str = String(data: handle.readDataToEndOfFile(), encoding: .utf8), !str.isEmpty {
                updateLog(str, replaceAll: true)
            } else {
                logContent = []
            }
            return
        }

        let data = handle.readDataToEndOfFile()
        if let newString = String(data: data, encoding: .utf8), !newString.isEmpty {
            updateLog(newString)
        }
    }
    
    private func updateLog(_ logs: String, replaceAll: Bool = false) {
        let newLines = logs
            .split(separator: "\n")
            .suffix(logPreservedLines)
            .map { LogLine(text: String($0)) }
        let lines =
            (replaceAll ? [] : self.logContent).suffix(
                logPreservedLines - newLines.count
            ) + newLines

        if Thread.isMainThread {
            self.logContent = lines
        } else {
            DispatchQueue.main.async {
                self.logContent = lines
            }
        }
    }
    
    func stop() {
        source?.cancel()
        source = nil
        fileHandle = nil
        currentFileURL = nil
        isWatching = false
    }

    func clear(
        appGroupID: String,
        filename: String,
        providerClear: (() async throws -> Void)? = nil
    ) async {
        let wasWatching = isWatching
        stop()

        do {
            if let providerClear {
                try await providerClear()
            } else {
                try clearLogFile(appGroupID: appGroupID, filename: filename)
            }

            logContent = []
        } catch {
            errorMessage = .init("Could not clear log: \(error.localizedDescription)")
        }

        if wasWatching {
            startWatching(appGroupID: appGroupID, filename: filename, fromStart: true)
        }
    }

    private func clearLogFile(appGroupID: String, filename: String) throws {
        guard let containerURL = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroupID) else {
            throw CocoaError(.fileNoSuchFile)
        }

        let fileURL = containerURL.appendingPathComponent(filename)

        if !FileManager.default.fileExists(atPath: fileURL.path) {
            FileManager.default.createFile(atPath: fileURL.path, contents: nil)
        }

        let handle = try FileHandle(forWritingTo: fileURL)
        try handle.truncate(atOffset: 0)
        try handle.close()
    }
    
    deinit {
        source?.cancel()
        source = nil
        fileHandle = nil
    }
}
