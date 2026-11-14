// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import SwiftUI

extension Color {
    static let koi = Color(red: 1, green: 0x6A / 255, blue: 0x13 / 255)
    static let onKoi = Color(red: 0x1A / 255, green: 0x0A / 255, blue: 0)
    static let koiText = Color(light: 0xC2480A, dark: 0xFF8F4D)
    static let statusGreen = Color(light: 0x1FA83A, dark: 0x32D74B)
    static let statusRed = Color(light: 0xE0352B, dark: 0xFF453A)
    static let redLabel = Color(light: NSColor(hex: 0xD70015), dark: NSColor(hex: 0xFF6961),
                                increasedContrast: (NSColor(hex: 0xB0000F), NSColor(hex: 0xFF8A82)))
    static let statusYellow = Color(light: 0xB07A00, dark: 0xFFD60A)
    static let needsYou = statusYellow
    static let needsYouTint = Color(light: NSColor(red: 176 / 255, green: 122 / 255, blue: 0, alpha: 0.12),
                                    dark: NSColor(red: 1, green: 214 / 255, blue: 10 / 255, alpha: 0.14))
    static let redTint = Color(light: NSColor(red: 224 / 255, green: 53 / 255, blue: 43 / 255, alpha: 0.1),
                               dark: NSColor(red: 1, green: 69 / 255, blue: 58 / 255, alpha: 0.16))
    static let redFill = Color(red: 0xD7 / 255, green: 0, blue: 0x15 / 255)
    static let content = Color(light: 0xFFFFFF, dark: 0x2C2C2E)
    static let ink = Color(light: NSColor(hex: 0x1D1D1F), dark: NSColor(white: 1, alpha: 0.88))
    static let onInk = Color(light: 0xFFFFFF, dark: 0x1F1F21)
    static let label = Color(light: NSColor(white: 0, alpha: 0.85), dark: NSColor(white: 1, alpha: 0.88))
    static let label3 = Color(light: NSColor(white: 0, alpha: 0.28), dark: NSColor(white: 1, alpha: 0.28))
    static let separatorLine = Color(light: NSColor(white: 0, alpha: 0.1), dark: NSColor(white: 1, alpha: 0.1))
    static let fillStrong = Color(light: NSColor(white: 0, alpha: 0.1), dark: NSColor(white: 1, alpha: 0.14))
    static let accentRing = Color(red: 1, green: 106 / 255, blue: 19 / 255, opacity: 0.35)
    static let label2 = Color(light: NSColor(white: 0, alpha: 0.52), dark: NSColor(white: 1, alpha: 0.56))

    static let panel = Color(light: 0xF9F9FA, dark: 0x1F1F21)
    static let group = Color(light: 0xEFEFF0, dark: 0x2A2A2C)
    static let line = Color(light: 0xEDEDEE, dark: 0x2C2C2E)
    static let lineStrong = Color(light: 0xDEDEDE, dark: 0x353538)
    static let chipBorder = Color(light: 0xE3E3E5, dark: 0x3A3A3D)
    static let text = Color(light: 0x1B1B1B, dark: 0xE3E4E6)
    static let text2 = Color(light: 0x5C5C5E, dark: 0x959597)
    static let text3 = Color(light: 0xA0A0A2, dark: 0x5C5D5F)

    init(light: Int, dark: Int) {
        self.init(light: NSColor(hex: light), dark: NSColor(hex: dark))
    }

    init(light: NSColor, dark: NSColor, increasedContrast: (light: NSColor, dark: NSColor)? = nil) {
        self.init(nsColor: NSColor(name: nil) { appearance in
            switch appearance.bestMatch(from: [.aqua, .darkAqua, .accessibilityHighContrastAqua, .accessibilityHighContrastDarkAqua]) {
            case .darkAqua: dark
            case .accessibilityHighContrastAqua: increasedContrast?.light ?? light
            case .accessibilityHighContrastDarkAqua: increasedContrast?.dark ?? dark
            default: light
            }
        })
    }
}

extension NSColor {
    convenience init(hex: Int) {
        self.init(srgbRed: CGFloat(hex >> 16 & 0xFF) / 255, green: CGFloat(hex >> 8 & 0xFF) / 255,
                  blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
    }
}

struct KoiButtonStyle: ButtonStyle {
    enum Kind {
        case primary, secondary, destructiveConfirm
    }

