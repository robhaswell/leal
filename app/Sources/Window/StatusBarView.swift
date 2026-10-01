import AppKit

/// The bar under the grid (DESIGN §4.1, mockups 01a, 02a, 06a and 06b):
/// counts, delimiter, line ending and encoding on the left; indexing
/// progress, or the "Header row: off" toggle, on the right.
@MainActor
final class StatusBarView: NSView {
    static let height: CGFloat = 24

    private let label = NSTextField(labelWithString: "")
    private let progress = NSProgressIndicator()
    private let percent = NSTextField(labelWithString: "")
    let headerToggle = NSButton()
    /// The user clicked the Header row toggle.
    var onToggleHeader: (() -> Void)?

    override init(frame: NSRect) {
        super.init(frame: frame)
        label.font = .systemFont(ofSize: 12)
        label.textColor = .secondaryLabelColor
        label.lineBreakMode = .byTruncatingTail
        label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        progress.style = .bar
        progress.isIndeterminate = false
        progress.minValue = 0
        progress.maxValue = 1
        progress.controlSize = .small
        percent.font = .monospacedDigitSystemFont(ofSize: 11, weight: .regular)
        percent.textColor = .secondaryLabelColor
        percent.alignment = .right
        headerToggle.bezelStyle = .push
        headerToggle.controlSize = .small
        headerToggle.font = .systemFont(ofSize: 11)
        headerToggle.title = String(localized: "Header row: off", comment: "Status bar button: no header row; click to use the first row as the header (mockup 06b)")
        headerToggle.target = self
        headerToggle.action = #selector(toggleHeader(_:))
        headerToggle.toolTip = String(localized: "Use the first row as the header row", comment: "Tooltip of the Header row: off button")

        for view in [label, progress, percent, headerToggle] as [NSView] {
            view.translatesAutoresizingMaskIntoConstraints = false
            addSubview(view)
        }
        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: Self.height),
            label.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 10),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
            label.trailingAnchor.constraint(lessThanOrEqualTo: progress.leadingAnchor, constant: -12),
            label.trailingAnchor.constraint(lessThanOrEqualTo: headerToggle.leadingAnchor, constant: -12),
            percent.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            percent.centerYAnchor.constraint(equalTo: centerYAnchor),
            percent.widthAnchor.constraint(equalToConstant: 34),
            progress.trailingAnchor.constraint(equalTo: percent.leadingAnchor, constant: -6),
            progress.centerYAnchor.constraint(equalTo: centerYAnchor),
            progress.widthAnchor.constraint(equalToConstant: 90),
            headerToggle.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            headerToggle.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    override var isFlipped: Bool { true }
    /// Since the macOS 14 SDK views don't clip to their bounds by default,
    /// and `draw(_:)`'s rectangle may reach past them; this view's fills
    /// must not spill onto its neighbours.
    override var clipsToBounds: Bool {
        get { true }
        set {}
    }

    func show(_ status: StatusSummary) {
        let separator = NSAttributedString(
            string: "  ·  ",
            attributes: [.font: NSFont.systemFont(ofSize: 12), .foregroundColor: NSColor.tertiaryLabelColor]
        )
        let text = NSMutableAttributedString()
        for (index, segment) in StatusText.segments(status).enumerated() {
            if index > 0 { text.append(separator) }
            text.append(NSAttributedString(
                string: segment,
                attributes: [.font: NSFont.systemFont(ofSize: 12), .foregroundColor: NSColor.secondaryLabelColor]
            ))
        }
        label.attributedStringValue = text
        label.toolTip = StatusText.encodingHelp(status.encodingSource)
        progress.isHidden = !status.indexing
        percent.isHidden = !status.indexing
        progress.doubleValue = status.fractionIndexed
        percent.stringValue = StatusText.percent(status.fractionIndexed)
        headerToggle.isHidden = status.header || status.indexing
    }

    /// The label's text, for tests.
    var text: String { label.stringValue }

    @objc private func toggleHeader(_ sender: Any?) {
        onToggleHeader?()
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor.windowBackgroundColor.setFill()
        dirtyRect.fill()
        NSColor.separatorColor.setFill()
        NSRect(x: dirtyRect.minX, y: 0, width: dirtyRect.width, height: 1).fill()
    }
}

/// A non-modal banner under the title bar (ADR-0002): an icon, a message,
/// an optional button and a dismiss button. The UTF-16 notice (mockup 06a)
/// is the first; task 1.7's diagnostics and drive banners use it too.
@MainActor
final class BannerView: NSView {
    enum Kind {
        case info
        case warning
    }

    private let kind: Kind
    var onDismiss: (() -> Void)?

    init(kind: Kind, message: String, buttonTitle: String?, target: AnyObject?, action: Selector?) {
        self.kind = kind
        super.init(frame: .zero)
        let symbol = kind == .info ? "info.circle" : "exclamationmark.triangle"
        let icon = NSImageView(image: NSImage(systemSymbolName: symbol, accessibilityDescription: nil) ?? NSImage())
        icon.symbolConfiguration = .init(pointSize: 14, weight: .regular)
        icon.contentTintColor = tint
        let label = NSTextField(wrappingLabelWithString: message)
        label.font = .systemFont(ofSize: 13)
        label.textColor = tint
        label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        var views: [NSView] = [icon, label]
        if let buttonTitle {
            let button = NSButton(title: buttonTitle, target: target, action: action)
            button.bezelStyle = .push
            // The primary, accent-coloured look (mockup 06a), without making
            // it the default button: Return belongs to the grid.
            if #available(macOS 26.0, *) {
                button.tintProminence = .primary
            } else {
                button.bezelColor = .controlAccentColor
            }
            views.append(button)
        }
        let close = NSButton(
            image: NSImage(systemSymbolName: "xmark", accessibilityDescription: String(localized: "Dismiss", comment: "Banner's close button, for VoiceOver")) ?? NSImage(),
            target: nil,
            action: nil
        )
        close.isBordered = false
        close.contentTintColor = tint
        close.target = self
        close.action = #selector(dismiss(_:))
        let spacer = NSView()
        spacer.setContentHuggingPriority(.init(1), for: .horizontal)
        views.insert(spacer, at: 2)
        views.append(close)
        let stack = NSStackView(views: views)
        stack.orientation = .horizontal
        stack.spacing = 10
        stack.edgeInsets = NSEdgeInsets(top: 8, left: 14, bottom: 8, right: 14)
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor),
            stack.topAnchor.constraint(equalTo: topAnchor),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor),
            heightAnchor.constraint(greaterThanOrEqualToConstant: 38),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    private var tint: NSColor {
        kind == .info ? .systemBlue : .systemOrange
    }

    override var isFlipped: Bool { true }
    /// Since the macOS 14 SDK views don't clip to their bounds by default,
    /// and `draw(_:)`'s rectangle may reach past them; this view's fills
    /// must not spill onto its neighbours.
    override var clipsToBounds: Bool {
        get { true }
        set {}
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor.controlBackgroundColor.setFill()
        bounds.fill()
        tint.withAlphaComponent(0.08).setFill()
        bounds.fill(using: .sourceOver)
        NSColor.separatorColor.setFill()
        NSRect(x: 0, y: bounds.height - 1, width: bounds.width, height: 1).fill()
    }

    @objc private func dismiss(_ sender: Any?) {
        onDismiss?()
    }
}
