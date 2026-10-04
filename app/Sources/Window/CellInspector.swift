import AppKit
import LealFFI

/// The cell inspector (ADR-0002 question 11, mockup 05a): a pane under the
/// grid, about 190 points tall, showing the active cell's whole value, with
/// its column, row and size above it. ⌘I shows and hides it.
///
/// It edits the value where the core allows (task 2.5.1): Return puts in a
/// line break, ⌘↩ commits and Esc cancels. ⌘↩ is the inspector's own, so
/// it never reaches the grid, where it will insert a row (task 2.5a).
@MainActor
final class CellInspectorView: NSView {
    static let height: CGFloat = 190
    static let headerHeight: CGFloat = 30

    let columnLabel = NSTextField(labelWithString: "")
    let rowLabel = NSTextField(labelWithString: "")
    let sizeLabel = NSTextField(labelWithString: "")
    let textView = InspectorTextView()
    private let scroll = NSScrollView()

    override init(frame: NSRect) {
        super.init(frame: frame)
        columnLabel.font = .systemFont(ofSize: 12, weight: .semibold)
        columnLabel.lineBreakMode = .byTruncatingTail
        rowLabel.font = .systemFont(ofSize: 12)
        rowLabel.textColor = .secondaryLabelColor
        sizeLabel.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
        sizeLabel.textColor = .secondaryLabelColor
        sizeLabel.alignment = .right
        sizeLabel.lineBreakMode = .byTruncatingHead
        sizeLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)

        textView.isEditable = false
        textView.isSelectable = true
        textView.isRichText = false
        // What is typed reaches the cell as typed: no smart quotes or
        // dashes, text replacement, autocorrection and the like.
        textView.keepTextAsTyped()
        textView.font = .systemFont(ofSize: 13)
        textView.textColor = .labelColor
        textView.drawsBackground = true
        textView.backgroundColor = .textBackgroundColor
        textView.textContainerInset = NSSize(width: 6, height: 8)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        // Lay out only what is on screen, so a long value shows at once.
        textView.layoutManager?.allowsNonContiguousLayout = true
        // Control characters (NUL, DEL, the C1 controls) are drawn as
        // visible glyphs, not at zero width, as the grid shows them as
        // symbols (`CellText`). The text itself is the value, unchanged
        // (phase 1 review, fid-3).
        textView.layoutManager?.showsControlCharacters = true
        textView.setAccessibilityLabel(InspectorText.valueLabel)
        scroll.documentView = textView
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = true
        scroll.backgroundColor = .textBackgroundColor
        scroll.borderType = .noBorder

