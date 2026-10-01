import AppKit

/// The find bar under the title bar (ADR-0002 question 9, mockup 04a): the
/// search field, the match count, Previous and Next, and Done. No Replace
/// in v1.
///
/// Typing searches as you go (the field's incremental search); Return is
/// Next and Shift-Return Previous; Esc and Done close the bar. The field's
/// magnifying-glass menu has **Ignore Case**, on by default (DESIGN §4.2).
@MainActor
final class FindBarView: NSView, NSSearchFieldDelegate {
    static let height: CGFloat = 36

    let field = NSSearchField()
    let count = NSTextField(labelWithString: "")
    let arrows = NSSegmentedControl()
    let done = NSButton()

    /// The query changed (as the user types).
    var onQuery: ((_ text: String) -> Void)?
    /// Next (`true`) or Previous.
    var onStep: ((_ forward: Bool) -> Void)?
    /// Done, or Esc.
    var onDone: (() -> Void)?
    /// Ignore Case was turned on or off.
    var onIgnoreCase: ((_ ignoreCase: Bool) -> Void)?

    private(set) var ignoresCase = true
    private let ignoreCaseItem = NSMenuItem()

    override init(frame: NSRect) {
        super.init(frame: frame)
        field.placeholderString = FindText.placeholder
        field.sendsSearchStringImmediately = false
        field.sendsWholeSearchString = false
        field.delegate = self
        field.target = self
        field.action = #selector(queryChanged(_:))
        field.setAccessibilityLabel(FindText.fieldLabel)
        let menu = NSMenu(title: FindText.optionsMenu)
        ignoreCaseItem.title = FindText.ignoreCase
        ignoreCaseItem.target = self
        ignoreCaseItem.action = #selector(toggleIgnoreCase(_:))
        ignoreCaseItem.state = .on
        menu.addItem(ignoreCaseItem)
        field.searchMenuTemplate = menu

        count.font = .monospacedDigitSystemFont(ofSize: 13, weight: .regular)
        count.textColor = .secondaryLabelColor
        count.lineBreakMode = .byTruncatingTail
        count.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)

        arrows.segmentCount = 2
        arrows.trackingMode = .momentary
        arrows.segmentStyle = .rounded
        arrows.setImage(NSImage(systemSymbolName: "chevron.left", accessibilityDescription: FindText.previous), forSegment: 0)
        arrows.setImage(NSImage(systemSymbolName: "chevron.right", accessibilityDescription: FindText.next), forSegment: 1)
        arrows.setToolTip(FindText.previousTip, forSegment: 0)
        arrows.setToolTip(FindText.nextTip, forSegment: 1)
        arrows.setWidth(28, forSegment: 0)
        arrows.setWidth(28, forSegment: 1)
        arrows.target = self
        arrows.action = #selector(arrowClicked(_:))

        done.title = FindText.done
        done.bezelStyle = .push
        done.target = self
        done.action = #selector(doneClicked(_:))

