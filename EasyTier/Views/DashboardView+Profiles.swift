import SwiftUI
import os
import EasyTierShared

private let profileSelectionLogger = Logger(subsystem: APP_BUNDLE_ID, category: "profile.selection")

extension DashboardView {
    var isSynchronizingProfile: Bool { profileSynchronizationID != nil }

    var hasSelectedProfile: Bool {
        selectedSession.session != nil && selectedSession.session?.name == lastSelected
    }

    @MainActor
    func synchronizeSelectedProfile() async {
        await replaceSelectedProfile(named: lastSelected, updateConnection: false)
    }

    @MainActor
    func loadProfile(_ named: String) async {
        await replaceSelectedProfile(named: named)
    }

    // Manual selections and external changes share one operation token, captured
    // before saving the old editor. Keep it open until the replacement is ready.
    @MainActor
    @discardableResult
    private func replaceSelectedProfile(
        named name: String?,
        saveCurrent: Bool = true,
        updateConnection: Bool = true
    ) async -> Bool {
        guard !Task.isCancelled else { return false }
        let previous = selectedSession.session
        if !updateConnection, previous?.name == name {
            // A delayed SwiftUI sync of the existing selection must not cancel
            // a manual replacement that is still opening or saving its editor.
            currentProfile = previous?.document.profile ?? NetworkProfile()
            return true
        }
        let operation = UUID()
        profileSynchronizationID = operation
        defer {
            if profileSynchronizationID == operation { profileSynchronizationID = nil }
        }
        let selection = lastSelected
        func isCurrentSelection() -> Bool {
            !Task.isCancelled && profileSynchronizationID == operation
                && lastSelected == selection && selectedSession.session === previous
        }
        autoSaveTask?.cancel()
        autoSaveTask = nil
        var candidate: ProfileSession?
        var errorProfileName = previous?.name
        do {
            if saveCurrent, let previous {
                try currentProfile.prepareForUse()
                previous.document.profile = currentProfile
                try await previous.save()
            }
            guard isCurrentSelection() else { return false }
            errorProfileName = name
            if let name { candidate = try await ProfileStore.openSession(named: name) }
            guard isCurrentSelection() else {
                await candidate?.close()
                return false
            }
            var options: EasyTierOptions?
            if updateConnection, let candidate {
                options = try NetworkExtensionManager.generateOptions(&candidate.document.profile)
                try await candidate.save()
            }
            guard isCurrentSelection() else {
                await candidate?.close()
                return false
            }
            await previous?.close()
            guard isCurrentSelection() else {
                await candidate?.close()
                return false
            }
            selectedSession.session = candidate
            currentProfile = candidate?.document.profile ?? NetworkProfile()
            if updateConnection {
                lastSelected = name
                if connectionMode == .local {
                    if let options {
                        NetworkExtensionManager.saveOptions(options)
                    } else {
                        clearConnectionOptions()
                    }
                }
            }
            return true
        } catch {
            await candidate?.close()
            guard isCurrentSelection() else { return false }
            if let conflict = error as? ProfileStoreError, case .conflict = conflict {
                handleConflict(configName: errorProfileName)
            } else {
                errorMessage = .init(error.localizedDescription)
            }
            return false
        }
    }

    @MainActor
    private func clearConnectionOptions() {
        connectionDefaults?.removeObject(forKey: "VPNConfig")
        connectionDefaults?.synchronize()
    }

    // Restore local VPNConfig when leaving web mode. Writes App Group options
    // before awaiting document I/O so the UI can switch immediately.
    @MainActor
    func prepareLocalConnection() async -> Bool {
        guard let session = selectedSession.session else {
            clearConnectionOptions()
            return true
        }
        // Selection moved (e.g. shortcut) while this editor is still open.
        guard session.name == lastSelected else { return false }
        let selection = lastSelected
        do {
            session.document.profile = currentProfile
            let options = try NetworkExtensionManager.generateOptions(&session.document.profile)
            currentProfile = session.document.profile
            NetworkExtensionManager.saveOptions(options)
            try await session.save()
            guard !Task.isCancelled, lastSelected == selection,
                  selectedSession.session === session,
                  session.name == lastSelected else { return false }
            return true
        } catch {
            guard lastSelected == selection, selectedSession.session === session else { return false }
            if let conflict = error as? ProfileStoreError, case .conflict = conflict {
                handleConflict(configName: session.name)
            } else {
                errorMessage = .init(error.localizedDescription)
            }
            return false
        }
    }

    @MainActor
    @discardableResult
    func saveProfile(saveOptions: Bool = true) async -> Bool {
        if let session = selectedSession.session {
            do {
                try currentProfile.prepareForUse()
                session.document.profile = currentProfile
                let options: EasyTierOptions?
                if saveOptions && connectionMode == .local && lastSelected == session.name {
                    options = try NetworkExtensionManager.generateOptions(&session.document.profile)
                    currentProfile = session.document.profile
                } else {
                    options = nil
                }
                try await session.save()
                if let options, connectionMode == .local,
                   lastSelected == session.name, selectedSession.session === session {
                    NetworkExtensionManager.saveOptions(options)
                }
            } catch {
                profileSelectionLogger.error("save failed: \(error)")
                if let conflict = error as? ProfileStoreError,
                   case .conflict = conflict {
                    handleConflict(configName: session.name)
                } else {
                    errorMessage = .init(error.localizedDescription)
                }
                return false
            }
        }
        return true
    }

    @MainActor
    @discardableResult
    func closeSelectedSession(save: Bool = true) async -> Bool {
        await replaceSelectedProfile(named: nil, saveCurrent: save)
    }
}
