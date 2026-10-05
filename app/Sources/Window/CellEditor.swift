import AppKit
import LealFFI
import os

/// A text view that keeps what is typed as typed (task 2.5.1): a cell's
/// value is what the user typed, byte for byte. None of AppKit's
/// substitutions (smart quotes and dashes, text replacement,
/// autocorrection), smart insert and delete, data and link detection or
/// completion. Both editors are one: the in-cell editor's field editor
/// (`CellEditorCell.fieldEditor(for:)`) and the inspector's text view.
/// Each stays off whatever turns it on (the user's defaults, a
/// Substitutions menu): the setters keep them off.
@MainActor
class LiteralTextView: NSTextView {
    /// The text checking that changes text, or adds links to it. Spelling
    /// and grammar checking only mark text, and stay as the user has them.
    static let changingChecks: NSTextCheckingTypes = NSTextCheckingResult.CheckingType([
        .quote, .dash, .replacement, .correction, .link, .date, .address, .phoneNumber, .transitInformation,
    ]).rawValue

    /// Turns every substitution off: call once it is made.
    func keepTextAsTyped() {
        isAutomaticQuoteSubstitutionEnabled = false
        isAutomaticDashSubstitutionEnabled = false
        isAutomaticTextReplacementEnabled = false
        isAutomaticSpellingCorrectionEnabled = false
        smartInsertDeleteEnabled = false
        isAutomaticDataDetectionEnabled = false
        isAutomaticLinkDetectionEnabled = false
        isAutomaticTextCompletionEnabled = false
        enabledTextCheckingTypes = super.enabledTextCheckingTypes
        inlinePredictionType = .no
        if #available(macOS 15.0, *) {
            writingToolsBehavior = .none
        }
    }

    override var isAutomaticQuoteSubstitutionEnabled: Bool {
        get { false }
        set { super.isAutomaticQuoteSubstitutionEnabled = false }
    }

    override var isAutomaticDashSubstitutionEnabled: Bool {
        get { false }
        set { super.isAutomaticDashSubstitutionEnabled = false }
    }

    override var isAutomaticTextReplacementEnabled: Bool {
        get { false }
        set { super.isAutomaticTextReplacementEnabled = false }
    }

    override var isAutomaticSpellingCorrectionEnabled: Bool {
        get { false }
        set { super.isAutomaticSpellingCorrectionEnabled = false }
    }

    override var smartInsertDeleteEnabled: Bool {
        get { false }
        set { super.smartInsertDeleteEnabled = false }
    }

    override var isAutomaticDataDetectionEnabled: Bool {
        get { false }
        set { super.isAutomaticDataDetectionEnabled = false }
    }

    override var isAutomaticLinkDetectionEnabled: Bool {
        get { false }
        set { super.isAutomaticLinkDetectionEnabled = false }
    }

    override var isAutomaticTextCompletionEnabled: Bool {
        get { false }
        set { super.isAutomaticTextCompletionEnabled = false }
    }

    /// Inline predictions (macOS 14) would suggest text to complete, and a
    /// Tab or Return might take it.
    override var inlinePredictionType: NSTextInputTraitType {
        get { .no }
        set { super.inlinePredictionType = .no }
    }

    /// Writing Tools (macOS 15) rewrite text: not in a cell.
    @available(macOS 15.0, *)
    override var writingToolsBehavior: NSWritingToolsBehavior {
        get { .none }
        set { super.writingToolsBehavior = .none }
    }

    override var enabledTextCheckingTypes: NSTextCheckingTypes {
        get { super.enabledTextCheckingTypes & ~Self.changingChecks }
        set { super.enabledTextCheckingTypes = newValue & ~Self.changingChecks }
    }

    /// ⌘⌫ is the text's while either editor has the focus and is editing:
    /// it deletes to the start of the line, as in any text field, and never
    /// reaches Edit > Delete Row (task 2.5a), which would take it first.
    /// With the inspector focused but not editing it is Delete Row's, as
    /// the menu item shows. ⌘↩ and ⇧⌘↩ go
    /// on: in the in-cell editor they commit the edit, then insert a row
    /// below or duplicate the row (in the inspector ⌘↩ commits only:
    /// `InspectorTextView`). ⌘ (or ⇧⌘) and the keypad's Enter, which isn't
    /// the menu items' key, do the same there, as they do in the grid; in
    /// the inspector ⇧⌘ and the keypad's Enter duplicates too, rather than
    /// reaching the text.
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if window?.firstResponder === self, let key = GridView.rowCommandKey(event) {
            if key == .delete {
                // Not editing (the inspector showing a value): there is no
                // text to delete, and the Delete Row item is on, so the
                // key does what it says (DESIGN §4.2).
                guard isEditable else { return tryToPerform(DocumentViewController.action(.deleteRows), with: self) }
                interpretKeyEvents([event])
                return true
            }
            // In the inspector only ⇧⌘ and the keypad's Enter gets here:
            // it takes ⌘ and the keypad's Enter as commit (`isCommit`).
            if event.keyCode == Self.keypadEnter {
                let command = StructureCommand(key)
                return tryToPerform(DocumentViewController.action(command), with: self)
            }
        }
        return super.performKeyEquivalent(with: event)
    }

    /// ⇧↩ puts in a line break, as ⌥↩ does (`insertNewlineIgnoringFieldEditor`):
    /// in the in-cell editor, where Return commits, the file's own; in the
    /// inspector, as Return does. Not while an input method composes.
    override func keyDown(with event: NSEvent) {
        if Self.isShiftReturn(event), !hasMarkedText() {
            doCommand(by: #selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)))
            return
        }
        super.keyDown(with: event)
    }

    /// ⇧↩ (Return or Enter, with Shift and nothing else; Caps Lock on or
    /// off).
    static func isShiftReturn(_ event: NSEvent) -> Bool {
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask).subtracting([.numericPad, .function, .capsLock])
        return event.type == .keyDown && modifiers == .shift && (event.keyCode == 36 || event.keyCode == 76)
    }

    /// The keypad's Enter key.
    static let keypadEnter: UInt16 = 76
}

