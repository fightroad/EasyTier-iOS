
// Appended to PacketTunnelProvider.swift by run-swift-tests.sh.
private final class TestProvider: PacketTunnelProvider {
    var operations = 0
    var completion: ((Error?) -> Void)?
    var cancelled = false
    var cancellationCount = 0
    var lastError: String?
    let applied = DispatchSemaphore(value: 0)

    override func setTunnelNetworkSettings(_ settings: NETunnelNetworkSettings?, completionHandler: ((Error?) -> Void)? = nil) {
        precondition(completion == nil, "network settings operations must not overlap")
        operations += 1
        completion = completionHandler
        applied.signal()
    }

    override func cancelTunnelWithError(_ error: Error?) {
        cancelled = true
        cancellationCount += 1
    }
    override func postDarwinNotification(_ name: String) {}
    override func notifyHostAppError(_ message: String) { lastError = message }
}

private func status(_ generation: UInt64, deleted: Bool = false) {
    let id = deleted ? "null" : "\"test-instance\""
    let options = deleted ? "null" : "{\"routes\":[],\"magicDNS\":false,\"dns\":[]}"
    test_set_status("""
    {"status":"\(deleted ? "waiting_config" : "starting")","serverConnected":true,
     "instanceID":\(id),"generation":\(generation),"options":\(options)}
    """)
}

private func event(_ name: String, _ generation: UInt64) -> TunnelInstanceEvent {
    try! JSONDecoder().decode(TunnelInstanceEvent.self, from: Data("""
    {"event":"\(name)","instance_id":"test-instance","instance_name":"test","network_name":"test","generation":\(generation)}
    """.utf8))
}

extension PacketTunnelProvider {
    fileprivate func prepareTestSession(_ generation: UInt64) {
        settingsQueue.sync {
            resetTunnelSessionState()
            activeTunnelGeneration = generation
            var options = EasyTierOptions()
            options.mode = .web
            tunnelSession = TunnelSession(options: options)
            tunnelSession?.phase = .ready
        }
    }

    fileprivate func syncTest(_ body: () -> Void) { settingsQueue.sync(execute: body) }
}

