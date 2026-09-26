import AppIntents
import EasyTierShared
import NetworkExtension
import SwiftUI

@available(iOS 18.0, *)
struct NetworkProfileEntity: AppEntity {
    static let typeDisplayRepresentation: TypeDisplayRepresentation = "easytier_network"
    static let defaultQuery = NetworkProfileQuery()

    var id: String
    var name: String

    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(name)")
    }

    init(id: String, name: String) {
        self.id = id
        self.name = name
    }

    init(from profile: NetworkProfile) {
        self.id = profile.networkName
        self.name = profile.networkName
    }
}

@available(iOS 18.0, *)
struct NetworkProfileQuery: EntityQuery {
    func entities(for identifiers: [String]) async throws -> [NetworkProfileEntity] {
        return await MainActor.run {
            let profiles = ProfileStore.loadIndexOrEmpty()
            return profiles
                .filter { identifiers.contains($0) }
                .map {
                    return NetworkProfileEntity(id: $0, name: $0)
                }
        }
    }

    func suggestedEntities() async throws -> [NetworkProfileEntity] {
        return await MainActor.run {
            let profiles = ProfileStore.loadIndexOrEmpty()
            return profiles.map {
                return NetworkProfileEntity(id: $0, name: $0)
            }
        }
    }
}

@available(iOS 18.0, *)
enum IntentError: Swift.Error, CustomLocalizedStringResourceConvertible {
    case noProfileFound
    case connectionFailed(String)

    var localizedStringResource: LocalizedStringResource {
        switch self {
        case .noProfileFound:
            return "no_network_profile_found"
        case .connectionFailed(let msg):
            return "connection_failed \(msg)"
        }
    }
}

@available(iOS 18.0, *)
@MainActor
private func prepareProfileForConnection(_ requestedProfile: NetworkProfileEntity?) async throws {
    let defaults = UserDefaults(suiteName: APP_GROUP_ID)
    if requestedProfile == nil,
       defaults?.string(forKey: "connectionMode") == EasyTierConnectionMode.web.rawValue {
        refreshVPNConfigBeforeConnect()
        guard let data = defaults?.data(forKey: "VPNConfig"),
              let options = try? JSONDecoder().decode(EasyTierOptions.self, from: data),
              options.mode == .web,
              !(options.webManagement?.server.isEmpty ?? true) else {
            throw IntentError.noProfileFound
        }
        return
    }
    let profileName = requestedProfile?.id ?? defaults?.string(forKey: "selectedProfileName")
    guard let profileName, !profileName.isEmpty else {
        throw IntentError.noProfileFound
    }

    let session = try await ProfileStore.openSession(named: profileName)
    do {
        var profile = session.document.profile
        let options = try NetworkExtensionManager.generateOptions(&profile)
        session.document.profile = profile
        try await session.save()
        NetworkExtensionManager.saveOptions(options)
        defaults?.set(EasyTierConnectionMode.local.rawValue, forKey: "connectionMode")
        defaults?.set(profileName, forKey: "selectedProfileName")
        await session.close()
    } catch {
        await session.close()
        throw error
    }
}

@available(iOS 18.0, *)
struct ConnectIntent: AppIntent {
    static let title: LocalizedStringResource = "connect_easytier"
    static let description: IntentDescription = IntentDescription("connect_to_easytier_network")
    static let openAppWhenRun: Bool = false

    @Parameter(title: "network")
    var network: NetworkProfileEntity?

    @MainActor
    func perform() async throws -> some IntentResult {
        let manager = NetworkExtensionManager()
        try await manager.load()
        guard ![.connected, .connecting, .reasserting, .disconnecting].contains(manager.status) else {
            return .result()
        }
        try await prepareProfileForConnection(network)
        try await manager.connect()
        return .result()
    }
}

@available(iOS 18.0, *)
struct DisconnectIntent: AppIntent {
    static let title: LocalizedStringResource = "disconnect_easytier"
    static let description: IntentDescription = IntentDescription("disconnect_from_easytier_network")
    static let openAppWhenRun: Bool = false

    @MainActor
    func perform() async throws -> some IntentResult {
        let manager = NetworkExtensionManager()
        try await manager.load()
        await manager.disconnect()
        return .result()
    }
}

@available(iOS 18.0, *)
struct ToggleConnectIntent: AppIntent {
    static let title: LocalizedStringResource = "toggle_easytier"
    static let description: IntentDescription = IntentDescription("toggle_easytier_network_connection")
    static let openAppWhenRun: Bool = false

    @Parameter(title: "network")
    var network: NetworkProfileEntity?

    @MainActor
    func perform() async throws -> some IntentResult {
        let manager = NetworkExtensionManager()
        try await manager.load()

        // Check current status
        // Note: manager.status might be initial state if not refreshed, but load() should refresh it.
        // However, NEManager.load() updates the manager instance which updates status via delegation.
        // We might need a small delay or rely on the fact that load() fetches the managers.

        // Since load() calls setManager which sets status, we can check it.
        // But manager.status is @Published, so accessing it directly is fine on MainActor.

        if [.connected, .connecting, .reasserting].contains(manager.status) {
            await manager.disconnect()
            return .result()
        } else if manager.status == .disconnecting {
            return .result()
        } else {
            try await prepareProfileForConnection(network)
            try await manager.connect()
            return .result()
        }
    }
}

@available(iOS 18.0, *)
struct EasyTierShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(
            intent: ConnectIntent(),
            phrases: [
                "Connect to \(.applicationName)",
                "Start \(.applicationName) VPN",
                "Start \(.applicationName)"
            ],
            shortTitle: "connect_easytier",
            systemImageName: "play.circle"
        )

        AppShortcut(
            intent: DisconnectIntent(),
            phrases: [
                "Disconnect from \(.applicationName)",
                "Stop \(.applicationName) VPN",
                "Stop \(.applicationName)"
            ],
            shortTitle: "disconnect_easytier",
            systemImageName: "stop.circle"
        )

        AppShortcut(
            intent: ToggleConnectIntent(),
            phrases: [
                "Toggle \(.applicationName)",
                "Switch \(.applicationName)"
            ],
            shortTitle: "toggle_easytier",
            systemImageName: "arrow.triangle.2.circlepath.circle"
        )
    }
}
