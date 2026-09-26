import NetworkExtension
import Network
import os

import EasyTierShared

let magicDNSCIDR = RunningIPv4CIDR(from: "100.100.100.101/32")!

func buildSettings(_ options: EasyTierOptions) -> NEPacketTunnelNetworkSettings {
    let settings = NEPacketTunnelNetworkSettings(tunnelRemoteAddress: "127.0.0.1")
    let runningInfo = fetchRunningInfo()
    if runningInfo == nil {
        logger.warning("prepareSettings() running info is nil")
    }

    let ipv4Settings: NEIPv4Settings?
    if let ipv4 = runningInfo?.myNodeInfo?.virtualIPv4,
       let mask = cidrToSubnetMask(ipv4.networkLength) {
        ipv4Settings = NEIPv4Settings(
            addresses: [ipv4.address.description],
            subnetMasks: [mask]
        )
    } else if let ipv4 = options.ipv4,
              let cidr = RunningIPv4CIDR(from: ipv4),
              let mask = cidrToSubnetMask(cidr.networkLength) {
        ipv4Settings = NEIPv4Settings(
            addresses: [cidr.address.description],
            subnetMasks: [mask]
        )
    } else {
        ipv4Settings = nil
    }
    let routes = buildIPv4Routes(info: runningInfo, options: options)
    if let ipv4Settings, !routes.isEmpty {
        logger.info("prepareSettings() ipv4 routes: \(routes.count)")
        ipv4Settings.includedRoutes = routes
        settings.ipv4Settings = ipv4Settings
    }

    if let ipv6CIDR = options.ipv6?.split(separator: "/"), ipv6CIDR.count == 2 {
        let ip = ipv6CIDR[0], cidrStr = ipv6CIDR[1]
        if let cidr = Int(cidrStr), (0...128).contains(cidr), let address = IPv6Address(String(ip)) {
            settings.ipv6Settings = .init(
                addresses: [String(ip)],
                networkPrefixLengths: [NSNumber(value: cidr)]
            )
            var subnet = Array(address.rawValue)
            for index in subnet.indices {
                let bits = max(0, min(8, cidr - index * 8))
                subnet[index] &= bits == 0 ? 0 : UInt8.max << (8 - bits)
            }
            if let network = IPv6Address(Data(subnet)) {
                settings.ipv6Settings?.includedRoutes = [NEIPv6Route(
                    destinationAddress: network.debugDescription,
                    networkPrefixLength: NSNumber(value: cidr)
                )]
            }
        }
    }

    // DHCP may not have assigned an address yet. Keep the Web control tunnel
    // alive without DNS, MTU, or a premature TUN binding until an address arrives.
    guard settings.ipv4Settings != nil || settings.ipv6Settings != nil else { return settings }

    if (settings.ipv4Settings != nil || !options.dns.isEmpty), let dns = buildDNSServers(options: options) {
        settings.dnsSettings = dns
    }
    
    if let mtu = options.mtu {
        settings.mtu = NSNumber(value: mtu)
    }

    return settings
}

func buildIPv4Routes(info: RunningInfo?, options: EasyTierOptions) -> [NEIPv4Route] {
    var cidrs = Set<RunningIPv4CIDR>()
    // Additive: manual routes used to replace the derived ones, which made every peer's
    // proxied subnet unreachable as soon as one manual route existed. includedRoutes is the
    // only way a route reaches the tun device here, so there is no system table to fall back
    // on. The overlap pass below drops whatever is redundant.
    if !options.routes.isEmpty {
        logger.info("buildIPv4Routes() found manual routes: \(options.routes.count)")
        for route in options.routes {
            if let normalized = normalizeCIDR(route) {
                cidrs.insert(normalized)
            }
        }
    }
    if let routes = info?.routes {
        for route in routes {
            for cidr in route.proxyCIDRs {
                if let normalized = normalizeCIDR(cidr) {
                    cidrs.insert(normalized)
                }
            }
        }
    }
    if let ipv4 = options.ipv4, let cidr = RunningIPv4CIDR(from: ipv4) {
        cidrs.insert(.init(address: ipv4MaskedSubnet(cidr), length: cidr.networkLength))
    }
    if let ipv4 = info?.myNodeInfo?.virtualIPv4 {
        cidrs.insert(.init(address: ipv4MaskedSubnet(ipv4), length: ipv4.networkLength))
    }
    if options.magicDNS {
        cidrs.insert(magicDNSCIDR)
    }
    if cidrs.isEmpty {
        logger.warning("buildIPv4Routes() no routes")
    }
    var sortedCIDRs = Array(cidrs)
    sortedCIDRs.sort { $0.networkLength < $1.networkLength }
    var indicesToRemove = Set<Int>()
    for i in 0..<sortedCIDRs.count {
        if indicesToRemove.contains(i) {
            continue
        }
        for j in (i + 1)..<sortedCIDRs.count {
            if indicesToRemove.contains(j) {
                continue
            }
            if ipv4SubnetsOverlap(bigger: sortedCIDRs[i], smaller: sortedCIDRs[j]) {
                logger.warning("buildIPv4Routes() remove covered route: \(sortedCIDRs[j].address.description, privacy: .public)/\(sortedCIDRs[j].networkLength, privacy: .public) covered by \(sortedCIDRs[i].address.description, privacy: .public)/\(sortedCIDRs[i].networkLength, privacy: .public)")
                indicesToRemove.insert(j)
            }
        }
    }
    for index in indicesToRemove.sorted(by: >) {
        sortedCIDRs.remove(at: index)
    }
    return sortedCIDRs.compactMap { cidr in
        guard let mask = cidrToSubnetMask(cidr.networkLength) else {
            logger.warning("buildIPv4Routes() invalid cidr length: \(cidr.networkLength, privacy: .public)")
            return nil
        }
        return NEIPv4Route(destinationAddress: cidr.address.description, subnetMask: mask)
    }
}

func buildDNSServers(options: EasyTierOptions) -> NEDNSSettings? {
    var settings: NEDNSSettings
    if !options.dns.isEmpty {
        logger.info("buildDNSServers() use override dns: \(options.dns.count)")
        settings = .init(servers: options.dns)
        settings.matchDomains = [""]
    } else if options.magicDNS {
        settings = .init(servers: [magicDNSCIDR.address.description])
        settings.matchDomains = ["et.net"]
    } else {
        return nil
    }
    
    if options.magicDNS {
        logger.info("buildDNSServers() enabled magic dns")
        settings.searchDomains = ["et.net"]
    }
    return settings
}