/// The in-cell editor (task 2.5.1, ADR-0001): an ordinary `NSTextField`
/// over the cell, in the grid's overlay (above the strips, following the
/// scroll; docs/tasks/2.0b.md, "For 2.5"), or over a header-row title. It
/// edits with a field editor of its own, which keeps text as typed
/// (`LiteralTextView`); the window's shared one, which the find bar uses,
/// is left as it is. Undo while typing, input methods and spell checking
/// come from the system.
@MainActor
final class CellEditorField: NSTextField {
    override class var cellClass: AnyClass? {
        get { CellEditorCell.self }
        set {}
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        isBordered = false
        isBezeled = false
        drawsBackground = true
        backgroundColor = .textBackgroundColor
        textColor = .labelColor
        focusRingType = .none
        usesSingleLineMode = false
        cell?.wraps = false
        cell?.isScrollable = true
        lineBreakMode = .byClipping
        wantsLayer = true
        layer?.borderWidth = 2
        updateBorder()
        setAccessibilityLabel(EditText.editorLabel)
        setAccessibilityHelp(EditText.editorKeys)
        toolTip = EditText.editorKeys
        // The accent colour can change while the editor is open.
        NotificationCenter.default.addObserver(
            self, selector: #selector(systemColorsChanged(_:)), name: NSColor.systemColorsDidChangeNotification, object: nil
        )
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    /// The active cell's ring colour, the accent (mockup 05b), resolved for
    /// the editor's appearance.
    private func updateBorder() {
        effectiveAppearance.performAsCurrentDrawingAppearance {
            layer?.borderColor = NSColor.controlAccentColor.cgColor
        }
    }

    @objc private func systemColorsChanged(_ notification: Notification) {
        updateBorder()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        updateBorder()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        updateBorder()
    }

    /// The height for `lines` lines of text, at least a row's.
    func height(lines: Int) -> CGFloat {
        let font = font ?? GridFonts.cell
        let line = (font.ascender - font.descender + font.leading).rounded(.up)
        return max(GridMetrics.rowHeight, CGFloat(lines) * line + 6)
    }
}

/// Lays the editor's text out where the grid draws a cell's: inset by the
/// cell padding, and a single line centred in the row. It edits with its
/// own field editor, which keeps text as typed.
final class CellEditorCell: NSTextFieldCell {
    /// The editor's field editor: made once, with every substitution off.
    private lazy var literalEditor: LiteralTextView = {
        let editor = LiteralTextView()
        editor.isFieldEditor = true
        editor.keepTextAsTyped()
        editor.setAccessibilityLabel(EditText.editorLabel)
        return editor
    }()

    override func fieldEditor(for controlView: NSView) -> NSTextView? {
        literalEditor
    }