        let stack = NSStackView(views: [field, count, arrows, NSView(), done])
        stack.orientation = .horizontal
        stack.spacing = 10
        stack.alignment = .centerY
        stack.edgeInsets = NSEdgeInsets(top: 0, left: 10, bottom: 0, right: 10)
        stack.setCustomSpacing(16, after: field)
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor),
            stack.topAnchor.constraint(equalTo: topAnchor),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor),
            field.widthAnchor.constraint(equalToConstant: 320),
        ])
        setAccessibilityRole(.group)
        setAccessibilityLabel(FindText.barLabel)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    override var isFlipped: Bool { true }
    /// Since the macOS 14 SDK views don't clip to their bounds by default,
    /// and `draw(_:)`'s rectangle may reach past them; this view's fill
    /// must not cover the views around it (1.6 notes, "Clipping").
    override var clipsToBounds: Bool {
        get { true }
        set {}
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor.windowBackgroundColor.setFill()
        dirtyRect.intersection(bounds).fill()
        NSColor.separatorColor.setFill()
        NSRect(x: dirtyRect.minX, y: bounds.maxY - 1, width: dirtyRect.width, height: 1).fill()
    }

    /// Shows the count ("6 of 2,318"), and turns the arrows off when there
    /// is nothing to step to.
    func show(count text: String, canStep: Bool) {
        count.stringValue = text
        arrows.isEnabled = canStep
    }

    @objc private func queryChanged(_ sender: NSSearchField) {
        onQuery?(sender.stringValue)
    }

    @objc private func arrowClicked(_ sender: NSSegmentedControl) {
        onStep?(sender.selectedSegment == 1)
    }

    @objc private func doneClicked(_ sender: Any?) {
        onDone?()
    }

    @objc private func toggleIgnoreCase(_ sender: NSMenuItem) {
        ignoresCase.toggle()
        ignoreCaseItem.state = ignoresCase ? .on : .off
        // The field copies its template: refresh its menu's copy too.
        field.searchMenuTemplate = ignoreCaseItem.menu
        onIgnoreCase?(ignoresCase)
    }

    /// Return is Next, Shift-Return Previous, Esc closes the bar.
    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        switch commandSelector {
        case #selector(NSResponder.insertNewline(_:)):
            onStep?(!(NSApp.currentEvent?.modifierFlags.contains(.shift) ?? false))
            return true
        case #selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)):
            onStep?(false)
            return true
        case #selector(NSResponder.cancelOperation(_:)):
            onDone?()
            return true
        default:
            return false
        }
    }
}

/// The find bar's and Go to Row's words (DESIGN §4.4: from the String
/// Catalog).
enum FindText {
    static let placeholder = String(localized: "Find", comment: "Placeholder of the find bar's search field (mockup 04a)")
    static let fieldLabel = String(localized: "Find in file", comment: "VoiceOver name of the find bar's search field")
    static let barLabel = String(localized: "Find bar", comment: "VoiceOver name of the find bar")
    static let optionsMenu = String(localized: "Find options", comment: "The find bar's search field menu")
    static let ignoreCase = String(localized: "Ignore Case", comment: "Find bar option, on by default (DESIGN §4.2)")
    static let previous = String(localized: "Previous", comment: "The find bar's previous-match arrow, for VoiceOver")
    static let next = String(localized: "Next", comment: "The find bar's next-match arrow, for VoiceOver")
    static let previousTip = String(localized: "Previous match (⇧⌘G)", comment: "Tooltip of the find bar's previous arrow")
    static let nextTip = String(localized: "Next match (⌘G)", comment: "Tooltip of the find bar's next arrow")
    static let done = String(localized: "Done", comment: "Button that closes the find bar (mockup 04a)")

    /// The count beside the field: "6 of 2,318" when a match is selected,
    /// "2,318 matches" otherwise. While the search is still running the
    /// total ends with "+" ("6 of 2,318+"), as the diagnostics popover's
    /// counts do (task 1.7).
    static func count(current: UInt64?, total: UInt64, complete: Bool, searching: Bool) -> String {
        let shown = complete ? total.formatted() : total.formatted() + "+"
        if let current {
            return String(localized: "\(current.formatted()) of \(shown)", comment: "Find bar count: the selected match's number, then how many there are (mockup 04a); a total ending in + is still growing")
        }
        if total == 0 {
            return searching
                ? String(localized: "Searching…", comment: "Find bar count while a search has found nothing yet")
                : String(localized: "No matches", comment: "Find bar count when nothing matches")
        }
        if complete, total == 1 {
            return String(localized: "1 match", comment: "Find bar count: one match, none selected")
        }
        return String(localized: "\(shown) matches", comment: "Find bar count: how many matches, none selected; a total ending in + is still growing")
    }

    /// VoiceOver's announcement when Next or Previous goes round the end.
    static func wrapped(forward: Bool, count: String) -> String {
        forward
            ? String(localized: "Wrapped to the first match. \(count)", comment: "VoiceOver: Next went past the last match to the first; then the find bar's count")
            : String(localized: "Wrapped to the last match. \(count)", comment: "VoiceOver: Previous went before the first match to the last; then the find bar's count")
    }