    let kind: Kind

    func makeBody(configuration: Configuration) -> some View {
        KoiButton(kind: kind, configuration: configuration)
    }
}

extension ButtonStyle where Self == KoiButtonStyle {
    static var primary: KoiButtonStyle { KoiButtonStyle(kind: .primary) }
    static var secondary: KoiButtonStyle { KoiButtonStyle(kind: .secondary) }
    static var destructiveConfirm: KoiButtonStyle { KoiButtonStyle(kind: .destructiveConfirm) }
}

private struct KoiButton: View {
    let kind: KoiButtonStyle.Kind
    let configuration: ButtonStyleConfiguration
    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.isFocused) private var isFocused
    @Environment(\.controlSize) private var controlSize

    private var isSmall: Bool { controlSize <= .small }

    var body: some View {
        configuration.label
            .font(isSmall ? .callout : kind == .secondary ? .body : .body.weight(.medium))
            .lineLimit(1)
            .foregroundStyle(labelColor)
            .padding(.horizontal, isSmall ? 10 : 14)
            .frame(height: isSmall ? 20 : 24)
            .background {
                ZStack {
                    if kind == .secondary { Capsule().fill(Color.separatorLine) }
                    Capsule().fill(fillColor).padding(kind == .secondary ? 1 : 0)
                    if kind == .secondary && configuration.isPressed { Capsule().fill(Color.fillStrong).padding(1) }
                }
                .overlay { if isFocused { Capsule().inset(by: -3.5).strokeBorder(Color.accentRing, lineWidth: 3.5) } }
            }
            .opacity(kind != .secondary && !isEnabled ? 0.45 : 1)
            .contentShape(Capsule())
    }

    private var labelColor: Color {
        switch kind {
        case .primary: .onInk
        case .destructiveConfirm: .white
        case .secondary where !isEnabled: .label3
        case .secondary where configuration.role == .destructive: .redLabel
        case .secondary: .label
        }
    }

    private var fillColor: Color {
        switch kind {
        case .primary: .ink.opacity(configuration.isPressed ? 0.8 : 1)
        case .destructiveConfirm: .redFill.opacity(configuration.isPressed ? 0.8 : 1)
        case .secondary: .content
        }
    }
}

struct CountBadge: View {
    let count: Int
    let fill: Color
    let text: Color

    var body: some View {
        Text(count.formatted())
            .font(.caption.weight(.bold))
            .monospacedDigit()
            .foregroundStyle(text)
            .padding(.horizontal, 6)
            .frame(minWidth: 18, minHeight: 16)
            .background(Capsule().fill(fill))
    }
}

struct ErrorBanner: View {
    let message: String

    var body: some View {
        Label(message, systemImage: "exclamationmark.triangle")
            .foregroundStyle(Color.statusRed)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.redTint))
    }
}

struct RowGroup<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            content
        }
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.content))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.separator))
    }
}

struct ProgressArc: NSViewRepresentable {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func makeNSView(context: Context) -> ArcView {
        ArcView()
    }

    func updateNSView(_ view: ArcView, context: Context) {
        view.spins = !reduceMotion
    }

    final class ArcView: NSView {
        private let arc = CAShapeLayer()
        private static let spinKey = "spin"

        var spins = false {
            didSet { updateSpin() }
        }

        override init(frame: NSRect) {
            super.init(frame: frame)
            wantsLayer = true
            arc.fillColor = nil
            arc.strokeColor = NSColor(Color.koi).cgColor
            arc.lineWidth = 1.5
            arc.lineCap = .round
            layer?.addSublayer(arc)
        }

        required init?(coder: NSCoder) {
            nil
        }

        override func layout() {
            super.layout()
            arc.frame = bounds
            let radius = (min(bounds.width, bounds.height) - arc.lineWidth) / 2
            let path = CGMutablePath()
            path.addArc(center: CGPoint(x: bounds.midX, y: bounds.midY), radius: radius, startAngle: .pi / 2, endAngle: .pi / 2 - 0.7 * 2 * .pi, clockwise: true)
            arc.path = path
        }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            updateSpin()
        }

