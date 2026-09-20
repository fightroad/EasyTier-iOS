import SwiftUI
#if os(iOS)
import UIKit
#else
import AppKit
#endif

struct WebManagementEditView: View {
    @Environment(\.isEnabled) private var isEnabled
    @Binding var server: String
    @Binding var hostname: String
    @Binding var machineID: String
    @Binding var secureMode: Bool

    let onSave: () -> Void

    @State private var showResetMachineIDAlert = false

    var body: some View {
        Form {
            Section {
                LabeledContent("web_management.server.placeholder") {
                    TextField(
                        "tcp://et-web.console.easytier.net:22020/your_token",
                        text: $server,
                        prompt: Text("tcp://localhost:22020/your_token"),
                        axis: .vertical
                    )
                    .labelsHidden()
                    .multilineTextAlignment(.trailing)
                    .adaptiveNoTextInputAutocapitalization()
                    .autocorrectionDisabled()
                }
                LabeledContent("hostname") {
                    TextField(
                        "common_text.default",
                        text: $hostname,
                        prompt: Text("common_text.default")
                    )
                    .labelsHidden()
                    .multilineTextAlignment(.trailing)
                }
                Toggle("web_management.secure_mode", isOn: $secureMode)
                    .onChange(of: secureMode) { _ in onSave() }
            } header: {
                Text("web_management.title")
            } footer: {
                Text("web_management.secure_mode_help")
            }
            Section("web_management.machine_id") {
                LabeledContent {
                    Button(action: copyMachineID) {
                        Image(systemName: "doc.on.doc")
                    }
                    .help("web_management.machine_id.copy")
                } label: {
                    Text(machineID).font(.system(.caption, design: .monospaced))
                        .textSelection(.enabled)
                }
                Button("web_management.machine_id.reset", role: .destructive) {
                    showResetMachineIDAlert = true
                }
            }
        }
        .formStyle(.grouped)
        .alert("web_management.machine_id.reset_confirm_title", isPresented: $showResetMachineIDAlert) {
            Button("common.cancel", role: .cancel) {}
            Button("reset", role: .destructive) {
                guard isEnabled else { return }
                machineID = UUID().uuidString.lowercased()
                onSave()
            }
        } message: {
            Text("web_management.machine_id.reset_confirm_message")
        }
    }

    private func copyMachineID() {
#if os(iOS)
        UIPasteboard.general.string = machineID
#else
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(machineID, forType: .string)
#endif
    }
}
