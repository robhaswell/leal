import AppKit
import LealFFI

/// The details popover (DESIGN §3.5, mockup 03b): one entry per kind found,
/// warnings and errors first with their count and **Previous**/**Next**,
/// then the info-level kinds without navigation. The entry navigated last
/// is highlighted. Everything it shows comes from the core's report.
@MainActor
final class DiagnosticsDetailsController: NSViewController {
    /// The user asked for a kind's previous (`false`) or next (`true`)
    /// occurrence.
    var onNavigate: ((DiagnosticKind, _ forward: Bool) -> Void)?

    private let titleLabel = NSTextField(labelWithString: "")
    private let stack = NSStackView()
    /// The entries, top to bottom, for tests.
    private(set) var entries: [DiagnosticsEntryView] = []

    override func loadView() {
        titleLabel.font = .systemFont(ofSize: 13, weight: .semibold)
        titleLabel.lineBreakMode = .byTruncatingMiddle
        stack.orientation = .vertical
        stack.alignment = .width
        stack.spacing = 2
        let content = NSStackView(views: [titleLabel, stack])
        content.orientation = .vertical
        content.alignment = .leading
        content.spacing = 8
        content.edgeInsets = NSEdgeInsets(top: 14, left: 8, bottom: 10, right: 8)
        content.translatesAutoresizingMaskIntoConstraints = false
        let root = NSView()
        root.addSubview(content)
        NSLayoutConstraint.activate([
            content.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            content.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            content.topAnchor.constraint(equalTo: root.topAnchor),
            content.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            root.widthAnchor.constraint(equalToConstant: 440),
            titleLabel.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 14),
            stack.widthAnchor.constraint(equalTo: content.widthAnchor, constant: -16),
        ])
        view = root
    }

    /// Shows `report`'s kinds, with `navigation`'s positions.
    func show(
        report: DiagnosticsReport,
        navigation: KindNavigation,
        fileName: String,
        headerRows: Int,
        lineEnding: LineEnding?,
        encoding: TextEncoding
    ) {
        _ = view
        titleLabel.stringValue = DiagnosticsText.popoverTitle(fileName: fileName)
        let warnings = report.diagnostics.filter { $0.severity != .info }
        let info = report.diagnostics.filter { $0.severity == .info }
        let kinds = (warnings + info).map(\.kind)
        if entries.map(\.kind) != kinds {
            for view in stack.arrangedSubviews { view.removeFromSuperview() }
            entries = []
            for (index, diagnostic) in (warnings + info).enumerated() {
                if index == warnings.count, !warnings.isEmpty {
                    let separator = NSBox()
                    separator.boxType = .separator
                    stack.addArrangedSubview(separator)
                }
                let entry = DiagnosticsEntryView(kind: diagnostic.kind, severity: diagnostic.severity)
                entry.onNavigate = { [weak self] kind, forward in self?.onNavigate?(kind, forward) }
                stack.addArrangedSubview(entry)
                entries.append(entry)
            }
        }
        for (entry, diagnostic) in zip(entries, warnings + info) {
            entry.update(
                title: DiagnosticsText.title(diagnostic.kind, encoding: encoding),
                subtitle: DiagnosticsText.subtitle(
                    diagnostic,
                    rowsWithCommonFieldCount: report.rowsWithCommonFieldCount,
                    headerRows: headerRows,
                    dominantLineEnding: lineEnding,
                    encoding: encoding
                ),
                position: diagnostic.severity == .info
                    ? nil
                    : DiagnosticsText.position(navigation.position(of: diagnostic), count: diagnostic.count),
                canGoBack: navigation.canGoBack(diagnostic),
                canGoForward: navigation.canGoForward(diagnostic),
                highlighted: navigation.current == diagnostic.kind
            )
        }
    }

    func entry(_ kind: DiagnosticKind) -> DiagnosticsEntryView? {
        entries.first { $0.kind == kind }
    }
}

/// One kind in the details popover: an icon, its name and what was found,
/// and for warnings and errors "1 of 3" with **Previous** and **Next**.
@MainActor
final class DiagnosticsEntryView: NSView {
    let kind: DiagnosticKind
    var onNavigate: ((DiagnosticKind, _ forward: Bool) -> Void)?

    private let titleLabel = NSTextField(labelWithString: "")
    private let subtitleLabel = NSTextField(wrappingLabelWithString: "")
    let positionLabel = NSTextField(labelWithString: "")
    let arrows = NSSegmentedControl()
    private var highlighted = false