    override func titleRect(forBounds rect: NSRect) -> NSRect {
        let font = font ?? GridFonts.cell
        let line = (font.ascender - font.descender + font.leading).rounded(.up)
        let inset = GridMetrics.cellPadding - 2
        var title = rect.insetBy(dx: inset, dy: 0)
        let lines = max(1, EditorLines.ranges(in: stringValue as NSString, upTo: CellEditController.maximumLines).count)
        let height = min(rect.height, CGFloat(lines) * line)
        title.origin.y = rect.minY + ((rect.height - height) / 2).rounded(.down)
        title.size.height = height
        return title
    }

    override func drawInterior(withFrame cellFrame: NSRect, in controlView: NSView) {
        super.drawInterior(withFrame: titleRect(forBounds: cellFrame), in: controlView)
    }

    override func edit(withFrame rect: NSRect, in controlView: NSView, editor textObj: NSText, delegate: Any?, event: NSEvent?) {
        super.edit(withFrame: titleRect(forBounds: rect), in: controlView, editor: textObj, delegate: delegate, event: event)
    }

    override func select(withFrame rect: NSRect, in controlView: NSView, editor textObj: NSText, delegate: Any?, start selStart: Int, length selLength: Int) {
        super.select(withFrame: titleRect(forBounds: rect), in: controlView, editor: textObj, delegate: delegate, start: selStart, length: selLength)
    }
}

/// A small bubble anchored to the cell being edited (ADR-0001, mockup
/// 05b): the invalid-bytes warning, a character the encoding can't hold,
/// or why a cell can't be edited. It sits in the grid's overlay with the
/// editor, under or over the cell.
@MainActor
final class EditCallout: NSView {
    static let width: CGFloat = 360
    static let arrow: CGFloat = 7
    static let inset: CGFloat = 10

    let icon = NSImageView()
    let label = NSTextField(wrappingLabelWithString: "")
    /// The arrow points up at a cell above the bubble (or down, at one
    /// below it).
    var pointsUp = true {
        didSet { needsLayout = true; needsDisplay = true }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        icon.image = NSImage(systemSymbolName: "exclamationmark.triangle.fill", accessibilityDescription: nil)
        icon.symbolConfiguration = .init(pointSize: 13, weight: .regular)
        icon.contentTintColor = .systemOrange
        label.font = .systemFont(ofSize: 12)
        label.textColor = .labelColor
        label.isSelectable = false
        for view in [icon, label] as [NSView] { addSubview(view) }
        wantsLayer = true
        shadow = NSShadow()
        layer?.shadowOpacity = 0.18
        layer?.shadowRadius = 6
        layer?.shadowOffset = CGSize(width: 0, height: -2)
        setAccessibilityElement(true)
        setAccessibilityRole(.staticText)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    override var isFlipped: Bool { true }

    var message: String {
        get { label.stringValue }
        set {
            label.stringValue = newValue
            setAccessibilityValue(newValue)
        }
    }

    /// The bubble's size for its message.
    var fittingHeight: CGFloat {
        let textWidth = Self.width - 2 * Self.inset - 24
        let size = label.cell?.cellSize(forBounds: NSRect(x: 0, y: 0, width: textWidth, height: 10_000)) ?? .zero
        return (size.height + 2 * Self.inset + Self.arrow).rounded(.up)
    }

    override func layout() {
        super.layout()
        let top = pointsUp ? Self.arrow : 0
        icon.frame = NSRect(x: Self.inset, y: top + Self.inset, width: 18, height: 16)
        label.frame = NSRect(
            x: Self.inset + 24,
            y: top + Self.inset - 1,
            width: bounds.width - 2 * Self.inset - 24,
            height: bounds.height - Self.arrow - 2 * Self.inset + 2
        )
    }

    override func draw(_ dirtyRect: NSRect) {
        let body = pointsUp
            ? NSRect(x: 0.5, y: Self.arrow + 0.5, width: bounds.width - 1, height: bounds.height - Self.arrow - 1)
            : NSRect(x: 0.5, y: 0.5, width: bounds.width - 1, height: bounds.height - Self.arrow - 1)
        let path = NSBezierPath(roundedRect: body, xRadius: 8, yRadius: 8)
        // The arrow, near the bubble's leading edge, towards the cell.
        let x: CGFloat = 40
        let arrow = NSBezierPath()
        if pointsUp {
            arrow.move(to: NSPoint(x: x - Self.arrow, y: body.minY + 1))
            arrow.line(to: NSPoint(x: x, y: 0.5))
            arrow.line(to: NSPoint(x: x + Self.arrow, y: body.minY + 1))
        } else {
            arrow.move(to: NSPoint(x: x - Self.arrow, y: body.maxY - 1))
            arrow.line(to: NSPoint(x: x, y: bounds.height - 0.5))
            arrow.line(to: NSPoint(x: x + Self.arrow, y: body.maxY - 1))
        }
        arrow.close()
        path.append(arrow)
        NSColor.controlBackgroundColor.setFill()
        path.fill()
        NSColor.separatorColor.setStroke()
        path.lineWidth = 1
        path.stroke()
    }
}

/// Runs the in-cell editor (task 2.5.1, DESIGN §4.2): Return, a
/// double-click or typing opens it on the active cell (or "Rename
/// Column…" on a header-row title); Return commits, Tab and Shift-Tab
/// commit and move along the row, Esc cancels, and leaving the editor (a
/// click elsewhere) commits. ⇧↩ or ⌥↩ puts a line break in the value
/// (the file's own, `DocumentModel.lineBreak`).
///
/// - It opens only where the core's `canEdit` allows; elsewhere the
///   callout says why.
/// - It starts from the core's full display value (ADR-0008 decision 3),
///   read off the main thread for a value the grid shows cut short, and
///   an untouched value commits as no edit. Text is compared scalar for
///   scalar (`isIdentical(to:)`), never with `==`, which would drop a
///   change between canonically equivalent forms.
/// - A cell holding bytes that aren't text in the file's encoding says so
///   in the callout (mockup 05b): committing replaces them.
/// - A character the encoding can't hold is named in the callout as it is
///   typed; Return then commits it anyway (Save will refuse it, and Save
///   As UTF-8 writes it; task 2.3). If it wasn't shown yet, the first
///   Return shows it and the second commits. A commit that doesn't wait
///   (leaving the editor, `commitOpenEdit`) commits, then names it.
/// - An edit the core refuses keeps the editor open with its text, and
///   the callout says why.
@MainActor
final class CellEditController: NSObject, NSTextFieldDelegate {
    let field = CellEditorField()
    let callout = EditCallout()

