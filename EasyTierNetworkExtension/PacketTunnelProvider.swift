import os
import NetworkExtension
import Network
import Foundation

import EasyTierShared

let loggerSubsystem = "\(APP_BUNDLE_ID).tunnel"
let debounceInterval = 0.5
let logger = Logger(subsystem: loggerSubsystem, category: "swift")

private struct ProviderMessageResponse: Codable {
    let ok: Bool
    let path: String?
    let error: String?
}

private final class OneShotErrorCompletion {
    private let lock = NSLock()
    private var handler: ((Error?) -> Void)?

    init(_ handler: @escaping (Error?) -> Void) {
        self.handler = handler
    }

    func finish(_ error: Error?) {
        lock.lock()
        guard let handler else {
            lock.unlock()
            return
        }
        self.handler = nil
        lock.unlock()
        handler(error)
    }
}

private struct PendingStartCompletion {
    let generation: UInt64
    let completion: OneShotErrorCompletion
}

// Shared by both configuration sources; owned by the settings queue.
private struct TunnelSession {
    enum Phase { case starting, ready, failed }

    let options: EasyTierOptions
    var phase = Phase.starting
    var instanceGeneration: UInt64?
    var needsTunRebind = false
    private var pendingEvents: [TunnelInstanceEvent] = []

    init(options: EasyTierOptions) { self.options = options }

    mutating func enqueue(_ event: TunnelInstanceEvent) {
        guard phase != .failed else { return }
        if event.event == "update" {
            pendingEvents.removeAll { $0.event == "update" && $0.generation == event.generation }
        }
        pendingEvents.append(event)
    }

    mutating func nextEvent() -> TunnelInstanceEvent? {
        guard phase == .ready, !pendingEvents.isEmpty else { return nil }
        return pendingEvents.removeFirst()
    }

    mutating func fail() {
        phase = .failed
        pendingEvents.removeAll()
    }
}

class PacketTunnelProvider: NEPacketTunnelProvider {
    // Hold a weak reference to the current provider for C callback bridging
    private static let currentLock = NSLock()
    private static weak var currentProvider: PacketTunnelProvider?
    private static var current: PacketTunnelProvider? {
        get {
            currentLock.lock()
            defer { currentLock.unlock() }
            return currentProvider
        }
        set {
            currentLock.lock()
            defer { currentLock.unlock() }
            currentProvider = newValue
        }
    }

    private func clearCurrentProvider() {
        Self.currentLock.lock()
        defer { Self.currentLock.unlock() }
        if Self.currentProvider === self { Self.currentProvider = nil }
    }
    private let settingsQueue = DispatchQueue(label: "\(APP_BUNDLE_ID).tunnel.settings")
    private var tunnelGeneration: UInt64 = 0
    private var activeTunnelGeneration: UInt64?
    private var settingsApplyGeneration: UInt64?
    private var pendingStartCompletion: PendingStartCompletion?
    private var lastOptions: EasyTierOptions?
    private var lastAppliedSettings: TunnelNetworkSettingsSnapshot?
    private var tunnelSession: TunnelSession?

    private func resetTunnelSessionState() {
        lastOptions = nil
        lastAppliedSettings = nil
        settingsApplyGeneration = nil
        tunnelSession = nil
        reasserting = false
    }

    private func completeStart(generation: UInt64, error: Error?) {
        guard let pendingStartCompletion,
              pendingStartCompletion.generation == generation else {
            return
        }
        self.pendingStartCompletion = nil
        pendingStartCompletion.completion.finish(error)
    }

    private func failStart(generation: UInt64, error: Error, stopNetwork: Bool) {
        guard activeTunnelGeneration == generation else {
            completeStart(generation: generation, error: error)
            return
        }

        notifyHostAppError(error.localizedDescription)
        activeTunnelGeneration = nil
        resetTunnelSessionState()
        clearCurrentProvider()
        if stopNetwork, stop_network_instance() != 0 {
            logger.error("failStart() failed to stop network instance")
        }
        completeStart(generation: generation, error: error)
    }
    
    func postDarwinNotification(_ name: String) {
        let center = CFNotificationCenterGetDarwinNotifyCenter()
        CFNotificationCenterPostNotification(center, CFNotificationName(name as CFString), nil, nil, true)
    }
    
    func notifyHostAppError(_ message: String) {
        // Persist the latest error into shared defaults so the host app can read details
        if let defaults = UserDefaults(suiteName: APP_GROUP_ID) {
            defaults.set(message, forKey: "TunnelLastError")
            defaults.synchronize()
        }
        // Wake the host app via Darwin notification
        postDarwinNotification("\(APP_BUNDLE_ID).error")
    }
    
