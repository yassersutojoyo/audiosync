import AppKit
import CAudioSyncCore
import CoreImage
import CoreImage.CIFilterBuiltins
import SwiftUI

/// The Web Remote setting, in a window of its own like the equalizer.
final class WebRemoteWindowController {
    private var window: NSWindow?
    private let remote: WebRemote
    private let text: UIStrings

    init(remote: WebRemote, text: UIStrings) {
        self.remote = remote
        self.text = text
    }

    func show() {
        if window == nil {
            let host = NSHostingController(rootView: WebRemoteView(settings: remote.settings, text: text))
            let panel = NSWindow(contentViewController: host)
            panel.title = text.webRemoteTitle
            panel.styleMask = [.titled, .closable]
            panel.isReleasedWhenClosed = false
            panel.center()
            window = panel
        }
        remote.windowOpened()
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
    }
}

struct WebRemoteView: View {
    @ObservedObject var settings: WebRemoteSettings
    let text: UIStrings
    @State private var portText = ""

    var body: some View {
        let t = text
        VStack(alignment: .leading, spacing: 14) {
            Toggle(t.webRemoteTitle, isOn: Binding(get: { settings.enabled }, set: { settings.actions.setEnabled($0) }))
                .toggleStyle(.switch)
                .font(.headline)
            Text(t.webRemoteSummary)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
            if settings.enabled {
                switch settings.status {
                case .running(let port):
                    Divider()
                    addressSection(port: port, t)
                    Divider()
                    codeSection(t)
                    Divider()
                    devicesSection(t)
                case .failed(let error, let port):
                    Label(failure(error, port: port, t), systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(Brand.errorText)
                        .fixedSize(horizontal: false, vertical: true)
                case .off:
                    EmptyView()
                }
                Divider()
                portSection(t)
            }
            Text(t.webRemotePlainHTTP)
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(20)
        .frame(width: 460, alignment: .leading)
        .tint(Brand.teal)
        .onAppear { portText = String(settings.port) }
        .onChange(of: settings.port) { _, port in portText = String(port) }
    }

    private func failure(_ error: Int32, port: Int, _ t: UIStrings) -> String {
        switch error {
        case ASC_REMOTE_ERROR_PORT_IN_USE: return t.webRemotePortInUse(port)
        case ASC_REMOTE_ERROR_LISTEN: return t.webRemoteListenFailed(port)
        default: return t.webRemoteStartFailed
        }
    }

    // MARK: Sections

    @ViewBuilder
    private func addressSection(port: Int, _ t: UIStrings) -> some View {
        let urls = settings.addresses.urls(port: port)
        HStack(alignment: .top, spacing: 16) {
            if let first = urls.first, let qr = QRCodeImage.make(first) {
                Image(nsImage: qr)
                    .interpolation(.none)
                    .resizable()
                    .frame(width: 132, height: 132)
                    // Dark modules on white in both appearances: cameras need that contrast.
                    .padding(8)
                    .background(RoundedRectangle(cornerRadius: 8).fill(Color.white))
                    .accessibilityLabel(t.webRemoteQRLabel(first))
            }
            VStack(alignment: .leading, spacing: 6) {
                Text(t.webRemoteAddressTitle)
                    .font(.subheadline.weight(.semibold))
                    .accessibilityAddTraits(.isHeader)
                if urls.isEmpty {
                    Text(t.webRemoteNoAddress).foregroundStyle(Brand.errorText)
                }
                ForEach(Array(urls.enumerated()), id: \.offset) { index, url in
                    Text(url)
                        .font(index == 0 ? .body.monospaced() : .callout.monospaced())
                        .foregroundStyle(index == 0 ? Color.primary : Brand.secondaryText)
                        .textSelection(.enabled)
                }
                Text(t.webRemoteSteps)
                    .font(.callout)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func codeSection(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(t.webRemoteCodeTitle)
                .font(.subheadline.weight(.semibold))
                .accessibilityAddTraits(.isHeader)
            HStack(alignment: .firstTextBaseline, spacing: 14) {
                if settings.locked {
                    Text(t.webRemoteLocked).foregroundStyle(Brand.errorText)
                        .fixedSize(horizontal: false, vertical: true)
                } else if let code = settings.code {
                    Text(code)
                        .font(.system(size: 28, weight: .semibold, design: .monospaced))
                        .tracking(4)
                        .textSelection(.enabled)
                        .accessibilityLabel(t.webRemoteCodeLabel(code))
                } else {
                    Text(t.webRemoteCodeExpired)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 0)
                Button(t.webRemoteNewCode) { settings.actions.newCode() }
            }
            if !settings.locked {
                Text(t.webRemoteCodeNote)
                    .font(.caption)
                    .foregroundStyle(Brand.secondaryText)
            }
        }
    }

    private func devicesSection(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(t.webRemoteDevices(settings.deviceCount))
                Spacer()
                Button(t.webRemoteForgetAll, role: .destructive) { settings.actions.forgetAll() }
                    .disabled(settings.deviceCount == 0)
            }
            if settings.forgetFailed {
                Text(t.webRemoteForgetFailed)
                    .font(.caption)
                    .foregroundStyle(Brand.errorText)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func portSection(_ t: UIStrings) -> some View {
        let typed = RemotePort.parse(portText)
        let failedOnPort: Bool = { if case .failed = settings.status { return true } else { return false } }()
        return VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text(t.webRemotePort)
                TextField(t.webRemotePort, text: $portText)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 80)
                    .accessibilityLabel(t.webRemotePort)
                    .onSubmit { if let typed { settings.actions.usePort(typed) } }
                Button(t.webRemoteUsePort) { if let typed { settings.actions.usePort(typed) } }
                    .disabled(typed == nil || (typed == settings.port && !failedOnPort))
            }
            if typed == nil {
                Text(t.webRemotePortRange)
                    .font(.caption)
                    .foregroundStyle(Brand.errorText)
            }
        }
    }
}

/// A QR code from Core Image's built-in generator, one point per module;
/// the view scales it up without smoothing.
enum QRCodeImage {
    static func make(_ text: String) -> NSImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage,
              let image = CIContext().createCGImage(output, from: output.extent) else { return nil }
        return NSImage(cgImage: image, size: output.extent.size)
    }
}