    private let model: DocumentModel
    private let grid: GridContainerView

    /// What is being edited.
    struct Session {
        let place: EditPlace
        /// The value it started from; `nil` when typing replaced it.
        let original: String?
        /// The user changed the text: an untouched value is never sent.
        var changed: Bool
        /// The cell has bytes that aren't text in the file's encoding.
        var invalidBytes: Bool
        /// The character the encoding can't hold, as last checked.
        var unencodable: UnencodableCharacter?
        /// The text whose unencodable character the callout has shown.
        var warned: String?
    }

    private(set) var session: Session?
    /// A long value being read before the editor opens, or (after typing
    /// opened it) read to see whether it holds invalid bytes.
    private(set) var loading: Task<Void, Never>? {
        didSet { if loading == nil { loadingPlace = nil } }
    }
    /// The cell whose value `loading` reads.
    private var loadingPlace: EditPlace?
    /// Ending the session: the field's end of editing is ours, not a
    /// commit.
    private var ending = false
    /// Why the last attempt to edit was refused, while its note shows.
    private(set) var shownRefusal: EditRefusal?
    /// Where the callout points, while it shows.
    private var calloutPlace: EditPlace?
    /// The callout shows a note with no editor open (a refusal, or a
    /// character named after its commit): it goes at the next key, click
    /// or selection change.
    private var noteShown = false
    /// The last committed edit's time from Return to the transaction that
    /// puts it on screen, in milliseconds (task 2.5.1, "Cell edit to
    /// screen"; DESIGN §1 < 16 ms).
    private(set) var lastEditToScreen: Double?
    /// Called with each committed edit's time to screen. Tests and the
    /// bench build wait on it.
    var onEditOnScreen: ((Double) -> Void)?

    /// Values longer than this many UTF-16 units aren't checked for the
    /// encoding as they are typed, only when committed.
    static let liveCheckLimit = 10_000
    /// The most lines the editor grows to show.
    static let maximumLines = 8

    init(model: DocumentModel, grid: GridContainerView) {
        self.model = model
        self.grid = grid
        super.init()
        field.delegate = self
    }

    var isEditing: Bool { session != nil }

    // MARK: Opening