    static func largeCopyTitle(_ size: String) -> String {
        String(localized: "Copy about \(size)?", comment: "Sheet before a very large Copy; the size, like “1.2 GB”")
    }
    static let largeCopyMessage = String(
        localized: "The selected cells make a lot of text. Copying them takes a while and puts all of it on the clipboard.",
        comment: "Sheet before a very large Copy"
    )
    static let copy = String(localized: "Copy", comment: "Button of the sheet before a very large Copy")

    static let goToRowTitle = String(localized: "Go to Row", comment: "Title of the Go to Row sheet (⌘L)")
    static func goToRowMessage(rows: Int, exact: Bool) -> String {
        exact
            ? String(localized: "Enter a row number from 1 to \(rows.formatted()).", comment: "Go to Row sheet; the file's row count")
            : String(localized: "Enter a row number. The file has about \(rows.formatted()) rows; it is still being read.", comment: "Go to Row sheet while indexing; the estimated row count")
    }
    static let goToRowField = String(localized: "Row number", comment: "Placeholder and VoiceOver name of the Go to Row sheet's field")
    static let go = String(localized: "Go", comment: "Go to Row sheet's button")
    static let cancel = String(localized: "Cancel", comment: "Go to Row sheet's cancel button")

    /// The row number typed in Go to Row, as the gutter shows it (from 1):
    /// digits, with any grouping separators or spaces the user typed.
    /// `nil` for anything else, or 0.
    static func rowNumber(from text: String) -> Int? {
        let digits = text.filter { !$0.isWhitespace && $0 != "," && $0 != "." && $0 != "\u{2019}" && $0 != "'" && $0 != "\u{202F}" }
        guard !digits.isEmpty, digits.allSatisfy(\.isASCII), let number = Int(digits), number > 0 else { return nil }
        return number
    }
}

/// The brief sign over the grid when Next or Previous goes round the end
/// of the file, as Safari shows one (1.8 review): a looped arrow on a dark
/// rounded square, which fades out. With Reduce Motion it doesn't fade; it
/// goes. It is decoration for sighted users: VoiceOver hears an
/// announcement instead.
@MainActor
final class WrapIndicatorView: NSView {
    static let size: CGFloat = 110
    /// How long it shows before it goes.
    static let duration: Duration = .milliseconds(700)

    private let image = NSImageView()
    private var hiding: Task<Void, Never>?
    /// How many times it has shown, for tests.
    private(set) var shown = 0

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.cornerRadius = 18
        image.image = NSImage(systemSymbolName: "arrow.triangle.2.circlepath", accessibilityDescription: nil)
        image.symbolConfiguration = .init(pointSize: 52, weight: .medium)
        image.contentTintColor = .white
        image.translatesAutoresizingMaskIntoConstraints = false
        addSubview(image)
        NSLayoutConstraint.activate([
            image.centerXAnchor.constraint(equalTo: centerXAnchor),
            image.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        isHidden = true
        setAccessibilityElement(false)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    override var wantsUpdateLayer: Bool { true }

    override func updateLayer() {
        layer?.backgroundColor = NSColor.black.withAlphaComponent(0.55).cgColor
    }

    /// Shows the sign in the middle of `grid`, then lets it go.
    func show(over grid: NSView) {
        shown += 1
        hiding?.cancel()
        alphaValue = 1
        isHidden = false
        hiding = Task { [weak self] in
            try? await Task.sleep(for: Self.duration)
            guard let self, !Task.isCancelled else { return }
            if NSWorkspace.shared.accessibilityDisplayShouldReduceMotion {
                isHidden = true
                return
            }
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.25
                self.animator().alphaValue = 0
            } completionHandler: {
                MainActor.assumeIsolated {
                    if self.alphaValue == 0 { self.isHidden = true }
                }
            }
        }
    }

    func dismiss() {
        hiding?.cancel()
        isHidden = true
    }
}
