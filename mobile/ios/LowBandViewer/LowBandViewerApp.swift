import SwiftUI

/// LowBand Viewer — assisted-side mobile client (pre-flight preview).
///
/// v0.1 scope: join-code entry and session state display. The LBTP session
/// runs in the Rust core (`core/lbtp`), linked over FFI in a later
/// milestone; until then joining validates the code format and shows the
/// consent-first session screen with no live transport.
@main
struct LowBandViewerApp: App {
    var body: some Scene {
        WindowGroup {
            JoinView()
        }
    }
}