    /// Opens the editor on `place`, from its full value; with `typing`, from
    /// that key's text instead, which replaces the value. An open editor
    /// commits first; if the core refuses that, it stays open (with the
    /// focus) and `place` isn't opened.
    func begin(_ place: EditPlace, typing: NSEvent? = nil) {
        if session != nil {
            guard commit(move: nil, refocus: false, confirmed: true) else {
                grid.window?.makeFirstResponder(field)
                return
            }
        }
        loading?.cancel()
        loading = nil
        dismissNote()
        if let refusal = model.editRefusal(place) {
            showRefusal(refusal, at: place)
            return
        }
        if model.isShownWhole(place) {
            guard let value = model.fullValue(place) else { return showRefusal(.notReadYet, at: place) }
            let invalid = model.hasInvalidBytes(place, value: value)
            open(place, value: typing == nil ? value : "", invalidBytes: invalid, typing: typing)
        } else if let typing {
            // Typing replaces a long value at once; whether it held
            // invalid bytes is read meanwhile, from its full value.
            open(place, value: "", invalidBytes: false, typing: typing)
            defer { loadingPlace = place }
            loading = Task { [weak self] in
                guard let self, let start = await model.fullValueInBackground(place) else { return }
                guard !Task.isCancelled, session?.place == place else { return }
                loading = nil
                session?.invalidBytes = start.invalid
                updateCallout()
            }
        } else {
            // A long value: read in full first (ADR-0008 decision 3).
            let selection = grid.activeCell
            defer { loadingPlace = place }
            loading = Task { [weak self] in
                guard let self else { return }
                let start = await model.fullValueInBackground(place)
                guard !Task.isCancelled else { return }
                loading = nil
                guard grid.activeCell == selection, session == nil else { return }
                guard let start else { return showRefusal(.notReadYet, at: place) }
                open(place, value: start.value, invalidBytes: start.invalid, typing: nil)
            }
        }
    }

    private func open(_ place: EditPlace, value: String, invalidBytes: Bool, typing: NSEvent?) {
        session = Session(place: place, original: typing == nil ? value : nil, changed: typing != nil, invalidBytes: invalidBytes)
        let numeric: Bool = if case .cell = place { model.isNumeric(column: place.column) } else { false }
        field.font = if case .header = place { GridFonts.header } else { numeric ? GridFonts.number : GridFonts.cell }
        field.stringValue = value
        position(for: place)
        grid.window?.makeFirstResponder(field)
        if let editor = field.currentEditor() {
            if let typing {
                // The key goes to the editor, whose input context composes
                // it: a dead key or an input method's marked text included.
                editor.string = ""
                editor.keyDown(with: typing)
            } else {
                editor.selectedRange = NSRange(location: (value as NSString).length, length: 0)
            }
        }
        textChanged()
    }

    /// Puts the editor over `place`, as wide as its text (up to the
    /// visible area's edge) and as tall as its lines.
    private func position(for place: EditPlace) {
        switch place {
        case let .cell(cell):
            guard cell.column < grid.geometry.columnCount else { return }
            grid.overlay.show(field, at: fieldRect(cell))
        case let .header(column):
            grid.headerView.showEditor(field, column: column)
        }
    }

    /// The editor's frame over `cell`. Only the start of the text is
    /// measured (`EditorLines`): its first lines, each to a length well
    /// past the visible area's width, which caps the editor's anyway.
    private func fieldRect(_ cell: CellPosition) -> CGRect {
        var rect = grid.geometry.cellRect(row: cell.row, column: cell.column)
        let text = field.stringValue as NSString
        let lines = EditorLines.ranges(in: text, upTo: Self.maximumLines * 4)
        let font = field.font ?? GridFonts.cell
        let widest = lines.map { line in
            let start = NSRange(location: line.location, length: min(line.length, EditorLines.measuredLength))
            return (text.substring(with: start) as NSString).size(withAttributes: [.font: font]).width
        }.max() ?? 0
        let visible = grid.scrollView.contentView.bounds
        let wanted = (widest + 2 * GridMetrics.cellPadding + 8).rounded(.up)
        rect.size.width = max(rect.width, min(wanted, visible.maxX - rect.minX))
        rect.size.height = field.height(lines: min(lines.count, Self.maximumLines))
        return rect
    }

    /// The editor and the callout go where the cell is now (a column
    /// resized, the grid's widths changed).
    func relayout() {
        guard let session else { return }
        position(for: session.place)
        updateCallout()
    }

    // MARK: Editing

    func controlTextDidChange(_ notification: Notification) {
        guard session != nil else { return }
        session?.changed = true
        textChanged()
    }

    /// The text changed (or the editor opened): check it for the encoding,
    /// resize the editor, and update the callout.
    private func textChanged() {
        guard var session else { return }
        let text = field.stringValue
        if session.changed, text.utf16.count <= Self.liveCheckLimit {
            session.unencodable = model.unencodable(text)
            session.warned = session.unencodable == nil ? nil : text
        }
        self.session = session
        if case let .cell(cell) = session.place {
            grid.overlay.show(field, at: fieldRect(cell))
        }
        updateCallout()
    }