    init(kind: DiagnosticKind, severity: Severity) {
        self.kind = kind
        super.init(frame: .zero)
        let symbol: String
        let tint: NSColor
        switch severity {
        case .error: (symbol, tint) = ("xmark.octagon", .systemRed)
        case .warning: (symbol, tint) = ("exclamationmark.triangle", .systemOrange)
        case .info: (symbol, tint) = ("info.circle", .secondaryLabelColor)
        }
        let icon = NSImageView(image: NSImage(systemSymbolName: symbol, accessibilityDescription: nil) ?? NSImage())
        icon.symbolConfiguration = .init(pointSize: 14, weight: .regular)
        icon.contentTintColor = tint
        icon.setContentHuggingPriority(.required, for: .horizontal)
        titleLabel.font = .systemFont(ofSize: 13, weight: .semibold)
        subtitleLabel.font = .systemFont(ofSize: 11)
        subtitleLabel.textColor = .secondaryLabelColor
        subtitleLabel.preferredMaxLayoutWidth = severity == .info ? 360 : 250
        subtitleLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let text = NSStackView(views: [titleLabel, subtitleLabel])
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 1
        let spacer = NSView()
        spacer.setContentHuggingPriority(.init(1), for: .horizontal)
        var views: [NSView] = [icon, text, spacer]
        if severity != .info {
            positionLabel.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
            positionLabel.textColor = .secondaryLabelColor
            positionLabel.alignment = .right
            positionLabel.setContentHuggingPriority(.required, for: .horizontal)
            arrows.segmentCount = 2
            arrows.trackingMode = .momentary
            arrows.segmentStyle = .rounded
            arrows.controlSize = .small
            // The images and their VoiceOver names come with the kind's
            // name, in `update`.
            arrows.target = self
            arrows.action = #selector(arrowClicked(_:))
            arrows.setContentHuggingPriority(.required, for: .horizontal)
            views += [positionLabel, arrows]
        }
        let row = NSStackView(views: views)
        row.orientation = .horizontal
        row.alignment = .centerY
        row.spacing = 10
        row.edgeInsets = NSEdgeInsets(top: 6, left: 6, bottom: 6, right: 6)
        row.translatesAutoresizingMaskIntoConstraints = false
        addSubview(row)
        NSLayoutConstraint.activate([
            row.leadingAnchor.constraint(equalTo: leadingAnchor),
            row.trailingAnchor.constraint(equalTo: trailingAnchor),
            row.topAnchor.constraint(equalTo: topAnchor),
            row.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
        wantsLayer = true
        layer?.cornerRadius = 6
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    var title: String { titleLabel.stringValue }
    /// The arrows' VoiceOver names, for tests.
    var arrowNames: [String] {
        (0..<arrows.segmentCount).map { arrows.image(forSegment: $0)?.accessibilityDescription ?? "" }
    }
    var subtitle: String { subtitleLabel.stringValue }
    var isHighlighted: Bool { highlighted }

    func update(title: String, subtitle: String, position: String?, canGoBack: Bool, canGoForward: Bool, highlighted: Bool) {
        if title != titleLabel.stringValue, arrows.segmentCount == 2 {
            // VoiceOver says which kind the arrows step through:
            // "Previous: Ragged rows".
            let previous = DiagnosticsText.previous(title)
            let next = DiagnosticsText.next(title)
            arrows.setImage(NSImage(systemSymbolName: "chevron.left", accessibilityDescription: previous), forSegment: 0)
            arrows.setImage(NSImage(systemSymbolName: "chevron.right", accessibilityDescription: next), forSegment: 1)
            arrows.setToolTip(previous, forSegment: 0)
            arrows.setToolTip(next, forSegment: 1)
        }
        titleLabel.stringValue = title
        subtitleLabel.stringValue = subtitle
        positionLabel.stringValue = position ?? ""
        arrows.setEnabled(canGoBack, forSegment: 0)
        arrows.setEnabled(canGoForward, forSegment: 1)
        self.highlighted = highlighted
        needsDisplay = true
        updateLayer()
    }

    override var wantsUpdateLayer: Bool { true }

    override func updateLayer() {
        layer?.backgroundColor = highlighted
            ? NSColor.controlAccentColor.withAlphaComponent(0.12).cgColor
            : NSColor.clear.cgColor
    }

    /// **Previous** (`false`) or **Next** (`true`), as the arrows do.
    func navigate(forward: Bool) {
        onNavigate?(kind, forward)
    }

    @objc private func arrowClicked(_ sender: NSSegmentedControl) {
        navigate(forward: sender.selectedSegment == 1)
    }
}