extension PacketTunnelProvider {
    fileprivate static func runLifecycleTests() {
        let error: Error = "Web instance has no tunnel options"
        precondition(error.localizedDescription == "Web instance has no tunnel options")

        let provider = TestProvider()
        let empty = TunnelNetworkSettingsSnapshot()
        let addressed = TunnelNetworkSettingsSnapshot(ipv4: .init(addresses: ["10.42.0.1"], subnetMasks: ["255.255.255.0"]))
        precondition(!provider.shouldUpdateTunFd(old: nil, new: empty, force: true))
        precondition(provider.shouldUpdateTunFd(old: empty, new: addressed, force: true))
        precondition(provider.shouldUpdateTunFd(old: addressed, new: addressed, force: true))
        precondition(!provider.shouldUpdateTunFd(old: addressed, new: addressed))

        var ipv6Options = EasyTierOptions()
        ipv6Options.ipv6 = "fd42::1234/64"
        let ipv6Settings = buildSettings(ipv6Options)
        precondition(ipv6Settings.ipv4Settings == nil)
        precondition(ipv6Settings.ipv6Settings?.addresses == ["fd42::1234"])
        precondition(ipv6Settings.ipv6Settings?.includedRoutes?.first?.destinationAddress == "fd42::")
        precondition(provider.shouldUpdateTunFd(old: empty, new: provider.snapshotSettings(ipv6Settings), force: true))

        // DHCP has no address yet: run succeeds without requesting any TUN FD,
        // and keeps the rebind flag for the later DHCP update.
        provider.prepareTestSession(1)
        status(10)
        provider.handleInstanceEvent(event("run", 10))
        precondition(provider.applied.wait(timeout: .now() + 3) == .success)
        provider.syncTest {
            precondition(provider.tunnelSession?.needsTunRebind == true)
            let completion = provider.completion
            provider.completion = nil
            completion?(nil)
        }
        provider.syncTest {
            precondition(test_ack_count() == 1 && test_ack_success())
            precondition(provider.tunnelSession?.needsTunRebind == true)
            precondition(!provider.cancelled)
        }

        // A delete arriving during a run waits for the first OS completion.
        // A newer run then supersedes that delete and receives its own ack.
        status(20)
        provider.handleInstanceEvent(event("run", 20))
        precondition(provider.applied.wait(timeout: .now() + 3) == .success)
        status(21, deleted: true)
        provider.handleInstanceEvent(event("delete", 21))
        provider.syncTest { precondition(provider.operations == 2) }
        status(22)
        provider.handleInstanceEvent(event("run", 22))
        provider.syncTest {
            precondition(provider.operations == 2)
            let completion = provider.completion
            provider.completion = nil
            completion?(nil)
        }
        precondition(provider.applied.wait(timeout: .now() + 3) == .success)
        provider.syncTest {
            precondition(!provider.cancelled)
            precondition(provider.tunnelSession?.instanceGeneration == 22)
            let completion = provider.completion
            provider.completion = nil
            completion?(nil)
        }
        provider.syncTest {
            precondition(test_ack_generation() == 22 && test_ack_success())
            precondition(!provider.cancelled)
        }

        // If the OS installs obsolete addressed settings, deletion must still
        // apply empty settings even though the last committed snapshot was empty.
        test_set_status("""
        {"status":"starting","serverConnected":true,"instanceID":"test-instance","generation":25,
         "options":{"ipv4":"10.42.0.1/24","routes":[],"magicDNS":false,"dns":[]}}
        """)
        provider.handleInstanceEvent(event("run", 25))
        precondition(provider.applied.wait(timeout: .now() + 3) == .success)
        status(26, deleted: true)
        provider.handleInstanceEvent(event("delete", 26))
        provider.syncTest {
            let completion = provider.completion
            provider.completion = nil
            completion?(nil)
        }
        precondition(provider.applied.wait(timeout: .now() + 3) == .success, "delete must remove stale OS routes")
        provider.syncTest {
            let completion = provider.completion
            provider.completion = nil
            completion?(nil)
        }
        provider.syncTest {
            precondition(provider.lastOptions == nil)
            precondition(provider.lastAppliedSettings == empty)
        }

        // An OS completion from a stopped session cannot reset the replacement
        // session's options, snapshot, startup state, or acknowledgement.
        status(30)
        provider.handleInstanceEvent(event("run", 30))
        precondition(provider.applied.wait(timeout: .now() + 3) == .success)
        var oldCompletion: ((Error?) -> Void)?
        provider.syncTest { oldCompletion = provider.completion; provider.completion = nil }
        provider.prepareTestSession(2)
        provider.syncTest { oldCompletion?(nil) }
        provider.syncTest {
            precondition(provider.activeTunnelGeneration == 2)
            precondition(provider.tunnelSession?.instanceGeneration == nil)
            precondition(provider.lastAppliedSettings == nil)
            precondition(test_ack_generation() == 25)
            precondition(!provider.cancelled)
        }
        // A timeout must cancel even when the OS never invokes its completion.
        status(40)
        provider.handleInstanceEvent(event("run", 40))
        precondition(provider.applied.wait(timeout: .now() + 3) == .success)
        test_set_status("""
        {"status":"error","serverConnected":true,"instanceID":"test-instance","generation":40,"error":"instance setup timed out"}
        """)
        provider.handleInstanceEvent(event("error", 40))
        provider.handleInstanceEvent(event("error", 40))
        provider.syncTest {
            precondition(provider.cancelled)
            precondition(provider.cancellationCount == 1)
            precondition(provider.lastError == "instance setup timed out")
        }

        // Coalesce an update burst behind an in-flight OS operation, skipping
        // obsolete/unknown events, then continue exactly once on completion.
        let queued = TestProvider()
        queued.prepareTestSession(3)
        status(50)
        queued.handleInstanceEvent(event("run", 50))
        precondition(queued.applied.wait(timeout: .now() + 3) == .success)
        queued.handleInstanceEvent(event("delete", 49))
        queued.handleInstanceEvent(event("unknown", 50))
        for _ in 0..<20 { queued.handleInstanceEvent(event("update", 50)) }
        queued.syncTest {
            precondition(queued.operations == 1)
            let completion = queued.completion
            queued.completion = nil
            completion?(nil)
        }
        precondition(queued.applied.wait(timeout: .now() + 3) == .success)
        queued.syncTest {
            precondition(queued.operations == 2)
            let completion = queued.completion
            queued.completion = nil
            completion?(nil)
        }
        queued.syncTest {
            precondition(queued.operations == 2 && queued.settingsApplyGeneration == nil)
            precondition(!queued.cancelled)
        }
        // Local config uses the same run -> settings -> acknowledgement path.
        // It must not finish VPN startup merely because the core accepted TOML.
        let local = TestProvider()
        var starts = 0
        var startupError: String?
        var localOptions = EasyTierOptions()
        localOptions.config = "local config"
        localOptions.mtu = 1360
        localOptions.dns = ["1.1.1.1"]
        localOptions.logLevel = .debug
        local.syncTest {
            local.activeTunnelGeneration = 100
            local.pendingStartCompletion = .init(generation: 100, completion: OneShotErrorCompletion {
                starts += 1
                startupError = $0?.localizedDescription
            })
            local.startConfiguredTunnel(options: localOptions, generation: 100)
            precondition(local.operations == 0 && starts == 0)
        }
        status(60)
        local.handleInstanceEvent(event("run", 60))
        precondition(local.applied.wait(timeout: .now() + 3) == .success)
        local.syncTest {
            precondition(starts == 0)
            precondition(local.lastOptions?.mode == .local)
            precondition(local.lastOptions?.dns == ["1.1.1.1"])
            precondition(local.lastOptions?.mtu == 1360)
            precondition(local.lastOptions?.logLevel == .debug)
            let completion = local.completion
            local.completion = nil
            completion?(nil)
        }
        local.syncTest {
            precondition(starts == 1 && startupError == nil)
            precondition(test_ack_generation() == 60 && test_ack_success())
        }
        // Live local updates use the same queue and stale completion guard.
        local.handleInstanceEvent(event("update", 60))
        precondition(local.applied.wait(timeout: .now() + 3) == .success)
        let staleLocalCompletion = local.completion
        local.syncTest { local.completion = nil }
        local.prepareTestSession(101)
        local.syncTest { staleLocalCompletion?("obsolete local update") }
        local.syncTest { precondition(local.lastError == nil && !local.cancelled) }

        // Failure while the OS setup is stuck completes local startup once,
        // stops the core, and cannot be undone by the late OS completion.
        let failing = TestProvider()
        var failureCount = 0
        failing.syncTest {
            failing.activeTunnelGeneration = 200
            failing.pendingStartCompletion = .init(generation: 200, completion: OneShotErrorCompletion {
                failureCount += 1
                precondition($0?.localizedDescription == "local initialization timed out")
            })
            failing.startConfiguredTunnel(options: EasyTierOptions(), generation: 200)
        }
        status(70)
        failing.handleInstanceEvent(event("run", 70))
        precondition(failing.applied.wait(timeout: .now() + 3) == .success)
        test_set_status("""
        {"status":"error","serverConnected":false,"instanceID":"test-instance",
         "generation":70,"error":"local initialization timed out"}
        """)
        failing.handleInstanceEvent(event("error", 70))
        failing.handleInstanceEvent(event("error", 70))
        failing.syncTest {
            precondition(failureCount == 1 && failing.activeTunnelGeneration == nil)
            precondition(failing.lastError == "local initialization timed out")
            let completion = failing.completion
            failing.completion = nil
            completion?(nil)
        }
        failing.syncTest { precondition(failureCount == 1 && failing.lastAppliedSettings == nil) }

        // A live local update failure now follows the same terminal error path.
        let live = TestProvider()
        live.prepareTestSession(300)
        live.syncTest { live.tunnelSession = TunnelSession(options: EasyTierOptions()); live.tunnelSession?.phase = .ready }
        status(80)
        live.handleInstanceEvent(event("update", 80))
        precondition(live.applied.wait(timeout: .now() + 3) == .success)
        live.syncTest {
            let completion = live.completion
            live.completion = nil
            completion?("local route update failed")
        }
        live.syncTest {
            precondition(live.lastError == "local route update failed" && live.cancelled)
            precondition(live.cancellationCount == 1)
        }
        print("PASS: error descriptions, DHCP, forced rebind, serialized settings, coalesced event continuation, stale delete/run, stale session completion, one-shot timeout cancellation, local startup acknowledgement, local host options, local update and startup failures")
    }
}

@main
private enum TunnelLifecycleTests {
    static func main() { PacketTunnelProvider.runLifecycleTests() }
}