    private func updateCallout() {
        guard let session else { return }
        var parts: [String] = []
        if let bad = session.unencodable, field.stringValue.isIdentical(to: session.warned) {
            parts.append(EditText.unencodable(bad))
        }
        if session.invalidBytes {
            parts.append(EditText.invalidBytes(encoding: StatusText.encodingName(model.interpretation.encoding)))
        }
        guard !parts.isEmpty else {
            removeCallout()
            return
        }
        show(callout: parts.joined(separator: "\n\n"), at: session.place)
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        switch selector {
        case #selector(NSResponder.insertNewline(_:)):
            _ = commit(move: nil)
        case #selector(NSResponder.insertTab(_:)):
            _ = commit(move: .next)
        case #selector(NSResponder.insertBacktab(_:)):
            _ = commit(move: .previous)
        case #selector(NSResponder.cancelOperation(_:)):
            cancel()
        case #selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)):
            // ⌥↩ (and ⇧↩, `LiteralTextView.keyDown`): a line break, the
            // file's own.
            textView.insertText(model.lineBreak, replacementRange: textView.selectedRange())
        default:
            return false
        }
        return true
    }

    /// The editor lost the focus (a click elsewhere): commit, as a text
    /// field does, without waiting on the encoding check.
    func controlTextDidEndEditing(_ notification: Notification) {
        guard !ending, session != nil else { return }
        if !commit(move: nil, refocus: false, confirmed: true) {
            takeBackFocus()
        }
    }

    /// A commit the core refused when the focus left: the editor stays open
    /// with its text, so it gets the focus back (once this resignation is
    /// over), to commit again or cancel, rather than sit open without it.
    private func takeBackFocus() {
        Task { @MainActor [weak self] in
            guard let self, session != nil, let window = grid.window, field.currentEditor() == nil else { return }
            window.makeFirstResponder(field)
        }
    }

    // MARK: Ending

    /// Commits the editor's value and closes it, then moves (Tab). Returns
    /// whether it closed: not if the callout first had to name a character
    /// the encoding can't hold, nor if the core refused the edit, which the
    /// callout says (the editor stays open with its text).
    ///
    /// `confirmed` (leaving the editor, opening another, `commitOpenEdit`)
    /// doesn't wait on the encoding check: a value too long to have been
    /// checked as it was typed is checked, committed, and then the
    /// character is named.
    @discardableResult
    func commit(move: GridMove?, refocus: Bool = true, confirmed: Bool = false) -> Bool {
        guard var session else { return true }
        let value = field.stringValue
        // An untouched value is no edit (ADR-0008 decision 3): it is never
        // sent, so a long or multiline value can't be changed by the round
        // trip through the editor.
        let changed = session.changed && !value.isIdentical(to: session.original)
        var named: UnencodableCharacter?
        if changed, !value.isIdentical(to: session.warned) {
            if !confirmed, let bad = model.unencodable(value) {
                // Checked before committing (task 2.3): say so, and commit
                // on the next Return.
                session.unencodable = bad
                session.warned = value
                self.session = session
                updateCallout()
                return false
            }
            if confirmed, value.utf16.count > Self.liveCheckLimit {
                named = model.unencodable(value)
            }
        }
        let signpost = Signposts.editCommitted()
        let started = CACurrentMediaTime()
        let outcome: EditOutcome = changed ? model.setCell(session.place, to: value) : .unchanged
        if case let .refused(refusal) = outcome {
            // The typed text stays, to commit again or cancel.
            Signposts.editOnScreen(signpost)
            if confirmed { NSSound.beep() }
            show(callout: EditText.refusal(refusal), at: session.place)
            return false
        }
        end(refocus: refocus)
        if let move, case .cell = session.place { grid.move(move) }
        if let named {
            // Committed, as leaving it does: Save will refuse it.
            NSSound.beep()
            showNote(EditText.unencodable(named), at: session.place)
        }
        guard case .edited = outcome else {
            Signposts.editOnScreen(signpost)
            return true
        }
        // On screen once the transaction this edit's drawing is in has
        // been committed (the strips are drawn at that commit).
        CATransaction.setCompletionBlock { [weak self] in
            MainActor.assumeIsolated {
                Signposts.editOnScreen(signpost)
                let milliseconds = (CACurrentMediaTime() - started) * 1000
                self?.lastEditToScreen = milliseconds
                self?.onEditOnScreen?(milliseconds)
            }
        }
        return true
    }