    private func applyNetworkSettings(
        generation: UInt64,
        completion: @escaping ((any Error)?) -> Void
    ) {
        guard activeTunnelGeneration == generation else {
            completion("tunnel session is no longer active")
            return
        }
        guard settingsApplyGeneration == nil else {
            logger.error("applyNetworkSettings() still in progress")
            completion("still in progress")
            return
        }
        settingsApplyGeneration = generation
        let instanceGeneration = tunnelSession?.instanceGeneration
        reasserting = true

        settingsQueue.asyncAfter(deadline: .now() + debounceInterval) { [weak self] in
            guard let self else {
                completion("packet tunnel provider was deallocated")
                return
            }
            guard self.activeTunnelGeneration == generation,
                  self.settingsApplyGeneration == generation else {
                completion("tunnel session is no longer active")
                return
            }
            guard self.lastOptions != nil || self.tunnelSession != nil else {
                logger.error("applyNetworkSettings() cannot get options")
                self.finishNetworkSettingsApply(
                    generation: generation,
                    snapshot: nil,
                    error: "cannot get options",
                    completion: completion
                )
                return
            }
            guard self.isCurrentInstanceSettings(instanceGeneration) else {
                self.finishNetworkSettingsApply(generation: generation, snapshot: nil,
                    error: "network instance was superseded", completion: completion)
                return
            }

            let settings = self.lastOptions.map(buildSettings)
                ?? NEPacketTunnelNetworkSettings(tunnelRemoteAddress: "127.0.0.1")
            let newSnapshot = self.snapshotSettings(settings)
            let forceTunRebind = self.tunnelSession?.needsTunRebind == true
            if newSnapshot == self.lastAppliedSettings && !forceTunRebind {
                logger.warning("applyNetworkSettings() new settings are exactly the same as last applied, skipping")
                self.finishNetworkSettingsApply(
                    generation: generation,
                    snapshot: newSnapshot,
                    error: nil,
                    completion: completion
                )
                return
            }

            let needSetTunFd = self.shouldUpdateTunFd(old: self.lastAppliedSettings, new: newSnapshot, force: forceTunRebind)
            logger.info("applyNetworkSettings() need set tunfd: \(needSetTunFd), settings: \(settings, privacy: .public)")
            self.setTunnelNetworkSettings(settings) { [weak self] error in
                guard let self else {
                    completion(error ?? "packet tunnel provider was deallocated")
                    return
                }
                self.settingsQueue.async {
                    guard self.activeTunnelGeneration == generation,
                          self.settingsApplyGeneration == generation else {
                        completion("tunnel session is no longer active")
                        return
                    }
                    guard self.isCurrentInstanceSettings(instanceGeneration) else {
                        // The OS may already have installed obsolete routes.
                        // Force the next operation to reconcile even if its
                        // desired snapshot equals the one from before this apply.
                        self.lastAppliedSettings = nil
                        self.finishNetworkSettingsApply(generation: generation, snapshot: nil,
                            error: "network instance was superseded", completion: completion)
                        return
                    }
                    if let error {
                        logger.error("applyNetworkSettings() failed to set tunnel settings: \(error, privacy: .public)")
                        self.finishNetworkSettingsApply(
                            generation: generation,
                            snapshot: newSnapshot,
                            error: error,
                            completion: completion
                        )
                        return
                    }
                    if needSetTunFd {
                        guard let tunFd = self.packetFlow.value(forKeyPath: "socket.fileDescriptor") as? Int32
                                ?? tunnelFileDescriptor() else {
                            let message = "no available tun fd"
                            logger.error("applyNetworkSettings() no available tun fd")
                            self.finishNetworkSettingsApply(
                                generation: generation,
                                snapshot: newSnapshot,
                                error: message,
                                completion: completion
                            )
                            return
                        }
                        logger.info("applyNetworkSettings() found fd: \(tunFd, privacy: .public)")
                        guard setNonBlocking(fd: tunFd) else {
                            let message = "failed to set tun fd non-blocking"
                            logger.error("applyNetworkSettings() failed to set fd \(tunFd, privacy: .public) non-blocking")
                            self.finishNetworkSettingsApply(
                                generation: generation,
                                snapshot: newSnapshot,
                                error: message,
                                completion: completion
                            )
                            return
                        }
                        var errPtr: UnsafePointer<CChar>? = nil
                        // Addressed settings always belong to an admitted instance.
                        guard let instanceGeneration else {
                            self.finishNetworkSettingsApply(generation: generation, snapshot: nil,
                                error: "instance generation is missing", completion: completion)
                            return
                        }
                        let ret = set_instance_tun_fd(instanceGeneration, tunFd, &errPtr)
                        guard ret == 0 else {
                            let message = extractRustString(errPtr) ?? "Unknown"
                            logger.error("applyNetworkSettings() failed to set tun fd to \(tunFd): \(message, privacy: .public)")
                            self.finishNetworkSettingsApply(
                                generation: generation,
                                snapshot: newSnapshot,
                                error: message,
                                completion: completion
                            )
                            return
                        }
                    }
                    logger.info("applyNetworkSettings() settings applied")
                    if needSetTunFd { self.tunnelSession?.needsTunRebind = false }
                    self.finishNetworkSettingsApply(
                        generation: generation,
                        snapshot: newSnapshot,
                        error: nil,
                        completion: completion
                    )
                }
            }
        }
    }

