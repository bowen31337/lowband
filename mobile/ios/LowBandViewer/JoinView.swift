import SwiftUI

struct JoinView: View {
    @State private var code = ""
    @State private var status: String?

    /// Short join codes as issued by lowband-signaling, e.g. K7F-2QX.
    private static let joinCode = "^([A-Z0-9]{3}-[A-Z0-9]{3}|[0-9]{9})$"

    var body: some View {
        VStack(spacing: 16) {
            Text("LowBand Viewer")
                .font(.largeTitle.bold())
                .padding(.top, 48)
            Text("Remote assist that works at 64 kbps")
                .font(.subheadline)
                .foregroundStyle(.secondary)

            TextField("Join code (e.g. K7F-2QX)", text: $code)
                .textFieldStyle(.roundedBorder)
                .textInputAutocapitalization(.characters)
                .autocorrectionDisabled()
                .padding(.top, 32)

            Button("Join session") {
                let trimmed = code.trimmingCharacters(in: .whitespaces).uppercased()
                if trimmed.range(of: Self.joinCode, options: .regularExpression) == nil {
                    status = "That doesn't look like a LowBand join code."
                } else {
                    // Transport lands with the FFI milestone; make the
                    // preview state unmistakable for TestFlight testers.
                    status = "Pre-flight preview: code \(trimmed) accepted.\nLive sessions arrive with the LBTP FFI milestone."
                }
            }
            .buttonStyle(.borderedProminent)
            .tint(Color(red: 0.18, green: 0.83, blue: 0.55))

            if let status {
                Text(status)
                    .font(.footnote)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
            }

            Spacer()
        }
        .padding(24)
    }
}

#Preview {
    JoinView()
}