    /// Commits an open edit as leaving the editor would (for Reload, Treat
    /// As, Reopen with Encoding, ⌘G, Go to Row and closing the window;
    /// `DocumentViewController.commitEditing`). Returns whether no editor
    /// is left open: one whose edit the core refused stays open.
    @discardableResult
    func commitOpenEdit() -> Bool {
        guard session != nil else {
            // A long value still being read: it doesn't open now.
            loading?.cancel()
            loading = nil
            return true
        }
        return commit(move: nil, refocus: grid.window?.firstResponder === field.currentEditor(), confirmed: true)
    }

    /// The values of grid rows `rows` changed (an undo or a redo, task
    /// 2.5.2): a long value being read for the editor from one of them is
    /// stale, so the read stops. An editor already open stays.
    func valuesChanged(rows: Range<Int>) {
        guard case let .cell(cell)? = loadingPlace, rows.contains(cell.row) else { return }
        loading?.cancel()
        loading = nil
    }

    /// Esc: the cell keeps its value (and its bytes).
    func cancel() {
        loading?.cancel()
        loading = nil
        guard session != nil else { return }
        end(refocus: true)
    }

    /// The file was read again, or failed, or the window closes: the
    /// editor closes, committing nothing.
    func abandon() {
        loading?.cancel()
        loading = nil
        dismissNote()
        guard session != nil else { return }
        end(refocus: grid.window?.firstResponder === field.currentEditor())
    }

    private func end(refocus: Bool) {
        ending = true
        defer { ending = false }
        loading?.cancel()
        loading = nil
        session = nil
        if refocus, let window = grid.window {
            window.makeFirstResponder(grid.gridView)
        } else {
            _ = field.abortEditing()
        }
        field.removeFromSuperview()
        removeCallout()
    }

    // MARK: The callout

    /// The note saying why the active cell can't be edited (or naming a
    /// character just committed) goes: the selection moved, or a key was
    /// pressed.
    func dismissNote() {
        guard noteShown else { return }
        shownRefusal = nil
        noteShown = false
        if session == nil { removeCallout() }
    }

    private func showRefusal(_ refusal: EditRefusal, at place: EditPlace) {
        NSSound.beep()
        showNote(EditText.refusal(refusal), at: place)
        shownRefusal = refusal
    }

    /// A note with no editor open, until `dismissNote`.
    private func showNote(_ message: String, at place: EditPlace) {
        show(callout: message, at: place)
        noteShown = true
    }

    private func removeCallout() {
        callout.removeFromSuperview()
        calloutPlace = nil
    }

    /// Shows the callout under `place` (over it near the bottom of the
    /// visible area), with its arrow at the cell.
    private func show(callout message: String, at place: EditPlace) {
        if callout.superview == nil || callout.message != message {
            // VoiceOver hears it once, as it appears or changes.
            NSAccessibility.post(
                element: grid.window ?? callout,
                notification: .announcementRequested,
                userInfo: [.announcement: message, .priority: NSAccessibilityPriorityLevel.high.rawValue]
            )
        }
        callout.message = message
        calloutPlace = place
        let height = callout.fittingHeight
        let visible = grid.scrollView.contentView.bounds
        var anchor: CGRect
        switch place {
        case let .cell(cell):
            anchor = grid.overlay.rect(of: field) ?? grid.geometry.cellRect(row: cell.row, column: cell.column)
            if field.superview == nil { anchor = grid.geometry.cellRect(row: cell.row, column: cell.column) }
        case let .header(column):
            // Under the header: the top of the visible area.
            let title = grid.geometry.cellRect(row: 0, column: column)
            anchor = CGRect(x: title.minX, y: visible.minY - 1, width: title.width, height: 1)
        }
        let below = anchor.maxY + height <= visible.maxY || anchor.minY - height < visible.minY
        callout.pointsUp = below
        let x = min(anchor.minX, max(visible.minX, visible.maxX - EditCallout.width))
        let y = below ? anchor.maxY : anchor.minY - height
        grid.overlay.show(callout, at: CGRect(x: x, y: y, width: EditCallout.width, height: height))
        callout.needsLayout = true
        callout.needsDisplay = true
    }