    private func fetchInstanceStatus() throws -> TunnelInstanceStatus {
        var jsonPtr: UnsafePointer<CChar>? = nil
        var errPtr: UnsafePointer<CChar>? = nil
        guard get_instance_status(&jsonPtr, &errPtr) == 0,
              let json = extractRustString(jsonPtr),
              let data = json.data(using: .utf8) else {
            throw extractRustString(errPtr) ?? "cannot get instance status"
        }
        return try JSONDecoder().decode(TunnelInstanceStatus.self, from: data)
    }

    private func isCurrentInstanceSettings(_ generation: UInt64?) -> Bool {
        guard let generation else { return true } // Initial empty control-session settings.
        guard let status = try? fetchInstanceStatus() else { return false }
        return status.generation == generation && status.status != .error
    }

    private func acknowledgeInstanceSetup(generation: UInt64, error: Error?) {
        if let error {
            error.localizedDescription.withCString {
                _ = complete_instance_setup(generation, false, $0)
            }
        } else {
            _ = complete_instance_setup(generation, true, nil)
        }
    }

    private func handleInstanceEvent(_ event: TunnelInstanceEvent) {
        settingsQueue.async { [weak self] in
            guard let self, self.activeTunnelGeneration != nil, self.tunnelSession != nil else { return }
            // A Rust timeout must be able to cancel even if an OS settings
            // completion is stuck. Do not queue errors behind that completion.
            if event.event == "error" {
                guard let status = try? self.fetchInstanceStatus(),
                      status.generation == event.generation,
                      status.instanceID == event.instanceID,
                      let error = status.error else { return }
                self.failSession(error)
                return
            }
            self.tunnelSession?.enqueue(event)
            self.drainInstanceEvents()
        }
    }

    private func failSession(_ error: Error) {
        guard let tunnelSession, tunnelSession.phase != .failed,
              let generation = activeTunnelGeneration else { return }
        self.tunnelSession?.fail()
        if pendingStartCompletion != nil {
            failStart(generation: generation, error: error, stopNetwork: true)
        } else {
            notifyHostAppError(error.localizedDescription)
            cancelTunnelWithError(error)
        }
    }

    private func drainInstanceEvents() {
        guard settingsApplyGeneration == nil,
              let tunnelGeneration = activeTunnelGeneration else { return }
        while let event = tunnelSession?.nextEvent() {
            do {
                let status = try fetchInstanceStatus()
                // Events may arrive after delete, overwrite, stop, or a new VPN session.
                guard status.generation == event.generation,
                      (event.event == "delete" ? status.instanceID == nil : status.instanceID == event.instanceID) else {
                    if event.event == "run" {
                        acknowledgeInstanceSetup(generation: event.generation, error: "network instance was superseded")
                    }
                    continue
                }
                guard ["run", "update", "delete"].contains(event.event) else {
                    continue
                }
                tunnelSession?.instanceGeneration = event.generation
                if event.event == "delete" {
                    if tunnelSession?.options.mode == .local {
                        failSession("network instance was removed")
                        return
                    }
                    lastOptions = nil
                    tunnelSession?.needsTunRebind = false
                } else {
                    guard let options = status.options else { throw "network instance has no tunnel options" }
                    guard let source = tunnelSession?.options else { return }
                    lastOptions = options.applying(to: source)
                    if event.event == "run" { tunnelSession?.needsTunRebind = true }
                }
                applyNetworkSettings(generation: tunnelGeneration) { error in
                    guard self.activeTunnelGeneration == tunnelGeneration else { return }
                    let current = try? self.fetchInstanceStatus()
                    let stillCurrent = current?.generation == event.generation && current?.status != .error
                    if event.event == "run" {
                        self.acknowledgeInstanceSetup(generation: event.generation,
                            error: error ?? (stillCurrent ? nil : "network instance was superseded"))
                    }
                    if let error, stillCurrent {
                        self.failSession(error)
                        return
                    }
                    if event.event == "run", stillCurrent, error == nil {
                        self.completeStart(generation: tunnelGeneration, error: nil)
                    }
                    if self.tunnelSession?.options.mode == .web {
                        self.postDarwinNotification("\(APP_BUNDLE_ID).web-management")
                    }
                }
            } catch {
                if event.event == "run" { acknowledgeInstanceSetup(generation: event.generation, error: error) }
                failSession(error)
            }
            // finishNetworkSettingsApply owns continuation after an OS operation.
            return
        }
    }

