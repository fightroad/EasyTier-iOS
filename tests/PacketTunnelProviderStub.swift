import Foundation
import NetworkExtension

// Apple's provider initializer requires an extension host. Shadow only the
// host-facing base class; compile the unmodified production subclass and use
// real NE network-settings types. No test installs or starts a system VPN.
class NEPacketTunnelProvider: NSObject {
    var reasserting = false
    let packetFlow = NSObject()
    func setTunnelNetworkSettings(_ settings: NETunnelNetworkSettings?, completionHandler: ((Error?) -> Void)? = nil) {}
    func cancelTunnelWithError(_ error: Error?) {}
    func startTunnel(options: [String: NSObject]?, completionHandler: @escaping (Error?) -> Void) {}
    func stopTunnel(with reason: NEProviderStopReason, completionHandler: @escaping () -> Void) {}
    func handleAppMessage(_ messageData: Data, completionHandler: ((Data?) -> Void)?) {}
    func sleep(completionHandler: @escaping () -> Void) {}
    func wake() {}
}