    /// The grid scrolled: the callout chooses again whether it goes under
    /// or over its cell (a header editor's stays under the header).
    func gridScrolled() {
        guard let place = calloutPlace, callout.superview != nil else { return }
        show(callout: callout.message, at: place)
    }
}

/// The start of a value's lines, for sizing the in-cell editor (task
/// 2.5.1) without going through all of a long value on each keystroke:
/// line breaks are looked for in its first `scannedLength` UTF-16 units,
/// and each line is measured to its first `measuredLength`, well past the
/// width of any screen (the editor is no wider than the visible area).
/// CRLF is one line break; so are LF, CR and the others `.newlines` has.
enum EditorLines {
    static let scannedLength = 100_000
    static let measuredLength = 1_000

    /// The ranges of `text`'s first `limit` lines (at least one).
    static func ranges(in text: NSString, upTo limit: Int) -> [NSRange] {
        let end = min(text.length, scannedLength)
        var lines: [NSRange] = []
        var start = 0
        while lines.count < limit {
            let found = text.rangeOfCharacter(from: .newlines, options: [], range: NSRange(location: start, length: end - start))
            guard found.location != NSNotFound else {
                lines.append(NSRange(location: start, length: end - start))
                break
            }
            lines.append(NSRange(location: start, length: found.location - start))
            start = found.location + found.length
            if text.character(at: found.location) == 0x0D, start < end, text.character(at: start) == 0x0A {
                start += 1
            }
        }
        return lines
    }
}

/// The editing words (DESIGN §4.4: from the String Catalog).
enum EditText {
    static let editorLabel = String(localized: "Cell editor", comment: "VoiceOver name of the in-cell editor (task 2.5.1)")
    static let editorKeys = String(localized: "Return commits · ⇧↩ or ⌥↩ for a new line · Esc cancels", comment: "In-cell editor's tooltip and VoiceOver help: its keys (task 2.5a)")
    static let renameColumn = String(localized: "Rename Column…", comment: "Column header context menu: edit the header row's cell (task 2.5.1)")

    /// Mockup 05b's callout. The mockup names the bytes (0xE9); the core
    /// doesn't give them to the app yet, so this doesn't.
    static func invalidBytes(encoding: String) -> String {
        String(
            localized: "This cell holds bytes that aren’t valid \(encoding) (shown as �). Committing the edit replaces them with the text you type. Esc keeps the original bytes.",
            comment: "Callout on a cell being edited whose bytes aren't valid text in the file's encoding (mockup 05b); the encoding's name"
        )
    }

    /// A character the file's encoding can't hold (task 2.3). Retyping
    /// wouldn't help (Windows-1258 can't hold precomposed Vietnamese at
    /// all), so it points to Save As UTF-8.
    static func unencodable(_ character: UnencodableCharacter) -> String {
        let encoding = StatusText.encodingName(character.encoding)
        return String(
            localized: "“\(character.character)” can’t be saved in \(encoding), so Save won’t be able to save this file. Save As UTF-8 can. Return keeps it.",
            comment: "Callout on a cell being edited: a character the file's encoding can't hold (task 2.3); the character, the encoding's name"
        )
    }

    static func refusal(_ refusal: EditRefusal) -> String {
        switch refusal {
        case .notReadYet:
            String(localized: "This row isn’t read yet. Try again once Leal has read it.", comment: "Callout: a cell that can't be edited yet (task 2.5.1)")
        case .afterUnterminatedQuote:
            String(localized: "This cell can’t be edited: a quote earlier in the row is never closed, so the rest of the row is inside it.", comment: "Callout: a cell after an unterminated quote can't be edited (ADR-0004 decision 8)")
        case .tooFarRight:
            String(localized: "This cell is too far past the end of its row to edit.", comment: "Callout: a cell too far right can't be edited")
        case .unreadable:
            String(localized: "This row can’t be read: its drive or share isn’t available.", comment: "Callout: a cell whose row can't be read can't be edited")
        // A cell edit never refuses `.tooManyRows` and the like: those
        // are Duplicate Row's, Paste's and Clear's (PasteText says them).
        case .noSuchRow, .noSuchColumn, .tooManyRows, .tooManyCells, .tooMuchText, .tooMuchReplaced, .pastLastRow, .pastLastColumn:
            String(localized: "This cell isn’t in the file.", comment: "Callout: a cell that isn't in the file can't be edited")
        case .valueChanged, .otherLineage:
            String(localized: "The cell changed meanwhile, so the edit wasn’t made.", comment: "Callout: an edit refused because the cell changed")
        case .stillReading, .saving:
            String(localized: "This cell can’t be edited right now. Try again in a moment.", comment: "Callout: a cell that can't be edited while the file is read or saved")
        }
    }
}