    // Only creation differs by source. Both feed the same event queue and wait
    // for the same settings acknowledgement before declaring an instance ready.
    private func startConfiguredTunnel(options: EasyTierOptions, generation: UInt64) {
        tunnelSession = TunnelSession(options: options)
        lastOptions = nil
        let callback: @convention(c) (UnsafePointer<CChar>?) -> Void = { pointer in
            guard let pointer,
                  let data = String(cString: pointer).data(using: .utf8),
                  let event = try? JSONDecoder().decode(TunnelInstanceEvent.self, from: data) else { return }
            PacketTunnelProvider.current?.handleInstanceEvent(event)
        }
        var errPtr: UnsafePointer<CChar>?
        let result: Int32
        switch options.mode {
        case .local:
            result = options.config.withCString { run_network_instance($0, callback, &errPtr) }
        case .web:
            guard let web = options.webManagement,
                  !web.server.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                  !web.machineID.isEmpty else {
                failStart(generation: generation, error: "Web management options are incomplete", stopNetwork: false)
                return
            }
            result = web.server.withCString { server in
                web.hostname.withCString { hostname in
                    web.machineID.withCString { machineID in
                        start_config_server_client(server, hostname, machineID, web.secureMode, callback, &errPtr)
                    }
                }
            }
        }
        guard result == 0 else {
            failStart(generation: generation,
                error: extractRustString(errPtr) ?? "cannot start tunnel", stopNetwork: false)
            return
        }
        if options.mode == .local {
            // Local startup completes only after its run event is applied.
            tunnelSession?.phase = .ready
            drainInstanceEvents()
        } else {
            // A Web control connection is usable before a config is assigned.
            // Install empty settings, then feed later instances through the same queue.
            applyNetworkSettings(generation: generation) { error in
                guard self.activeTunnelGeneration == generation else { return }
                if let error {
                    self.failStart(generation: generation, error: error, stopNetwork: true)
                } else {
                    guard self.tunnelSession?.phase == .starting else { return }
                    self.tunnelSession?.phase = .ready
                    self.completeStart(generation: generation, error: nil)
                    self.postDarwinNotification("\(APP_BUNDLE_ID).web-management")
                }
            }
        }
    }

    private func finishNetworkSettingsApply(
        generation: UInt64,
        snapshot: TunnelNetworkSettingsSnapshot?,
        error: Error?,
        completion: @escaping (Error?) -> Void
    ) {
        guard activeTunnelGeneration == generation,
              settingsApplyGeneration == generation else {
            completion(error ?? "tunnel session is no longer active")
            return
        }

        if error == nil, let snapshot {
            lastAppliedSettings = snapshot
        }
        settingsApplyGeneration = nil
        reasserting = false
        completion(error)
        // Completion acknowledges the current event (or finishes startup).
        // Advance the queue here once those state changes have been committed.
        drainInstanceEvents()

    }

