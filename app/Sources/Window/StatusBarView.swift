import AppKit
import LealFFI

/// The bar under the grid (DESIGN §4.1, mockups 01a, 02a, 03a, 06a and
/// 06b): counts, delimiter, line ending, encoding and notes on the left;
/// indexing progress, the "Header row: off" toggle and the diagnostics
/// badge on the right.
///
/// The delimiter and the encoding are menus (ADR-0005 decision 8): **Treat
/// as** another delimiter, and **Reopen with encoding**. They look like the
/// rest of the bar's text, with a small chevron.
@MainActor
final class StatusBarView: NSView {
    static let height: CGFloat = 24

    private let segments = NSStackView()
    private let progress = NSProgressIndicator()
    private let percent = NSTextField(labelWithString: "")
    let headerToggle = NSButton()
    /// The diagnostics badge, "⚠ 3" (mockup 03a): shows the details.
    let badge = NSButton()
    /// The delimiter's **Treat as** menu, and the encoding's **Reopen with
    /// encoding** menu, once the bar shows them.
    private(set) var delimiterButton: NSButton?
    private(set) var encodingButton: NSButton?
    private var shown: [StatusItem] = []
    private var shownStatus: StatusSummary?

    /// The user clicked the Header row toggle.
    var onToggleHeader: (() -> Void)?
    /// The user chose a delimiter in the **Treat as** menu.
    var onTreatAs: ((Delimiter) -> Void)?
    /// The user chose an encoding in the **Reopen with encoding** menu.
    var onReopen: ((TextEncoding) -> Void)?
    /// The user clicked the diagnostics badge.
    var onBadge: (() -> Void)?

