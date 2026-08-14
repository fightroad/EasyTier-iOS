import SwiftUI
#if os(iOS)
import UIKit
#elseif os(macOS)
import AppKit
#endif

#if os(iOS)
    let ToolbarLeading = ToolbarItemPlacement.topBarLeading
    let ToolbarTrailing = ToolbarItemPlacement.topBarTrailing
#else
    let ToolbarLeading = ToolbarItemPlacement.navigation
    let ToolbarTrailing = ToolbarItemPlacement.primaryAction
#endif

func availableSystemImage(_ name: String, fallback: String) -> String {
#if os(iOS)
    UIImage(systemName: name) != nil ? name : fallback
#elseif os(macOS)
    NSImage(systemSymbolName: name, accessibilityDescription: nil) != nil ? name : fallback
#else
    name
#endif
}

extension View {
    func decimalKeyboardType() -> some View {
#if os(iOS)
        return self.keyboardType(.decimalPad)
#else
        return self
#endif
    }
    
    func numberKeyboardType() -> some View {
#if os(iOS)
        return self.keyboardType(.numberPad)
#else
        return self
#endif
    }
    
    func adaptiveNavigationBarTitleInline() -> some View {
#if os(iOS)
        return self.navigationBarTitleDisplayMode(.inline)
#else
        return self
#endif
    }
    
    func adaptiveNoTextInputAutocapitalization() -> some View {
#if os(iOS)
        return self.textInputAutocapitalization(.never)
#else
        return self
#endif
    }
}

