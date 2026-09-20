import Foundation
import EasyTierShared

// Compile the production Dashboard profile workflows with in-memory documents
// and preferences so asynchronous saves can be paused without touching iCloud
// or the production App Group.
struct NetworkProfile {
    var name = ""
    mutating func prepareForUse() throws {}
}

struct TextItem {
    let text: String
    init(_ text: String) { self.text = text }
}

enum ProfileStoreError: Error { case conflict }

@MainActor
final class ProfileSession {
    final class Document {
        var profile: NetworkProfile
        init(_ name: String) { profile = NetworkProfile(name: name) }
    }
    let name: String
    let document: Document
    var onSave: (() async throws -> Void)?
    var closed = false

    init(_ name: String) {
        self.name = name
        document = Document(name)
    }
    func save() async throws { try await onSave?() }
    func close() async { closed = true }
}

@MainActor
enum ProfileStore {
    static var onOpen: ((String) async throws -> ProfileSession)?
    static func openSession(named name: String) async throws -> ProfileSession {
        if let onOpen { return try await onOpen(name) }
        return ProfileSession(name)
    }
}

@MainActor
enum NetworkExtensionManager {
    static var savedOptions = EasyTierOptions()
    static var hasSavedOptions = true
    static func generateOptions(_ profile: inout NetworkProfile) throws -> EasyTierOptions {
        var options = EasyTierOptions()
        options.config = profile.name
        return options
    }
    static func saveOptions(_ options: EasyTierOptions) {
        savedOptions = options
        hasSavedOptions = true
    }
}

@MainActor
final class MemoryDefaults {
    func removeObject(forKey key: String) {
        precondition(key == "VPNConfig")
        NetworkExtensionManager.hasSavedOptions = false
        NetworkExtensionManager.savedOptions = EasyTierOptions()
    }
    func synchronize() {}
}

@MainActor
final class DashboardView {
    final class Selection { var session: ProfileSession? }
    let selectedSession = Selection()
    var currentProfile = NetworkProfile()
    var profileSynchronizationID: UUID?
    var lastSelected: String?
    var connectionMode = EasyTierConnectionMode.local
    var errorMessage: TextItem?
    var autoSaveTask: Task<Void, Never>?
    var connectionDefaults: MemoryDefaults? = MemoryDefaults()
    func handleConflict(configName: String?) { errorMessage = TextItem(configName ?? "conflict") }

    init(_ name: String) {
        selectedSession.session = ProfileSession(name)
        currentProfile = NetworkProfile(name: name)
        lastSelected = name
        ProfileStore.onOpen = nil
        NetworkExtensionManager.savedOptions = EasyTierOptions()
        NetworkExtensionManager.savedOptions.config = name
        NetworkExtensionManager.hasSavedOptions = true
    }

    func shortcutSelect(_ name: String) {
        NetworkExtensionManager.savedOptions = EasyTierOptions()
        NetworkExtensionManager.savedOptions.config = name
        NetworkExtensionManager.hasSavedOptions = true
        connectionMode = .local
        lastSelected = name
    }
}

@MainActor
private final class SaveGate {
    var entered = false
    private var continuation: CheckedContinuation<Void, Never>?
    func pause() async {
        entered = true
        await withCheckedContinuation { continuation = $0 }
    }
    func waitUntilPaused() async {
        while !entered { await Task.yield() }
    }
    func resume() { continuation?.resume(); continuation = nil }
}