        let header = NSStackView(views: [columnLabel, rowLabel, NSView(), sizeLabel])
        header.orientation = .horizontal
        header.spacing = 8
        header.alignment = .firstBaseline
        header.edgeInsets = NSEdgeInsets(top: 0, left: 10, bottom: 0, right: 10)
        for view in [header, scroll] as [NSView] {
            view.translatesAutoresizingMaskIntoConstraints = false
            addSubview(view)
        }
        // Not required: the pane may be squeezed below its header (more
        // banners than room) without a constraint conflict (phase 1
        // review, app-6 and app-7).
        let headerHeight = header.heightAnchor.constraint(equalToConstant: Self.headerHeight)
        headerHeight.priority = .init(999)
        NSLayoutConstraint.activate([
            header.topAnchor.constraint(equalTo: topAnchor),
            header.leadingAnchor.constraint(equalTo: leadingAnchor),
            header.trailingAnchor.constraint(equalTo: trailingAnchor),
            headerHeight,
            scroll.topAnchor.constraint(equalTo: header.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
        setAccessibilityRole(.group)
        setAccessibilityLabel(InspectorText.paneLabel)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    override var isFlipped: Bool { true }
    /// Since the macOS 14 SDK views don't clip to their bounds by default,
    /// and `draw(_:)`'s rectangle may reach past them; this view's fill
    /// must not cover the grid above it (1.6 notes, "Clipping").
    override var clipsToBounds: Bool {
        get { true }
        set {}
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor.windowBackgroundColor.setFill()
        dirtyRect.intersection(bounds).fill()
        NSColor.separatorColor.setFill()
        NSRect(x: dirtyRect.minX, y: 0, width: dirtyRect.width, height: 1).fill()
        NSRect(x: dirtyRect.minX, y: Self.headerHeight - 1, width: dirtyRect.width, height: 1).fill()
    }

    /// What the size label says when there is no warning.
    private var size = ""

    /// Shows a cell: its column's title, its row number (as the gutter
    /// shows it), and its value, or a note if there is none to show. With
    /// `editable`, the value can be edited (task 2.5.1), and the label says
    /// how to commit (mockup 05a).
    func show(column: String, row: Int?, content: InspectorContent, editable: Bool = false) {
        columnLabel.stringValue = column
        rowLabel.stringValue = row.map(InspectorText.row) ?? ""
        switch content {
        case let .value(value):
            textView.string = value.text
            textView.textColor = .labelColor
            size = InspectorText.size(value)
            if editable { size += " · " + InspectorText.editingHint }
        case let .note(note):
            textView.string = note
            textView.textColor = .secondaryLabelColor
            size = ""
        }
        textView.isEditable = editable
        showWarning(nil)
        textView.scroll(.zero)
    }

    /// The whole of a long value, read for editing in place of its start.
    func showWhole(_ value: String) {
        let selection = textView.selectedRanges
        textView.string = value
        textView.selectedRanges = selection
        size = InspectorText.editingHint
        showWarning(nil)
    }

    /// A warning in place of the size (a character the encoding can't
    /// hold, a refused edit), or the size again with `nil`.
    func showWarning(_ warning: String?) {
        sizeLabel.stringValue = warning ?? size
        sizeLabel.textColor = warning == nil ? .secondaryLabelColor : .systemOrange
        sizeLabel.toolTip = warning
    }
}

/// What the inspector shows for a cell.
enum InspectorContent: Equatable {
    case value(CellValue)
    /// No value to show, and why.
    case note(String)
}

/// The inspector's text view, which keeps text as typed
/// (`LiteralTextView`). ⌘↩ is the inspector's (it commits an edit from
/// task 2.5, ADR-0002 question 11): consumed here, so it never reaches the
/// grid's Insert Row (task 2.5a). Return (and ⌥↩) put in the file's own
/// line break.
@MainActor
final class InspectorTextView: LiteralTextView {
    /// ⌘↩ was pressed: commits the edit.
    var onCommit: (() -> Void)?
    /// Esc was pressed: cancels it.
    var onCancel: (() -> Void)?
    /// The line break Return puts in (`DocumentModel.lineBreak`).
    var lineBreak = "\n"

    override func insertNewline(_ sender: Any?) {
        guard isEditable else { return super.insertNewline(sender) }
        insertText(lineBreak, replacementRange: selectedRange())
    }

    override func insertNewlineIgnoringFieldEditor(_ sender: Any?) {
        insertNewline(sender)
    }

    override func cancelOperation(_ sender: Any?) {
        guard isEditable, let onCancel else { return super.cancelOperation(sender) }
        onCancel()
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if Self.isCommit(event), window?.firstResponder === self {
            onCommit?()
            return true
        }
        return super.performKeyEquivalent(with: event)
    }

    /// ⌘↩ (Return or Enter, with ⌘ and nothing else).
    static func isCommit(_ event: NSEvent) -> Bool {
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask).subtracting([.numericPad, .function])
        return event.type == .keyDown && modifiers == .command && (event.keyCode == 36 || event.keyCode == 76)
    }
}

/// The inspector's words (DESIGN §4.4: from the String Catalog).
enum InspectorText {
    static let paneLabel = String(localized: "Cell inspector", comment: "VoiceOver name of the cell inspector pane (mockup 05a)")
    static let valueLabel = String(localized: "Cell value", comment: "VoiceOver name of the cell inspector's text")
    static let noCell = String(localized: "No cell selected.", comment: "Cell inspector with no active cell")
    static let notRead = String(localized: "This row isn’t read yet.", comment: "Cell inspector on a row past the indexed region")
    static let missing = String(localized: "This row has no field here: it is shorter than the others.", comment: "Cell inspector on a short (ragged) row's missing cell")

    static let editingHint = String(localized: "⌘↩ commits · Esc cancels", comment: "Cell inspector: how to commit or cancel an edit (mockup 05a)")
    static let loadingWhole = String(localized: "Reading the whole value to edit it…", comment: "Cell inspector: a long value is read in full before it can be edited (ADR-0008 decision 3)")

    static func row(_ row: Int) -> String {
        String(localized: "Row \(row.formatted())", comment: "Cell inspector: the active cell's row number, as the gutter shows it (mockup 05a)")
    }

    /// "3 lines · 62 characters" (mockup 05a), with a note for a value cut
    /// short or with invalid bytes.
    static func size(_ value: CellValue) -> String {
        let lines = Int(value.lines)
        let characters = Int(value.characters)
        var parts = [
            lines == 1
                ? String(localized: "1 line", comment: "Cell inspector: the value has one line")
                : String(localized: "\(lines.formatted()) lines", comment: "Cell inspector: how many lines the value has"),
            characters == 1
                ? String(localized: "1 character", comment: "Cell inspector: the value is one character long")
                : String(localized: "\(characters.formatted()) characters", comment: "Cell inspector: how long the value is"),
        ]
        if value.truncated {
            let shown = Int(DocumentModel.inspectorMaxCharacters).formatted()
            parts.append(String(localized: "value truncated: the first \(shown) shown", comment: "Cell inspector: a very long value is shown only in part; how many characters are shown"))
        }
        if value.invalid {
            parts.append(String(localized: "invalid bytes shown as �", comment: "Cell inspector: the value has bytes that aren't valid text (DESIGN §3.5)"))
        }
        return parts.joined(separator: " · ")
    }
}