    override func startTunnel(options: [String : NSObject]?, completionHandler: @escaping (Error?) -> Void) {
        logger.warning("startTunnel(): triggered")
        let completion = OneShotErrorCompletion(completionHandler)
        settingsQueue.async {
            self.tunnelGeneration &+= 1
            let generation = self.tunnelGeneration

            if let pendingStartCompletion = self.pendingStartCompletion {
                self.pendingStartCompletion = nil
                pendingStartCompletion.completion.finish("tunnel start was superseded")
            }
            self.resetTunnelSessionState()
            self.activeTunnelGeneration = generation
            self.pendingStartCompletion = .init(generation: generation, completion: completion)
            PacketTunnelProvider.current = self

            let defaults = UserDefaults(suiteName: APP_GROUP_ID)
            guard let configData = defaults?.data(forKey: "VPNConfig"),
                  let options = try? JSONDecoder().decode(EasyTierOptions.self, from: configData) else {
                let message = "options is nil"
                logger.error("startTunnel() options is nil")
                self.notifyHostAppError(message)
                self.failStart(generation: generation, error: message, stopNetwork: false)
                return
            }
            initRustLogger(level: options.logLevel)
            self.startConfiguredTunnel(options: options, generation: generation)
        }
    }
    
    override func stopTunnel(with reason: NEProviderStopReason, completionHandler: @escaping () -> Void) {
        logger.warning("stopTunnel(): triggered")
        settingsQueue.async {
            self.tunnelGeneration &+= 1
            self.activeTunnelGeneration = nil
            let pendingStartCompletion = self.pendingStartCompletion
            self.pendingStartCompletion = nil
            self.resetTunnelSessionState()
            self.clearCurrentProvider()

            let ret = stop_network_instance()
            if ret != 0 {
                logger.error("stopTunnel() failed")
            }
            pendingStartCompletion?.completion.finish("tunnel stopped before startup completed")
            completionHandler()
        }
    }
    
    override func handleAppMessage(_ messageData: Data, completionHandler: ((Data?) -> Void)?) {
        logger.debug("handleAppMessage(): triggered")
        // Add code here to handle the message.
        guard let completionHandler else { return }
        if let raw = String(data: messageData, encoding: .utf8),
           let command = ProviderCommand(rawValue: raw) {
            switch command {
            case .clearLog:
                var errPtr: UnsafePointer<CChar>? = nil
                if clear_logger(&errPtr) == 0 {
                    let response = ProviderMessageResponse(ok: true, path: nil, error: nil)
                    let data = try? JSONEncoder().encode(response)
                    completionHandler(data)
                } else {
                    let err = extractRustString(errPtr) ?? "Unknown"
                    logger.error("handleAppMessage() clear logger failed: \(err, privacy: .public)")
                    let response = ProviderMessageResponse(ok: false, path: nil, error: err)
                    let data = try? JSONEncoder().encode(response)
                    completionHandler(data)
                }
            case .exportOSLog:
                do {
                    let url = try OSLogExporter.exportToAppGroup(appGroupID: APP_GROUP_ID)
                    let response = ProviderMessageResponse(ok: true, path: url.path, error: nil)
                    let data = try JSONEncoder().encode(response)
                    completionHandler(data)
                } catch {
                    let response = ProviderMessageResponse(ok: false, path: nil, error: error.localizedDescription)
                    let data = try? JSONEncoder().encode(response)
                    completionHandler(data)
                }
            case .runningInfo:
                var infoPtr: UnsafePointer<CChar>? = nil
                var errPtr: UnsafePointer<CChar>? = nil
                if get_running_info(&infoPtr, &errPtr) == 0, let info = extractRustString(infoPtr) {
                    completionHandler(info.data(using: .utf8))
                } else if let err = extractRustString(errPtr) {
                    logger.error("handleAppMessage() failed: \(err, privacy: .public)")
                    completionHandler(nil)
                } else {
                    completionHandler(nil)
                }
            case .lastNetworkSettings:
                settingsQueue.async { [weak self] in
                    guard let lastAppliedSettings = self?.lastAppliedSettings else {
                        completionHandler(nil)
                        return
                    }
                    do {
                        let data = try JSONEncoder().encode(lastAppliedSettings)
                        completionHandler(data)
                    } catch {
                        logger.error("handleAppMessage() encode settings failed: \(error, privacy: .public)")
                        completionHandler(nil)
                    }
                }
            case .webManagementStatus:
                do {
                    let status = try fetchInstanceStatus()
                    completionHandler(try JSONEncoder().encode(status))
                } catch {
                    logger.error("handleAppMessage() Web status failed: \(error.localizedDescription, privacy: .public)")
                    completionHandler(nil)
                }
            }
            return
        }
        completionHandler(nil)
    }
    
    override func sleep(completionHandler: @escaping () -> Void) {
        // Add code here to get ready to sleep.
        completionHandler()
    }
    
    override func wake() {
        // Add code here to wake up.
    }
}

extension String: @retroactive Error, @retroactive LocalizedError {
    public var errorDescription: String? { self }
}
