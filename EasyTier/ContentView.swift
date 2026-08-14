import SwiftUI

#if os(iOS)
let columnMaxWidth: CGFloat = 450
let columnMinWidth: CGFloat = 300
#else
let columnMaxWidth: CGFloat = 360
let columnMinWidth: CGFloat = 300
#endif

struct ContentView<Manager: NetworkExtensionManagerProtocol>: View {
    enum TabItem: Hashable {
        case dashboard, log, settings
    }

    @ObservedObject var manager: Manager
    @StateObject private var selectedSession = SelectedProfileSession()
    
#if os(macOS)
    @State private var selectedTab: TabItem? = .dashboard
#else
    @State private var selectedTab: TabItem = .dashboard

    private var tabSelection: Binding<TabItem> {
        Binding(
            get: { selectedTab },
            set: { newValue in
                if newValue != selectedTab {
                    resignCurrentFirstResponder()
                }
                selectedTab = newValue
            }
        )
    }
#endif
    
    var body: some View {
#if os(macOS)
        NavigationSplitView {
            List(selection: $selectedTab) {
                NavigationLink(value: TabItem.dashboard) {
                    Label("main.dashboard", systemImage: "list.bullet.below.rectangle")
                }
                NavigationLink(value: TabItem.log) {
                    Label("logging", systemImage: "rectangle.and.text.magnifyingglass")
                }
                NavigationLink(value: TabItem.settings) {
                    Label("settings", systemImage: "gearshape")
                }
            }
        } detail: {
            switch selectedTab {
            case .dashboard:
                DashboardView(manager: manager, selectedSession: selectedSession)
            case .log:
                LogView(manager: manager)
            case .settings:
                SettingsView(manager: manager, selectedSession: selectedSession)
            case .none:
                ZStack {
#if os(iOS)
                    Color(.systemGroupedBackground)
#endif
                    Image(systemName: "network")
                        .resizable()
                        .frame(width: 128, height: 128)
                        .foregroundStyle(Color.accentColor.opacity(0.2))
                }
                .ignoresSafeArea()
            }
        }
        .navigationTitle("EasyTier")
        .frame(minWidth: 500, minHeight: 300)
#else
            TabView(selection: tabSelection) {
                DashboardView(manager: manager, selectedSession: selectedSession)
                    .tabItem {
                        Image(systemName: "list.bullet.below.rectangle")
                        Text("main.dashboard")
                    }
                    .tag(TabItem.dashboard)
                LogView(manager: manager)
                    .tabItem {
                        Image(systemName: "rectangle.and.text.magnifyingglass")
                        Text("logging")
                    }
                    .tag(TabItem.log)
                SettingsView(manager: manager, selectedSession: selectedSession)
                    .tabItem {
                        Image(systemName: "gearshape")
                            .environment(\.symbolVariants, .none)
                        Text("settings")
                    }
                    .tag(TabItem.settings)
            }
#endif
    }
}

#if DEBUG
#Preview("Content") {
    let manager = MockNEManager()
    return ContentView(manager: manager)
}

@available(iOS 17.0, macOS 14.0, *)
#Preview("Content Landscape", traits: .landscapeLeft) {
    let manager = MockNEManager()
    ContentView(manager: manager)
}
#endif