    override init(frame: NSRect) {
        super.init(frame: frame)
        segments.orientation = .horizontal
        segments.spacing = 0
        segments.alignment = .centerY
        segments.setHuggingPriority(.defaultLow, for: .horizontal)
        segments.setClippingResistancePriority(.defaultLow, for: .horizontal)
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
        headerToggle.toolTip = Self.headerToggleHelp
        badge.isBordered = false
        badge.image = NSImage(systemSymbolName: "exclamationmark.triangle.fill", accessibilityDescription: nil)
        badge.symbolConfiguration = .init(pointSize: 10, weight: .semibold)
        badge.contentTintColor = .systemOrange
        badge.imagePosition = .imageLeading
        badge.imageHugsTitle = true
        badge.font = .systemFont(ofSize: 11, weight: .semibold)
        badge.wantsLayer = true
        badge.layer?.cornerRadius = 8
        badge.target = self
        badge.action = #selector(badgeClicked(_:))
        badge.isHidden = true

        let right = NSStackView(views: [headerToggle, progress, percent, badge])
        right.orientation = .horizontal
        right.spacing = 6
        right.alignment = .centerY
        right.setHuggingPriority(.required, for: .horizontal)
        for view in [segments, right] as [NSView] {
            view.translatesAutoresizingMaskIntoConstraints = false
            addSubview(view)
        }
        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: Self.height),
            segments.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 10),
            segments.centerYAnchor.constraint(equalTo: centerYAnchor),
            segments.trailingAnchor.constraint(lessThanOrEqualTo: right.leadingAnchor, constant: -12),
            right.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            right.centerYAnchor.constraint(equalTo: centerYAnchor),
            progress.widthAnchor.constraint(equalToConstant: 90),
            percent.widthAnchor.constraint(equalToConstant: 34),
            badge.heightAnchor.constraint(equalToConstant: 17),
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

    private static var headerToggleHelp: String {
        String(localized: "Use the first row as the header row", comment: "Tooltip of the Header row: off button")
    }

    func show(_ status: StatusSummary) {
        let items = StatusText.items(status)
        if items != shown {
            rebuild(items)
        }
        shownStatus = status
        segments.toolTip = StatusText.help(status)
        // A save's progress, while one runs, in place of the index's (task
        // 2.5.3a): its own steps, `Checking` included, each from 0.
        let fraction = status.saving.map { $0.fraction } ?? status.fractionIndexed
        let busy = status.indexing || status.saving != nil
        progress.isHidden = !busy
        percent.isHidden = !busy || fraction == nil
        progress.doubleValue = fraction ?? 0
        percent.stringValue = StatusText.percent(fraction ?? 0)
        headerToggle.isHidden = status.header || busy
        // After a change while reading, the file can't be read another way
        // until it is reloaded, nor while it is saved: the menus and the
        // toggle are off, and say so.
        let reread = !status.changedOnDisk && status.saving == nil
        let rereadReason = status.changedOnDisk ? StatusText.reloadFirst : StatusText.waitForSave
        headerToggle.isEnabled = reread
        headerToggle.toolTip = reread ? Self.headerToggleHelp : rereadReason
        // Edits are tied to how the file is split: no other delimiter or
        // encoding while there are any (ADR-0008 decision 4).
        let split = reread && !status.unsavedEdits
        let splitReason = reread ? StatusText.saveOrRevertFirst : rereadReason
        delimiterButton?.isEnabled = split
        delimiterButton?.toolTip = split ? StatusText.treatAsHelp : splitReason
        encodingButton?.isEnabled = split
        encodingButton?.toolTip = split ? StatusText.reopenHelp : splitReason
        badge.isHidden = status.warningKinds == 0
        badge.title = status.warningKinds.formatted()
        badge.toolTip = DiagnosticsText.badgeHelp(kinds: status.warningKinds)
        badge.setAccessibilityLabel(DiagnosticsText.badgeHelp(kinds: status.warningKinds))
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        badge.layer?.backgroundColor = NSColor.systemYellow.withAlphaComponent(0.22).cgColor
    }

    override func layout() {
        super.layout()
        badge.layer?.backgroundColor = NSColor.systemYellow.withAlphaComponent(0.22).cgColor
    }

    /// Whether the progress bar (indexing, or a save) shows, for tests.
    var isShowingProgress: Bool { !progress.isHidden }
    /// The progress bar's value and percentage, for tests.
    var progressShown: (value: Double, percent: String?) {
        (progress.doubleValue, percent.isHidden ? nil : percent.stringValue)
    }

    /// The left side's text, segments joined by " · ", for tests.
    var text: String { shown.map(\.text).joined(separator: "  ·  ") }

    private func rebuild(_ items: [StatusItem]) {
        shown = items
        for view in segments.arrangedSubviews { view.removeFromSuperview() }
        delimiterButton = nil
        encodingButton = nil
        infoButton = nil
        let font = NSFont.systemFont(ofSize: 12)
        for (index, item) in items.enumerated() {
            if index > 0 {
                let dot = NSTextField(labelWithString: "  ·  ")
                dot.font = font
                dot.textColor = .tertiaryLabelColor
                segments.addArrangedSubview(dot)
            }
            switch item.role {
            case .plain:
                let label = NSTextField(labelWithString: item.text)
                label.font = font
                label.textColor = .secondaryLabelColor
                label.lineBreakMode = .byTruncatingTail
                label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
                segments.addArrangedSubview(label)
            case .delimiter, .encoding:
                let button = NSButton(title: item.text, target: self, action: #selector(menuClicked(_:)))
                button.isBordered = false
                button.font = font
                button.attributedTitle = NSAttributedString(
                    string: item.text,
                    attributes: [.font: font, .foregroundColor: NSColor.secondaryLabelColor]
                )
                button.image = NSImage(systemSymbolName: "chevron.up.chevron.down", accessibilityDescription: nil)
                button.symbolConfiguration = .init(pointSize: 8, weight: .medium)
                button.contentTintColor = .tertiaryLabelColor
                button.imagePosition = .imageTrailing
                button.imageHugsTitle = true
                // VoiceOver: what it is, its value, and that it opens a menu.
                button.setAccessibilityRole(.popUpButton)
                if item.role == .delimiter {
                    button.toolTip = StatusText.treatAsHelp
                    button.setAccessibilityLabel(StatusText.delimiterMenuLabel(item.text))
                    delimiterButton = button
                } else {
                    button.toolTip = StatusText.reopenHelp
                    button.setAccessibilityLabel(StatusText.encodingMenuLabel(item.text))
                    encodingButton = button
                }
                segments.addArrangedSubview(button)
            case .diagnostics:
                // An info-level note opens the details, which a file with
                // no warning has no banner or badge for.
                let button = NSButton(title: item.text, target: self, action: #selector(badgeClicked(_:)))
                button.isBordered = false
                button.attributedTitle = NSAttributedString(
                    string: item.text,
                    attributes: [.font: font, .foregroundColor: NSColor.secondaryLabelColor]
                )
                button.toolTip = DiagnosticsText.showDetailsHelp
                if infoButton == nil { infoButton = button }
                segments.addArrangedSubview(button)
            }
        }
    }

    /// The first info-level note, which opens the details.
    private(set) var infoButton: NSButton?

    /// Where the details popover points: the badge, or for a file with only
    /// info-level kinds their note, else the bar itself.
    var detailsAnchor: NSView {
        if !badge.isHidden { return badge }
        return infoButton ?? self
    }

    // MARK: Menus (ADR-0005 decision 8)

    /// The **Treat as** menu: each delimiter, the one in use ticked.
    func treatAsMenu() -> NSMenu {
        let menu = NSMenu(title: StatusText.treatAs)
        let header = menu.addItem(withTitle: StatusText.treatAs, action: nil, keyEquivalent: "")
        header.isEnabled = false
        for delimiter in [Delimiter.comma, .semicolon, .tab, .pipe] {
            let item = menu.addItem(withTitle: StatusText.delimiter(delimiter), action: #selector(treatAsChosen(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = DelimiterBox(delimiter)
            item.state = shownStatus?.delimiter == delimiter ? .on : .off
        }
        return menu
    }

    /// The **Reopen with encoding** menu: each encoding the file's BOM
    /// allows, the one in use ticked.
    func reopenMenu() -> NSMenu {
        let menu = NSMenu(title: StatusText.reopenWithEncoding)
        let header = menu.addItem(withTitle: StatusText.reopenWithEncoding, action: nil, keyEquivalent: "")
        header.isEnabled = false
        for encoding in shownStatus?.encodingChoices ?? [] {
            let item = menu.addItem(withTitle: StatusText.encodingName(encoding), action: #selector(reopenChosen(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = EncodingBox(encoding)
            item.state = shownStatus?.encoding == encoding ? .on : .off
        }
        return menu
    }

    @objc private func menuClicked(_ sender: NSButton) {
        let menu = sender === delimiterButton ? treatAsMenu() : reopenMenu()
        menu.popUp(positioning: nil, at: NSPoint(x: 0, y: sender.bounds.height + 2), in: sender)
    }

    @objc private func treatAsChosen(_ sender: NSMenuItem) {
        guard let box = sender.representedObject as? DelimiterBox else { return }
        onTreatAs?(box.delimiter)
    }

    @objc private func reopenChosen(_ sender: NSMenuItem) {
        guard let box = sender.representedObject as? EncodingBox else { return }
        onReopen?(box.encoding)
    }

    @objc private func toggleHeader(_ sender: Any?) {
        onToggleHeader?()
    }

    @objc private func badgeClicked(_ sender: Any?) {
        onBadge?()
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor.windowBackgroundColor.setFill()
        dirtyRect.fill()
        NSColor.separatorColor.setFill()
        NSRect(x: dirtyRect.minX, y: 0, width: dirtyRect.width, height: 1).fill()
    }
}

/// A delimiter in a menu item's `representedObject`.
final class DelimiterBox: NSObject {
    let delimiter: Delimiter
    init(_ delimiter: Delimiter) { self.delimiter = delimiter }
}

/// An encoding in a menu item's `representedObject`.
final class EncodingBox: NSObject {
    let encoding: TextEncoding
    init(_ encoding: TextEncoding) { self.encoding = encoding }
}

/// A non-modal banner under the title bar (ADR-0002): an icon, a message,
/// an optional button and a dismiss button. The UTF-16 notice (mockup
/// 06a), the diagnostics banner (03a), the suggestions and the
/// removable-drive banners (task 1.7) all use it. The file's banners
/// (task 1.9) have a second, plain button: **Keep Editing**.
@MainActor
final class BannerView: NSView {
    enum Kind {
        case info
        case warning
    }

    /// A banner's height on one line.
    static let minimumHeight: CGFloat = 38

    private let kind: Kind
    private let label: NSTextField
    /// The banner's button, if it has one.
    let button: NSButton?
    /// A second, plain button after it, if it has one.
    let secondaryButton: NSButton?
    var onDismiss: (() -> Void)?

    /// `prominent` gives the button the primary, accent-coloured look
    /// (mockup 06a); otherwise it is a plain push button ("Details" in
    /// mockup 03a). `secondaryTitle` adds a plain button after it, which
    /// sends `secondaryAction`.
    init(
        kind: Kind,
        message: String,
        buttonTitle: String?,
        prominent: Bool = true,
        target: AnyObject?,
        action: Selector?,
        secondaryTitle: String? = nil,
        secondaryAction: Selector? = nil
    ) {
        self.kind = kind
        label = NSTextField(wrappingLabelWithString: message)
        button = buttonTitle.map { NSButton(title: $0, target: target, action: action) }
        secondaryButton = secondaryTitle.map { NSButton(title: $0, target: target, action: secondaryAction) }
        super.init(frame: .zero)
        let symbol = kind == .info ? "info.circle" : "exclamationmark.triangle"
        let icon = NSImageView(image: NSImage(systemSymbolName: symbol, accessibilityDescription: nil) ?? NSImage())
        icon.symbolConfiguration = .init(pointSize: 14, weight: .regular)
        icon.contentTintColor = tint
        label.font = .systemFont(ofSize: 13)
        label.textColor = kind == .info ? tint : .labelColor
        label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        var views: [NSView] = [icon, label]
        if let button {
            button.bezelStyle = .push
            if prominent {
                // The primary, accent-coloured look (mockup 06a), without
                // making it the default button: Return belongs to the grid.
                if #available(macOS 26.0, *) {
                    button.tintProminence = .primary
                } else {
                    button.bezelColor = .controlAccentColor
                }
            }
            views.append(button)
        }
        if let secondaryButton {
            secondaryButton.bezelStyle = .push
            views.append(secondaryButton)
        }
        let close = NSButton(
            image: NSImage(systemSymbolName: "xmark", accessibilityDescription: String(localized: "Dismiss", comment: "Banner's close button, for VoiceOver")) ?? NSImage(),
            target: nil,
            action: nil
        )
        close.isBordered = false
        close.contentTintColor = kind == .info ? tint : .secondaryLabelColor
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
            heightAnchor.constraint(greaterThanOrEqualToConstant: Self.minimumHeight),
        ])
        setAccessibilityElement(true)
        setAccessibilityRole(.group)
        setAccessibilityLabel(message)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    /// The banner's text.
    var message: String {
        get { label.stringValue }
        set {
            label.stringValue = newValue
            setAccessibilityLabel(newValue)
        }
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
        // The warning banner is the mockups' pale yellow (03a); the info
        // banner a pale blue (06a).
        (kind == .info ? tint.withAlphaComponent(0.08) : NSColor.systemYellow.withAlphaComponent(0.16)).setFill()
        bounds.fill(using: .sourceOver)
        NSColor.separatorColor.setFill()
        NSRect(x: 0, y: bounds.height - 1, width: bounds.width, height: 1).fill()
    }

    /// The dismiss button, as clicking it does.
    @objc func dismiss(_ sender: Any?) {
        onDismiss?()
    }
}