        private func updateSpin() {
            guard spins, window != nil else {
                arc.removeAnimation(forKey: Self.spinKey)
                return
            }
            guard arc.animation(forKey: Self.spinKey) == nil else { return }
            let spin = CABasicAnimation(keyPath: "transform.rotation.z")
            spin.fromValue = 0
            spin.toValue = -2 * Double.pi
            spin.duration = 1
            spin.repeatCount = .infinity
            spin.isRemovedOnCompletion = false
            arc.add(spin, forKey: Self.spinKey)
        }
    }
}

struct StatusPill: View {
    enum Mark {
        case dot(Color)
        case progress
    }

    let title: String
    let mark: Mark

    var body: some View {
        HStack(spacing: 6) {
            switch mark {
            case .dot(let color):
                Circle().fill(color).frame(width: 6, height: 6)
            case .progress:
                ProgressArc()
                    .frame(width: 8, height: 8)
            }
            Text(verbatim: title)
                .font(.callout.weight(.medium))
                .foregroundStyle(Color.text)
                .lineLimit(1)
        }
        .padding(.horizontal, 8)
        .frame(height: 22)
        .overlay(Capsule().strokeBorder(Color.chipBorder))
    }
}

struct GroupBand<Trailing: View>: View {
    let title: Text
    @ViewBuilder var trailing: Trailing

    init(_ title: Text, @ViewBuilder trailing: () -> Trailing) {
        self.title = title
        self.trailing = trailing()
    }

    init(title: LocalizedStringKey, @ViewBuilder trailing: () -> Trailing) {
        self.init(Text(title), trailing: trailing)
    }

    var body: some View {
        HStack(spacing: 8) {
            title
                .font(.body.weight(.medium))
                .foregroundStyle(Color.text)
            Spacer(minLength: 0)
            trailing
        }
        .padding(.horizontal, 12)
        .frame(height: 36)
        .background(RoundedRectangle(cornerRadius: 6).fill(Color.group))
    }
}

extension GroupBand where Trailing == EmptyView {
    init(_ title: LocalizedStringKey) {
        self.init(Text(title)) { EmptyView() }
    }

    init(verbatim title: String) {
        self.init(Text(verbatim: title)) { EmptyView() }
    }
}

struct PathText: View {
    let path: String

    var body: some View {
        let slash = path.lastIndex(of: "/").map { path.index(after: $0) } ?? path.startIndex
        return HStack(alignment: .firstTextBaseline, spacing: 0) {
            Text(verbatim: String(path[..<slash]))
                .foregroundStyle(Color.text2)
                .truncationMode(.tail)
            Text(verbatim: String(path[slash...]))
                .foregroundStyle(Color.text)
                .truncationMode(.middle)
                .layoutPriority(1)
        }
        .lineLimit(1)
    }
}

struct EmptyState<Actions: View>: View {
    let systemImage: String
    let title: LocalizedStringKey
    let message: Text?
    var imageColor = Color.text3
    @ViewBuilder var actions: Actions

    var body: some View {
        VStack(spacing: 6) {
            Image(systemName: systemImage)
                .font(.system(size: 22, weight: .light))
                .foregroundStyle(imageColor)
                .padding(.bottom, 6)
            Text(title)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Color.text)
            if let message {
                message
                    .font(.body)
                    .foregroundStyle(Color.text2)
                    .multilineTextAlignment(.center)
            }
            actions
                .padding(.top, 8)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

extension EmptyState where Actions == EmptyView {
    init(systemImage: String, title: LocalizedStringKey, message: Text? = nil) {
        self.init(systemImage: systemImage, title: title, message: message) { EmptyView() }
    }
}

struct ErrorLine: View {
    let message: String
    var detail: String?

    init(_ error: SessionError) {
        message = error.message
        detail = error.detail
    }

    init(message: String) {
        self.message = message
    }

    var body: some View {
        Label {
            Text(verbatim: message)
                .foregroundStyle(Color.text)
                .fixedSize(horizontal: false, vertical: true)
        } icon: {
            Image(systemName: "exclamationmark.triangle")
                .foregroundStyle(Color.statusRed)
        }
        .help(detail ?? "")
    }
}