@main
private enum ProfileSelectionTests {
    @MainActor
    static func main() async {
        // A missing remembered profile must release the UI and preserve saved
        // connection options in either mode. The same selection can be retried.
        for mode in [EasyTierConnectionMode.local, .web] {
            let missing = DashboardView("missing")
            missing.selectedSession.session = nil
            missing.connectionMode = mode
            NetworkExtensionManager.savedOptions.mode = mode
            let missingGate = SaveGate()
            ProfileStore.onOpen = { _ in
                await missingGate.pause()
                throw CocoaError(.fileNoSuchFile)
            }
            let loading = Task { await missing.synchronizeSelectedProfile() }
            await missingGate.waitUntilPaused()
            precondition(missing.isSynchronizingProfile)
            missingGate.resume()
            await loading.value
            precondition(!missing.isSynchronizingProfile)
            precondition(!missing.hasSelectedProfile && missing.errorMessage != nil)
            precondition(missing.lastSelected == "missing")
            precondition(NetworkExtensionManager.savedOptions.mode == mode)
            precondition(NetworkExtensionManager.savedOptions.config == "missing")

            ProfileStore.onOpen = nil
            await missing.synchronizeSelectedProfile()
            precondition(!missing.isSynchronizingProfile && missing.hasSelectedProfile)
            precondition(missing.currentProfile.name == "missing")
        }

        // A failed open must also leave explicit selection of another profile
        // available, even when there is no editor to close.
        let recovery = DashboardView("missing")
        recovery.selectedSession.session = nil
        ProfileStore.onOpen = { _ in throw CocoaError(.fileReadCorruptFile) }
        await recovery.synchronizeSelectedProfile()
        precondition(!recovery.isSynchronizingProfile && !recovery.hasSelectedProfile)
        ProfileStore.onOpen = nil
        await recovery.loadProfile("replacement")
        precondition(recovery.hasSelectedProfile && recovery.lastSelected == "replacement")
        precondition(NetworkExtensionManager.savedOptions.config == "replacement")

        // Failure to save the old editor also releases pending, but must not
        // make that editor eligible to connect as the new external selection.
        let failedSave = DashboardView("A")
        failedSave.selectedSession.session?.onSave = { throw CocoaError(.fileWriteNoPermission) }
        failedSave.shortcutSelect("B")
        await failedSave.synchronizeSelectedProfile()
        precondition(!failedSave.isSynchronizingProfile && !failedSave.hasSelectedProfile)
        precondition(failedSave.selectedSession.session?.name == "A")
        precondition(NetworkExtensionManager.savedOptions.config == "B")
        failedSave.selectedSession.session?.onSave = nil
        await failedSave.synchronizeSelectedProfile()
        precondition(failedSave.hasSelectedProfile && !failedSave.isSynchronizingProfile)
        precondition(failedSave.selectedSession.session?.name == "B")

        // Reopening the Dashboard with an existing session restores its editor.
        let restored = DashboardView("A")
        restored.currentProfile = NetworkProfile()
        await restored.synchronizeSelectedProfile()
        precondition(restored.currentProfile.name == "A")

        // SwiftUI can synchronize the old selection while a manual replacement
        // is saving. Renaming also takes this path with an intermediate nil.
        for clearSelection in [false, true] {
            let selecting = DashboardView("A")
            if clearSelection {
                let closed = await selecting.closeSelectedSession()
                precondition(closed && selecting.lastSelected == nil)
                precondition(!NetworkExtensionManager.hasSavedOptions)
            }
            let candidate = ProfileSession("B")
            let candidateGate = SaveGate()
            candidate.onSave = { await candidateGate.pause() }
            ProfileStore.onOpen = { _ in candidate }
            let replacement = Task { await selecting.loadProfile("B") }
            await candidateGate.waitUntilPaused()
            let operation = selecting.profileSynchronizationID
            precondition(operation != nil)
            await selecting.synchronizeSelectedProfile()
            precondition(selecting.profileSynchronizationID == operation)
            precondition(selecting.isSynchronizingProfile && !candidate.closed)
            candidateGate.resume()
            await replacement.value
            precondition(selecting.selectedSession.session === candidate)
            precondition(selecting.lastSelected == "B" && selecting.hasSelectedProfile)
            precondition(!selecting.isSynchronizingProfile && !candidate.closed)
            precondition(NetworkExtensionManager.hasSavedOptions)
            precondition(NetworkExtensionManager.savedOptions.config == "B")
        }

        // An old automatic save must not replace the Shortcut's new VPNConfig.
        let saving = DashboardView("A")
        let gate = SaveGate()
        saving.selectedSession.session?.onSave = { await gate.pause() }
        let oldSave = Task { await saving.saveProfile() }
        await gate.waitUntilPaused()
        saving.shortcutSelect("B")
        gate.resume()
        let saved = await oldSave.value
        precondition(saved)
        precondition(NetworkExtensionManager.savedOptions.config == "B")

        // A same-name editor reopened during a save is a different session.
        let reopened = DashboardView("A")
        let reopenGate = SaveGate()
        reopened.selectedSession.session?.onSave = { await reopenGate.pause() }
        let obsoleteSave = Task { await reopened.saveProfile() }
        await reopenGate.waitUntilPaused()
        reopened.selectedSession.session = ProfileSession("A")
        NetworkExtensionManager.savedOptions.config = "new-A"
        reopenGate.resume()
        let reopenedSaved = await obsoleteSave.value
        precondition(reopenedSaved && NetworkExtensionManager.savedOptions.config == "new-A")

        // Reconcile an external selection without clearing its saved options.
        let view = DashboardView("A")
        let oldSession = view.selectedSession.session!
        view.connectionMode = .web
        view.shortcutSelect("B")
        await view.synchronizeSelectedProfile()
        precondition(oldSession.closed)
        precondition(view.selectedSession.session?.name == "B")
        precondition(view.currentProfile.name == "B" && view.lastSelected == "B")
        precondition(NetworkExtensionManager.savedOptions.config == "B")
        let backgroundSaved = await view.saveProfile()
        precondition(backgroundSaved && NetworkExtensionManager.savedOptions.config == "B")

        // If B finishes opening after C was selected, discard B's editor.
        let switching = DashboardView("A")
        let openGate = SaveGate()
        let obsolete = ProfileSession("B")
        ProfileStore.onOpen = { name in
            if name == "B" { await openGate.pause(); return obsolete }
            return ProfileSession(name)
        }
        switching.shortcutSelect("B")
        let openB = Task { await switching.synchronizeSelectedProfile() }
        await openGate.waitUntilPaused()
        switching.shortcutSelect("C")
        await switching.synchronizeSelectedProfile()
        openGate.resume()
        await openB.value
        precondition(obsolete.closed)
        precondition(switching.selectedSession.session?.name == "C")
        precondition(switching.lastSelected == "C" && NetworkExtensionManager.savedOptions.config == "C")

        // SwiftUI cancels the previous selection task without waiting for its
        // document operation to finish. Its cleanup must not unlock a newer one.
        let cancelled = DashboardView("A")
        let cancelledGate = SaveGate()
        let latestGate = SaveGate()
        let cancelledSession = ProfileSession("B")
        ProfileStore.onOpen = { name in
            if name == "B" { await cancelledGate.pause(); return cancelledSession }
            await latestGate.pause()
            return ProfileSession(name)
        }
        cancelled.shortcutSelect("B")
        let cancelledLoad = Task { await cancelled.synchronizeSelectedProfile() }
        await cancelledGate.waitUntilPaused()
        cancelledLoad.cancel()
        cancelled.shortcutSelect("C")
        let latestLoad = Task { await cancelled.synchronizeSelectedProfile() }
        await latestGate.waitUntilPaused()
        cancelledGate.resume()
        await cancelledLoad.value
        precondition(cancelledSession.closed)
        precondition(cancelled.isSynchronizingProfile && !cancelled.hasSelectedProfile)
        latestGate.resume()
        await latestLoad.value
        precondition(!cancelled.isSynchronizingProfile && cancelled.hasSelectedProfile)
        precondition(cancelled.lastSelected == "C")
        precondition(NetworkExtensionManager.savedOptions.config == "C")

        // Deselecting a local editor must preserve the saved Web connection.
        let web = DashboardView("A")
        web.connectionMode = .web
        NetworkExtensionManager.savedOptions.mode = .web
        NetworkExtensionManager.savedOptions.config = "web-control"
        let closedWeb = await web.closeSelectedSession()
        precondition(closedWeb && web.lastSelected == nil)
        precondition(NetworkExtensionManager.savedOptions.mode == .web)
        precondition(NetworkExtensionManager.savedOptions.config == "web-control")

        // A Shortcut may select B while the old editor is being closed.
        let closing = DashboardView("A")
        let closeGate = SaveGate()
        closing.selectedSession.session?.onSave = { await closeGate.pause() }
        let closeA = Task { await closing.closeSelectedSession() }
        await closeGate.waitUntilPaused()
        closing.shortcutSelect("B")
        closeGate.resume()
        let closed = await closeA.value
        precondition(!closed && closing.lastSelected == "B")
        precondition(NetworkExtensionManager.savedOptions.config == "B")
        closing.selectedSession.session?.onSave = nil
        await closing.synchronizeSelectedProfile()
        precondition(closing.selectedSession.session?.name == "B")

        // Also protect the save performed when explicitly activating an editor.
        let activating = DashboardView("A")
        let activateGate = SaveGate()
        let candidate = ProfileSession("B")
        candidate.onSave = { await activateGate.pause() }
        ProfileStore.onOpen = { _ in candidate }
        let activation = Task { await activating.loadProfile("B") }
        await activateGate.waitUntilPaused()
        activating.shortcutSelect("C")
        activateGate.resume()
        await activation.value
        precondition(candidate.closed && activating.lastSelected == "C")
        precondition(NetworkExtensionManager.savedOptions.config == "C")

        // The selection token must cover saving the previous editor as well
        // as opening and saving the replacement.
        let manual = DashboardView("A")
        let previousGate = SaveGate()
        manual.selectedSession.session?.onSave = { await previousGate.pause() }
        let oldManual = Task { await manual.loadProfile("B") }
        await previousGate.waitUntilPaused()
        precondition(manual.isSynchronizingProfile)
        manual.shortcutSelect("C")
        previousGate.resume()
        await oldManual.value
        precondition(manual.lastSelected == "C")
        precondition(NetworkExtensionManager.savedOptions.config == "C")
        precondition(!manual.isSynchronizingProfile)

        let conflicting = DashboardView("A")
        conflicting.selectedSession.session?.onSave = { throw ProfileStoreError.conflict }
        await conflicting.loadProfile("B")
        precondition(conflicting.errorMessage?.text == "A")
        precondition(conflicting.lastSelected == "A")

        // Superseded opens must neither commit their candidate nor clear the
        // newer editor when the old open eventually fails.
        for failOpen in [false, true] {
            let loading = DashboardView("A")
            let loadingGate = SaveGate()
            let oldCandidate = ProfileSession("B")
            ProfileStore.onOpen = { name in
                if name == "B" {
                    await loadingGate.pause()
                    if failOpen { throw CocoaError(.fileReadCorruptFile) }
                    return oldCandidate
                }
                return ProfileSession(name)
            }
            let obsoleteLoad = Task { await loading.loadProfile("B") }
            await loadingGate.waitUntilPaused()
            loading.shortcutSelect("C")
            await loading.synchronizeSelectedProfile()
            loadingGate.resume()
            await obsoleteLoad.value
            precondition(loading.selectedSession.session?.name == "C")
            precondition(loading.lastSelected == "C" && loading.errorMessage == nil)
            precondition(NetworkExtensionManager.savedOptions.config == "C")
            if !failOpen { precondition(oldCandidate.closed) }
        }

        // Two manual selections also share the token; the last request wins.
        let concurrent = DashboardView("A")
        let firstOpenGate = SaveGate()
        ProfileStore.onOpen = { name in
            if name == "B" { await firstOpenGate.pause() }
            return ProfileSession(name)
        }
        let firstSelection = Task { await concurrent.loadProfile("B") }
        await firstOpenGate.waitUntilPaused()
        await concurrent.loadProfile("C")
        firstOpenGate.resume()
        await firstSelection.value
        precondition(concurrent.lastSelected == "C" && concurrent.hasSelectedProfile)
        precondition(NetworkExtensionManager.savedOptions.config == "C")

        let noLocalProfile = DashboardView("A")
        noLocalProfile.connectionMode = .web
        NetworkExtensionManager.savedOptions.mode = .web
        _ = await noLocalProfile.closeSelectedSession()
        precondition(NetworkExtensionManager.savedOptions.mode == .web)
        let preparedEmpty = await noLocalProfile.prepareLocalConnection()
        precondition(preparedEmpty && !NetworkExtensionManager.hasSavedOptions)
        precondition(noLocalProfile.lastSelected == nil)

        let localMode = DashboardView("A")
        localMode.connectionMode = .web
        NetworkExtensionManager.savedOptions.mode = .web
        let preparedLocal = await localMode.prepareLocalConnection()
        precondition(preparedLocal && NetworkExtensionManager.savedOptions.mode == .local)
        precondition(NetworkExtensionManager.savedOptions.config == "A")
        // The picker publishes the new mode only after preparation succeeds.
        precondition(localMode.connectionMode == .web)

        let failedMode = DashboardView("A")
        failedMode.connectionMode = .web
        NetworkExtensionManager.savedOptions.mode = .web
        failedMode.selectedSession.session?.onSave = { throw CocoaError(.fileWriteNoPermission) }
        let preparedFailure = await failedMode.prepareLocalConnection()
        precondition(!preparedFailure && NetworkExtensionManager.savedOptions.mode == .web)
        precondition(failedMode.connectionMode == .web)

        let switchedExternally = DashboardView("A")
        switchedExternally.connectionMode = .web
        NetworkExtensionManager.savedOptions.mode = .web
        let modeGate = SaveGate()
        switchedExternally.selectedSession.session?.onSave = { await modeGate.pause() }
        let obsoleteModeSwitch = Task { await switchedExternally.prepareLocalConnection() }
        await modeGate.waitUntilPaused()
        switchedExternally.shortcutSelect("C")
        modeGate.resume()
        let preparedObsolete = await obsoleteModeSwitch.value
        precondition(!preparedObsolete && NetworkExtensionManager.savedOptions.config == "C")
        let preparedAfterShortcut = await switchedExternally.prepareLocalConnection()
        precondition(!preparedAfterShortcut && NetworkExtensionManager.savedOptions.config == "C")

        print("PASS: profile load/save failure recovery, retry and reselection, cancelled sync ownership, no-op sync during manual selection/rename, stale saves, external selection sync, superseded opens, Web config preservation, close/activate races, manual selection ownership, local mode preparation")
    }
}
