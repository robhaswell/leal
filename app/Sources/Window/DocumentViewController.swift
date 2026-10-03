import AppKit
import LealFFI
import os

/// A document window's content (DESIGN §4.1): banners at the top, the grid,
/// and the status bar. It binds the grid to the `DocumentModel` and passes
/// the user's scrolling and typing to the core's scheduler.
///
/// The banners, top to bottom: the file's own (`FileBanner`: it changed
/// while read from a drive that can't make a snapshot, it changed or was
/// deleted elsewhere, or its removable drive was disconnected; ADR-0006,
/// task 1.9), the UTF-16 notice (mockup 06a), the diagnostics banner (03a),
/// and the delimiter and encoding suggestions (DESIGN §3.2, ADR-0005
/// decision 4). Each stays until dismissed; reading the file again (Treat
/// As, Reopen with Encoding, the Header row toggle) starts all but the
/// file's afresh, and **Reload** starts every one afresh.
@MainActor
final class DocumentViewController: NSViewController, NSMenuItemValidation {
    let model: DocumentModel
    private let scheduler: Scheduler
    let grid = GridContainerView()
    let statusBar = StatusBarView()
    /// The banners, top to bottom.
    let banners = NSStackView()
    /// The document failed (DESIGN §3.9).
    var onFailure: (() -> Void)?
    /// **Reload**, through the `NSDocument` (`CSVDocument.reloadInBackground`),
    /// so it stays in step with the file shown. It opens the file off the
    /// main thread (task 2.0). Without one, the model reloads.
    var onReload: (() async throws -> Void)?
    /// A Reload, while it is under way.
    private(set) var reloading: Task<Void, Never>? {
        didSet { updateSaveAsUTF8Button() }
    }
    /// Whether the file is read-only (UTF-16) changed: the window's lock
    /// glyph follows it.
    var onReadOnlyChanged: ((Bool) -> Void)?
    /// What the lock glyph was last told.
    private var shownReadOnly: Bool?

    /// The file's banner (`FileBanner`), if one shows.
    private(set) var driveBanner: BannerView?
    private(set) var readOnlyBanner: BannerView?
    private(set) var diagnosticsBanner: BannerView?
    private(set) var delimiterBanner: BannerView?
    private(set) var encodingBanner: BannerView?
    /// What was dismissed, for the current reading.
    private var dismissed = Set<String>()
    /// The reading the banners are for.
    private var bannerGeneration: UInt64?

    /// The details popover (mockup 03b), while it is open.
    private(set) var detailsPopover: NSPopover?
    private(set) var details: DiagnosticsDetailsController?
    /// Where each kind's Previous and Next are.
    private(set) var navigation = KindNavigation()
    /// Navigations started, so a late answer is dropped.
    private var navigationCount = 0

    /// The find bar (mockup 04a), under the title bar, and its state.
    let findBar = FindBarView()
    let find: FindModel
    private var findBarHeight: NSLayoutConstraint!
    var isFindBarShown: Bool { !findBar.isHidden }

    /// The cell inspector (mockup 05a), under the grid.
    let inspector = CellInspectorView()
    /// The inspector's height: none while hidden (required), and its own
    /// while shown, which gives way before the grid's minimum does.
    private var inspectorHidden: NSLayoutConstraint!
    private var inspectorShown: NSLayoutConstraint!
    var isInspectorShown: Bool { !inspector.isHidden }
    /// The inspector's latest read of a value, so tests can wait for it.
    private(set) var inspectorTask: Task<Void, Never>?
    /// What the inspector shows now.
    private(set) var inspectorContent: InspectorContent?

    /// Where Copy puts the cells: the general pasteboard, or a private one
    /// in tests (which must not touch the user's clipboard).
    var pasteboard = NSPasteboard.general
    /// The latest Copy's job, while it runs, and its promise to the
    /// pasteboard.
    private(set) var copyTask: Task<Void, Never>?
    private(set) var copyPromise: CopyPromise?
    /// The brief "wrapped" sign over the grid (Safari's), when Next or
    /// Previous goes round the end of the file.
    let wrapIndicator = WrapIndicatorView()
    /// The latest VoiceOver announcement, for tests.
    private(set) var lastAnnouncement: String?

    init(model: DocumentModel, scheduler: Scheduler) {
        self.model = model
        self.scheduler = scheduler
        find = FindModel(model: model)
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    /// The least height the grid is given: its header and three rows.
    static let gridMinimumHeight = GridMetrics.headerHeight + 3 * GridMetrics.rowHeight

    override func loadView() {
        let root = NSView(frame: NSRect(x: 0, y: 0, width: 1200, height: 752))
        banners.orientation = .vertical
        banners.spacing = 0
        banners.alignment = .width
        for view in [findBar, banners, grid, inspector, statusBar, wrapIndicator] as [NSView] {
            view.translatesAutoresizingMaskIntoConstraints = false
            root.addSubview(view)
        }
        // With no banners the stack has no height of its own; keep it at
        // zero rather than letting it take the grid's space.
        let noBanners = banners.heightAnchor.constraint(equalToConstant: 0)
        noBanners.priority = .defaultLow
        // The find bar and the inspector are hidden until asked for.
        findBarHeight = findBar.heightAnchor.constraint(equalToConstant: 0)
        inspectorHidden = inspector.heightAnchor.constraint(equalToConstant: 0)
        inspectorShown = inspector.heightAnchor.constraint(equalToConstant: CellInspectorView.height)
        inspectorShown.priority = .init(999)
        // The grid always keeps its header and a few rows, however many
        // banners and panes show: the window can't be made smaller than
        // that, and grows if a banner needs the room (phase 1 review,
        // app-6).
        let gridMinimum = grid.heightAnchor.constraint(greaterThanOrEqualToConstant: Self.gridMinimumHeight)
        findBar.isHidden = true
        inspector.isHidden = true
        NSLayoutConstraint.activate([
            noBanners,
            findBarHeight,
            findBar.topAnchor.constraint(equalTo: root.topAnchor),
            findBar.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            findBar.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            banners.topAnchor.constraint(equalTo: findBar.bottomAnchor),
            banners.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            banners.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            grid.topAnchor.constraint(equalTo: banners.bottomAnchor),
            grid.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            grid.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            grid.bottomAnchor.constraint(equalTo: inspector.topAnchor),
            gridMinimum,
            inspectorHidden,
            inspector.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            inspector.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            inspector.bottomAnchor.constraint(equalTo: statusBar.topAnchor),
            statusBar.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            statusBar.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            statusBar.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            wrapIndicator.centerXAnchor.constraint(equalTo: grid.centerXAnchor),
            wrapIndicator.centerYAnchor.constraint(equalTo: grid.centerYAnchor),
            wrapIndicator.widthAnchor.constraint(equalToConstant: WrapIndicatorView.size),
            wrapIndicator.heightAnchor.constraint(equalToConstant: WrapIndicatorView.size),
        ])
        view = root
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        let scheduler = scheduler
        grid.onUserInput = { [weak self] in
            scheduler.noteUserInput()
            // The user went somewhere: find doesn't move the selection now.
            self?.find.cancelPendingStep()
        }
        grid.onGesture = { [weak self] in self?.model.setInteracting($0) }
        grid.onColumnResized = { [weak self] column, width in self?.model.columnResized(column, width: width) }
        grid.fittingWidth = { [weak self] column in
            guard let self else { return nil }
            return model.fittingWidth(column: column, visibleRows: grid.visibleRows)
        }
        grid.isIndexComplete = { [weak self] in self?.model.isIndexComplete ?? true }
        grid.dataSource = model
        grid.setColumnWidths(model.columnWidths)
        statusBar.onToggleHeader = { [weak self] in self?.setHeaderRow(true) }
        statusBar.onTreatAs = { [weak self] in self?.treatAs($0) }
        statusBar.onReopen = { [weak self] in self?.reopen(encoding: $0) }
        statusBar.onBadge = { [weak self] in self?.showDetails(nil) }
        grid.onSelectionChanged = { [weak self] in self?.selectionChanged() }
        grid.onCopy = { [weak self] in self?.copySelection() }
        confirmLargeCopy = { [weak self] bytes, answer in
            guard let self else { return answer(false) }
            askBeforeCopying(bytes, answer)
        }
        findBar.onQuery = { [weak self] text in self?.search(for: text) }
        findBar.onStep = { [weak self] forward in self?.step(forward: forward) }
        findBar.onDone = { [weak self] in self?.hideFindBar() }
        findBar.onIgnoreCase = { [weak self] _ in self?.search(for: self?.findBar.field.stringValue ?? "") }
        find.onChange = { [weak self] in self?.findChanged() }
        find.onSelect = { [weak self] cell in self?.grid.select(cell) }
        model.onChange = { [weak self] change in self?.modelChanged(change) }
        updateBanners()
        statusBar.show(model.status)
        if model.rowCount > 0 {
            grid.activeCell = CellPosition(row: 0, column: 0)
        }
    }

    private func modelChanged(_ change: DocumentChange) {
        switch change {
        case .progress:
            if grid.geometry.columnCount != model.columnCount {
                grid.setColumnWidths(model.columnWidths)
            }
            grid.reloadData()
            // A row the inspector was waiting for may be read now.
            if inspectorContent == .note(InspectorText.notRead) { updateInspector() }
        case .columns:
            grid.setColumnWidths(model.columnWidths)
            grid.invalidateContent()
        case .content:
            grid.setColumnWidths(model.columnWidths)
            grid.invalidateContent()
            grid.activeCell = model.rowCount > 0 ? CellPosition(row: 0, column: 0) : nil
            navigation.reset()
            // The file was read again: its matches are different. A copy
            // of the old reading's cells goes on: it was promised to the
            // pasteboard, and keeps what it reads (`CopyPromise`).
            if isFindBarShown { find.restart(from: grid.activeCell) }
            updateInspector()
        case .rows:
            // The same reading with fresh rows (after a change while reading),
            // or the file read again after its drive came back: matches and
            // the inspected value may differ.
            grid.invalidateContent()
            if isFindBarShown { find.restart(from: grid.activeCell) }
            updateInspector()
        case .reloaded:
            // A new snapshot, however the reload came (the banner, the
            // menu, or a second `read(from:)`): every banner may show
            // again, and the selection and scroll position stay where they
            // were, as far as the new file reaches.
            let cell = grid.activeCell
            let origin = grid.scrollView.contentView.bounds.origin
            dismissed.removeAll()
            detailsPopover?.close()
            navigation.reset()
            grid.setColumnWidths(model.columnWidths)
            grid.invalidateContent()
            restore(cell: cell, origin: origin)
            // Task 1.8's tools follow the new snapshot: an open find runs
            // again from the restored cell, and the inspector shows that
            // cell's value as it is now. A copy already promised keeps the
            // cells it was made from (`CopyPromise` holds its own core job).
            if isFindBarShown { find.restart(from: grid.activeCell) }
            updateInspector()
        case .failed:
            grid.invalidateContent()
            detailsPopover?.close()
            find.stop()
            onFailure?()
        }
        updateBanners()
        statusBar.show(model.status)
        updateDetails()
        updateReadOnly()
    }

    /// Tells the window whether the file is read-only, when that changes:
    /// a Reload can find a UTF-16 file saved as UTF-8, or the reverse
    /// (phase 1 review, app-4).
    func updateReadOnly() {
        guard shownReadOnly != model.isReadOnly else { return }
        shownReadOnly = model.isReadOnly
        onReadOnlyChanged?(model.isReadOnly)
    }

    // MARK: Banners

    /// Shows the banners the document's state calls for, in their order.
    func updateBanners() {
        if bannerGeneration != model.generation {
            // A new reading of the file: suggestions and the diagnostics
            // banner start afresh. The file's banners are about the file,
            // not the reading.
            dismissed = dismissed.filter { $0.hasPrefix("drive") || $0.hasPrefix("file") }
            bannerGeneration = model.generation
        }
        guard !model.isFailed else {
            for banner in [driveBanner, readOnlyBanner, diagnosticsBanner, delimiterBanner, encodingBanner] { banner?.removeFromSuperview() }
            driveBanner = nil
            readOnlyBanner = nil
            diagnosticsBanner = nil
            delimiterBanner = nil
            encodingBanner = nil
            return
        }

        // The file itself (ADR-0006, 1.1a, task 1.9): the first that
        // applies and wasn't dismissed.
        let file = FileBanner.applicable(
            changedWhileReading: model.changedOnDisk,
            original: model.original.state,
            storage: model.storage,
            readStopped: model.readStopped
        ).first { !dismissed.contains($0.key) }
        driveBanner = banner(
            driveBanner,
            key: file?.key,
            make: { key in
                makeBanner(
                    kind: .warning,
                    message: file?.message ?? "",
                    button: file?.buttonTitle,
                    action: file?.reloads == true ? #selector(reloadFromDisk(_:)) : #selector(saveACopy(_:)),
                    key: key,
                    secondaryTitle: file?.secondaryTitle,
                    secondaryAction: #selector(keepEditing(_:))
                )
            }
        )
        driveBanner?.message = file?.message ?? ""

        // UTF-16 (mockup 06a).
        readOnlyBanner = banner(readOnlyBanner, key: model.isReadOnly ? "read-only" : nil) { key in
            makeBanner(
                kind: .info,
                message: String(
                    localized: "This file is UTF-16, so Leal shows it read-only. Save a UTF-8 copy to edit it.",
                    comment: "Banner on a UTF-16 file (DESIGN §4.3, mockup 06a)"
                ),
                button: String(localized: "Save As UTF-8…", comment: "Banner button on a UTF-16 file"),
                action: #selector(saveAsUTF8(_:)),
                key: key
            )
        }
        updateSaveAsUTF8Button()

        // The irregularities (mockup 03a).
        let kinds = Int(model.diagnostics?.bannerKinds ?? 0)
        let showsDiagnostics = model.diagnostics?.showsBanner == true
        diagnosticsBanner = banner(diagnosticsBanner, key: showsDiagnostics ? "diagnostics" : nil) { key in
            makeBanner(
                kind: .warning,
                message: DiagnosticsText.banner(kinds: kinds),
                button: DiagnosticsText.details,
                prominent: false,
                action: #selector(showDetails(_:)),
                key: key
            )
        }
        diagnosticsBanner?.message = DiagnosticsText.banner(kinds: kinds)

        // The suggestions (DESIGN §3.2, ADR-0005 decision 4).
        let delimiter = model.review?.delimiterSuggestion
        delimiterBanner = banner(delimiterBanner, key: delimiter.map { "delimiter-\($0)" }) { key in
            makeBanner(
                kind: .info,
                message: delimiter.map(DiagnosticsText.delimiterSuggestion) ?? "",
                button: DiagnosticsText.switchDelimiter,
                action: #selector(acceptDelimiterSuggestion(_:)),
                key: key
            )
        }
        let encoding = model.review?.encodingSuggestion
        encodingBanner = banner(encodingBanner, key: encoding.map { "encoding-\($0)" }) { key in
            makeBanner(
                kind: .info,
                message: encoding.map(DiagnosticsText.encodingSuggestion) ?? "",
                button: encoding.map(DiagnosticsText.reopenAs),
                action: #selector(acceptEncodingSuggestion(_:)),
                key: key
            )
        }

        updateSuggestionButtons()

        let order = [driveBanner, readOnlyBanner, diagnosticsBanner, delimiterBanner, encodingBanner].compactMap { $0 }
        if banners.arrangedSubviews != order {
            for view in banners.arrangedSubviews { banners.removeArrangedSubview(view); view.removeFromSuperview() }
            for view in order { banners.addArrangedSubview(view) }
        }
        updateWindowMinimum()
    }

    // MARK: The window's size (phase 1 review, app-6)

    /// The least content height for what shows now: the find bar, each
    /// banner (at least `BannerView.minimumHeight`), the grid's header and
    /// a few rows, the inspector and the status bar.
    var minimumContentHeight: CGFloat {
        (isFindBarShown ? FindBarView.height : 0)
            + CGFloat(banners.arrangedSubviews.count) * BannerView.minimumHeight
            + Self.gridMinimumHeight
            + (isInspectorShown ? CellInspectorView.height : 0)
            + StatusBarView.height
    }

    /// The window can't be made smaller than `minimumContentHeight`, and
    /// grows to it when a banner or pane appears. The grid's minimum is a
    /// required constraint as well, so a banner that wraps onto more lines
    /// takes its room from the window too.
    func updateWindowMinimum() {
        guard let window = view.window else { return }
        let base = DocumentWindowController.minimumContentSize
        let minimum = NSSize(width: base.width, height: max(base.height, minimumContentHeight))
        guard window.contentMinSize != minimum else { return }
        window.contentMinSize = minimum
        let current = window.contentRect(forFrameRect: window.frame).size
        if current.height < minimum.height {
            window.setContentSize(NSSize(width: current.width, height: minimum.height))
        }
    }

    /// The banner for `key` (`nil`: none), made if needed, unless it was
    /// dismissed.
    private func banner(_ existing: BannerView?, key: String?, make: (String) -> BannerView) -> BannerView? {
        guard let key, !dismissed.contains(key) else {
            existing?.removeFromSuperview()
            return nil
        }
        if let existing, existing.identifier?.rawValue == key { return existing }
        existing?.removeFromSuperview()
        return make(key)
    }

    private func makeBanner(
        kind: BannerView.Kind,
        message: String,
        button: String?,
        prominent: Bool = true,
        action: Selector,
        key: String,
        secondaryTitle: String? = nil,
        secondaryAction: Selector? = nil
    ) -> BannerView {
        let banner = BannerView(
            kind: kind,
            message: message,
            buttonTitle: button,
            prominent: prominent,
            target: self,
            action: action,
            secondaryTitle: secondaryTitle,
            secondaryAction: secondaryAction
        )
        banner.identifier = NSUserInterfaceItemIdentifier(key)
        banner.onDismiss = { [weak self] in
            self?.dismissed.insert(key)
            self?.updateBanners()
        }
        return banner
    }

    // MARK: The file changed elsewhere (task 1.9, DESIGN §3.1)

    /// **Reload**, from the file's banner or File ▸ Reload from Disk: open
    /// the file again as it is now. The active cell and scroll position
    /// stay where they were, as far as the new file reaches. If the file
    /// can't be opened, the window says why and keeps what it shows.
    @objc func reloadFromDisk(_ sender: Any?) {
        scheduler.noteUserInput()
        // Opening the file reads it, which on a network share can block:
        // always off the main thread (task 2.0, ADR-0009). The window shows
        // the old snapshot until the new one is ready.
        guard reloading == nil, savingAsUTF8 == nil else { return }
        let reload = onReload
        let model = model
        model.willReload()
        reloading = Task { [weak self] in
            do {
                if let reload {
                    try await reload()
                } else {
                    try await model.reloadInBackground()
                }
            } catch {
                self?.showReloadError(error)
            }
            model.reloadEnded()
            self?.reloading = nil
        }
    }

    /// After a Reload: the active cell and scroll position as they were,
    /// kept inside the new file.
    private func restore(cell: CellPosition?, origin: NSPoint) {
        if let cell, model.rowCount > 0, model.columnCount > 0 {
            grid.activeCell = CellPosition(row: min(cell.row, model.rowCount - 1), column: min(cell.column, model.columnCount - 1))
        } else {
            grid.activeCell = model.rowCount > 0 ? CellPosition(row: 0, column: 0) : nil
        }
        let clip = grid.scrollView.contentView
        let maxX = max(0, grid.gridView.frame.width - clip.bounds.width)
        let maxY = max(0, grid.gridView.frame.height - clip.bounds.height)
        clip.scroll(to: NSPoint(x: min(origin.x, maxX), y: min(origin.y, maxY)))
        grid.scrollView.reflectScrolledClipView(clip)
    }

    /// **Keep Editing**: keep the version Leal opened and hide the banner.
    /// The core remembers that the file diverged, for Save's check (phase
    /// 2), and the status bar keeps a note.
    @objc func keepEditing(_ sender: Any?) {
        guard let key = driveBanner?.identifier?.rawValue else { return }
        dismissed.insert(key)
        updateBanners()
    }

    private func showReloadError(_ error: any Error) {
        guard let window = view.window else { return }
        let failure = CSVDocument.openError(error, url: model.url)
        let alert = NSAlert()
        alert.messageText = String(
            localized: "Leal couldn’t reload “\(model.url.lastPathComponent)”.",
            comment: "Alert title: Reload failed (task 1.9); the file's name"
        )
        alert.informativeText = failure.localizedRecoverySuggestion ?? ""
        alert.beginSheetModal(for: window)
    }

    /// "Switch": read the file with the suggested delimiter.
    @objc func acceptDelimiterSuggestion(_ sender: Any?) {
        guard let delimiter = model.review?.delimiterSuggestion else { return }
        treatAs(delimiter)
    }

    /// "Reopen as …": read the file in the suggested encoding.
    @objc func acceptEncodingSuggestion(_ sender: Any?) {
        guard let encoding = model.review?.encodingSuggestion else { return }
        reopen(encoding: encoding)
    }

    // MARK: Changing the interpretation (ADR-0005 decision 8)

    private func setHeaderRow(_ header: Bool) {
        scheduler.noteUserInput()
        guard savingAsUTF8 == nil else { return NSSound.beep() }
        model.setHeaderRow(header)
    }

    func treatAs(_ delimiter: Delimiter) {
        scheduler.noteUserInput()
        guard savingAsUTF8 == nil else { return NSSound.beep() }
        model.treatAs(delimiter)
    }

    func reopen(encoding: TextEncoding) {
        scheduler.noteUserInput()
        guard savingAsUTF8 == nil else { return NSSound.beep() }
        model.reopen(encoding: encoding)
    }

    /// View > Use First Row as Header.
    @objc func toggleHeaderRow(_ sender: Any?) {
        setHeaderRow(!model.interpretation.header)
    }

    /// View > Treat As > a delimiter.
    @objc func treatAsDelimiter(_ sender: NSMenuItem) {
        guard let delimiter = MainMenu.delimiter(of: sender) else { return }
        treatAs(delimiter)
    }

    /// File > Reopen with Encoding > an encoding.
    @objc func reopenWithEncoding(_ sender: NSMenuItem) {
        guard let encoding = MainMenu.encoding(of: sender) else { return }
        reopen(encoding: encoding)
    }

    /// Whether the file can be read another way (a different delimiter,
    /// encoding or header row): not once it changed on disk until a Reload,
    /// nor while Save As UTF-8 replaces the file shown.
    private var canReinterpret: Bool {
        model.canReinterpret && savingAsUTF8 == nil
    }

    /// The suggestions read the file again too: off until a Reload, and
    /// while Save As UTF-8 is under way.
    private func updateSuggestionButtons() {
        for suggestion in [delimiterBanner, encodingBanner] {
            suggestion?.button?.isEnabled = canReinterpret
            suggestion?.button?.toolTip = rereadReason
        }
    }

    /// Why reading the file another way is off, if it is for a reason the
    /// user can act on: it changed while Leal read it, so Reload first. The
    /// core refuses to read it again then (`ChangedOnDisk`).
    private var rereadReason: String? {
        model.changedOnDisk && !model.isFailed ? StatusText.reloadFirst : nil
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case #selector(toggleHeaderRow(_:)):
            menuItem.state = model.interpretation.header ? .on : .off
            menuItem.toolTip = rereadReason
            // Not while Save As UTF-8 replaces the file shown.
            return canReinterpret
        case #selector(treatAsDelimiter(_:)):
            menuItem.state = MainMenu.delimiter(of: menuItem) == model.interpretation.delimiter ? .on : .off
            menuItem.toolTip = rereadReason
            return canReinterpret
        case #selector(reopenWithEncoding(_:)):
            let encoding = MainMenu.encoding(of: menuItem)
            menuItem.state = encoding == model.interpretation.encoding ? .on : .off
            menuItem.toolTip = rereadReason
            return canReinterpret
                && encoding.map(model.interpretation.encodingChoices.contains) == true
        case #selector(showDetails(_:)):
            return !model.isFailed && model.diagnostics?.diagnostics.isEmpty == false
        case #selector(showFind(_:)):
            return !model.isFailed
        case #selector(findNext(_:)), #selector(findPrevious(_:)):
            return !model.isFailed && !findBar.field.stringValue.isEmpty
        case #selector(goToRow(_:)):
            return !model.isFailed && model.rowCount > 0
        case #selector(toggleCellInspector(_:)):
            menuItem.title = isInspectorShown ? MainMenu.hideInspector : MainMenu.showInspector
            return !model.isFailed
        case #selector(reloadFromDisk(_:)):
            // There must be a file to open again, and no Reload or Save As
            // UTF-8 under way.
            return !model.isFailed && !model.isReloading && reloading == nil && savingAsUTF8 == nil
                && model.original.state != .deleted && model.original.state != .unavailable
        default:
            return true
        }
    }

    // MARK: Find (⌘F, ⌘G, ⇧⌘G; mockup 04a)

    /// Edit > Find > Find… (⌘F): shows the find bar, with its query
    /// selected, and searches for it again if the bar was closed.
    @objc func showFind(_ sender: Any?) {
        showFindBar()
        view.window?.makeFirstResponder(findBar.field)
        findBar.field.selectText(nil)
    }

    /// Edit > Find > Find Next (⌘G). With no query yet, it opens the bar.
    @objc func findNext(_ sender: Any?) {
        stepFromMenu(forward: true)
    }

    /// Edit > Find > Find Previous (⇧⌘G).
    @objc func findPrevious(_ sender: Any?) {
        stepFromMenu(forward: false)
    }

    private func stepFromMenu(forward: Bool) {
        guard !findBar.field.stringValue.isEmpty else {
            showFind(nil)
            return
        }
        showFindBar()
        step(forward: forward)
    }

    /// Shows the find bar, its highlights, and the search for its query.
    func showFindBar() {
        guard findBar.isHidden else { return }
        findBar.isHidden = false
        findBarHeight.constant = FindBarView.height
        updateWindowMinimum()
        grid.highlighter = find
        if !findBar.field.stringValue.isEmpty {
            search(for: findBar.field.stringValue)
        }
        findChanged()
    }

    /// Done or Esc: the bar, its highlights and its search go; the query
    /// stays for the next ⌘F or ⌘G.
    func hideFindBar() {
        guard !findBar.isHidden else { return }
        find.stop()
        grid.highlighter = nil
        findBar.isHidden = true
        findBarHeight.constant = 0
        updateWindowMinimum()
        view.window?.makeFirstResponder(grid.gridView)
    }

    /// The query changed: search for it from the active cell, so the
    /// first match at or after it is selected as soon as it is found.
    func search(for text: String) {
        find.find(text, caseSensitive: !findBar.ignoresCase, from: grid.activeCell)
    }

    /// Next or Previous from the active cell.
    func step(forward: Bool) {
        if find.search == nil, !findBar.field.stringValue.isEmpty {
            // Searching again (the bar was closed, or the file read again).
            find.find(findBar.field.stringValue, caseSensitive: !findBar.ignoresCase, from: nil)
        }
        find.step(forward: forward, from: grid.activeCell)
    }

    private func findChanged() {
        let complete = find.progress?.complete ?? false
        findBar.show(
            count: find.query.isEmpty
                ? ""
                : FindText.count(current: find.current?.ordinal, total: find.matchCount, complete: complete, searching: find.isSearching),
            canStep: find.matchCount > 0 || find.isSearching
        )
        if find.misses > lastMisses {
            NSSound.beep()
        }
        lastMisses = find.misses
        // VoiceOver hears the count after Next or Previous, and when a
        // search finishes (DESIGN §4.4).
        if find.steps > lastSteps, let current = find.current {
            let count = FindText.count(current: current.ordinal, total: find.matchCount, complete: complete, searching: find.isSearching)
            if find.lastStepWrapped {
                wrapIndicator.show(over: grid)
                announce(FindText.wrapped(forward: find.lastStepForward, count: count))
            } else {
                announce(count)
            }
        }
        lastSteps = find.steps
        if complete, let search = find.search, announcedSearch != ObjectIdentifier(search) {
            announcedSearch = ObjectIdentifier(search)
            announce(FindText.count(current: find.current?.ordinal, total: find.matchCount, complete: true, searching: false))
        }
        grid.gridView.needsDisplay = true
    }

    private var lastMisses = 0
    private var lastSteps = 0
    /// The search whose end was last announced.
    private var announcedSearch: ObjectIdentifier?

    private func announce(_ text: String) {
        lastAnnouncement = text
        NSAccessibility.post(
            element: view.window ?? findBar,
            notification: .announcementRequested,
            userInfo: [.announcement: text, .priority: NSAccessibilityPriorityLevel.high.rawValue]
        )
    }

    // MARK: Go to Row (⌘L)

    /// Edit > Go to Row… (⌘L): asks for a row number, as the gutter shows
    /// it, and goes there.
    @objc func goToRow(_ sender: Any?) {
        guard let window = view.window else { return }
        let (alert, field) = goToRowAlert()
        alert.beginSheetModal(for: window) { [weak self] response in
            guard response == .alertFirstButtonReturn else { return }
            guard let number = FindText.rowNumber(from: field.stringValue) else {
                NSSound.beep()
                return
            }
            self?.goTo(rowNumber: number)
        }
    }

    /// The Go to Row sheet and its row number field.
    func goToRowAlert() -> (NSAlert, NSTextField) {
        let alert = NSAlert()
        alert.messageText = FindText.goToRowTitle
        alert.informativeText = FindText.goToRowMessage(rows: model.rowCount, exact: model.isIndexComplete)
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 220, height: 24))
        field.placeholderString = FindText.goToRowField
        field.setAccessibilityLabel(FindText.goToRowField)
        alert.accessoryView = field
        alert.addButton(withTitle: FindText.go)
        alert.addButton(withTitle: FindText.cancel)
        alert.window.initialFirstResponder = field
        return (alert, field)
    }

    /// Goes to row `number` (from 1, as the gutter numbers rows). Past the
    /// indexed rows, the grid shows the row as soon as the index reaches it
    /// (DESIGN §3.10 rule 5).
    func goTo(rowNumber number: Int) {
        scheduler.noteUserInput()
        find.cancelPendingStep()
        view.window?.makeFirstResponder(grid.gridView)
        grid.goTo(row: number - 1)
    }

    // MARK: Copy (⌘C)

    /// A copy of at most this many bytes, of rows the index has, goes on
    /// the clipboard at once, on the main thread: a few screens of cells
    /// take well under a millisecond.
    nonisolated static let immediateCopyBytes: UInt64 = 4 << 20
    /// A copy of more than this many bytes asks first (1.8 review).
    var askBeforeCopyBytes: UInt64 = 100_000_000

    /// Asks whether to go on with a copy of about `bytes` bytes. Tests
    /// answer for themselves.
    var confirmLargeCopy: (_ bytes: UInt64, _ answer: @escaping @MainActor (Bool) -> Void) -> Void = { _, answer in answer(true) }

    /// Copies the selected cells as tab-separated display values (DESIGN
    /// §4.2), from the core. A small selection the index has goes on the
    /// clipboard at once. A larger one, or one past the indexed rows, is a
    /// P2 job in the core, whose text is promised to the pasteboard
    /// straight away (`Clipboard.promise`), so a paste never gets the old
    /// clipboard; the text is built off the main thread and taken from
    /// the core only when something pastes it. Over about 100 MB, it asks
    /// first.
    func copySelection() {
        guard let selection = grid.selection, !model.isFailed else { return }
        // A copy still running is stopped by its pasteboard, when this one
        // replaces its promise (`CopyPromise`).
        copyTask = nil
        copyPromise = nil
        let range = model.copyRange(selection)
        let bytes = model.estimatedCopyBytes(range)
        if bytes <= Self.immediateCopyBytes, let text = model.copyCellsNow(range) {
            Clipboard.write(text, to: pasteboard)
            return
        }
        guard bytes > askBeforeCopyBytes else {
            startCopy(range)
            return
        }
        confirmLargeCopy(bytes) { [weak self] go in
            if go { self?.startCopy(range) }
        }
    }

    private func startCopy(_ range: CopyRange) {
        guard let job = model.copyCells(range) else { return }
        copyPromise = Clipboard.promise(job, to: pasteboard)
        let waiter = job.job()
        // Only for tests to await: nothing cancels this task, since the
        // copy outlives the window (`CopyPromise`).
        copyTask = Task {
            try? await waiter.finish()
        }
    }

    /// The sheet that asks before a very large copy.
    func askBeforeCopying(_ bytes: UInt64, _ answer: @escaping @MainActor (Bool) -> Void) {
        guard let window = view.window else {
            answer(false)
            return
        }
        let alert = NSAlert()
        let size = ByteCountFormatter.string(fromByteCount: Int64(clamping: bytes), countStyle: .file)
        alert.messageText = FindText.largeCopyTitle(size)
        alert.informativeText = FindText.largeCopyMessage
        alert.addButton(withTitle: FindText.copy)
        alert.addButton(withTitle: FindText.cancel)
        alert.beginSheetModal(for: window) { response in
            answer(response == .alertFirstButtonReturn)
        }
    }

    /// The document is closing (`CSVDocument.close`): the find search and
    /// the inspector's read stop, so neither keeps the core's document, and
    /// its file, open. A copy promised to the pasteboard goes on: it keeps
    /// its own reading and the file's clone until the pasteboard has its
    /// text (`CopyPromise`).
    func documentWillClose() {
        find.stop()
        inspectorTask?.cancel()
        // The save stops (its job is cancelled with the task), writing
        // nothing, and says nothing.
        savingAsUTF8?.cancel()
        wrapIndicator.dismiss()
    }

    // MARK: The cell inspector (⌘I; mockup 05a)

    /// View > Show Cell Inspector (⌘I).
    @objc func toggleCellInspector(_ sender: Any?) {
        setInspectorShown(!isInspectorShown)
    }

    func setInspectorShown(_ shown: Bool) {
        guard shown != isInspectorShown else { return }
        inspector.isHidden = !shown
        // Deactivate before activating, so the two never both hold.
        (shown ? inspectorHidden : inspectorShown).isActive = false
        (shown ? inspectorShown : inspectorHidden).isActive = true
        updateWindowMinimum()
        if shown {
            updateInspector()
            if let cell = grid.activeCell { grid.gridView.scrollToVisible(grid.geometry.cellRect(row: cell.row, column: cell.column)) }
        } else {
            inspectorTask?.cancel()
            if view.window?.firstResponder === inspector.textView {
                view.window?.makeFirstResponder(grid.gridView)
            }
        }
    }

    private func selectionChanged() {
        find.activeCellChanged(grid.activeCell)
        updateInspector()
    }

    /// Shows the active cell's whole value, read off the main thread.
    func updateInspector() {
        guard isInspectorShown else { return }
        inspectorTask?.cancel()
        guard let cell = grid.activeCell, cell.column < model.columnCount else {
            showInInspector(column: "", row: nil, content: .note(InspectorText.noCell))
            return
        }
        let title = model.headerTitle(column: cell.column)
        let column = title.style == .number ? GridStrings.extraColumn(cell.column + 1) : title.text
        guard cell.row < model.loadedRowCount else {
            showInInspector(column: column, row: cell.row + 1, content: .note(InspectorText.notRead))
            return
        }
        inspectorTask = Task { [weak self] in
            guard let self else { return }
            let value = await model.cellValue(row: cell.row, column: cell.column)
            guard !Task.isCancelled, grid.activeCell == cell else { return }
            let content: InspectorContent = switch value {
            case let value? where value.exists: .value(value)
            case .some: .note(InspectorText.missing)
            case nil: .note(InspectorText.notRead)
            }
            showInInspector(column: column, row: cell.row + 1, content: content)
        }
    }

    private func showInInspector(column: String, row: Int?, content: InspectorContent) {
        inspectorContent = content
        inspector.show(column: column, row: row, content: content)
    }

    // MARK: The details popover (mockup 03b)

    /// Shows the details of the irregularities: from the banner's Details
    /// button, or the status bar's badge once the banner is dismissed.
    @objc func showDetails(_ sender: Any?) {
        if let popover = detailsPopover, popover.isShown {
            popover.close()
            return
        }
        guard !model.isFailed, model.diagnostics?.diagnostics.isEmpty == false else { return }
        let controller = DiagnosticsDetailsController()
        controller.onNavigate = { [weak self] kind, forward in self?.navigate(kind, forward: forward) }
        details = controller
        updateDetails()
        let popover = NSPopover()
        popover.contentViewController = controller
        popover.behavior = .transient
        popover.animates = false
        detailsPopover = popover
        // The banner's Details button; once it is dismissed, the badge; for
        // a file with only info-level kinds, their note in the status bar.
        let anchor = detailsAnchor
        if anchor.window != nil {
            popover.show(relativeTo: anchor.bounds, of: anchor, preferredEdge: anchor === diagnosticsBanner?.button ? .minY : .maxY)
        }
    }

    /// What the details popover points at: always a view on screen.
    var detailsAnchor: NSView {
        if let button = diagnosticsBanner?.button, !button.isHiddenOrHasHiddenAncestor {
            return button
        }
        return statusBar.detailsAnchor
    }

    private func updateDetails() {
        guard let details, let report = model.diagnostics else { return }
        details.show(
            report: report,
            navigation: navigation,
            fileName: model.url.lastPathComponent,
            headerRows: model.headerRows,
            lineEnding: model.lineEnding,
            encoding: model.interpretation.encoding
        )
    }

    /// A kind's **Previous** or **Next** (mockup 03b): the core finds the
    /// occurrence, in every row, and the grid selects its cell. Returns
    /// once it is shown (for tests).
    @discardableResult
    func navigate(_ kind: DiagnosticKind, forward: Bool) -> Task<Void, Never> {
        scheduler.noteUserInput()
        navigationCount += 1
        let count = navigationCount
        let start: UInt64? = forward ? navigation.nextStart(kind) : navigation.previousEnd(kind)
        return Task { @MainActor [weak self] in
            guard let self, let start else { return }
            guard let place = await model.find(kind, forward: forward, from: start), count == navigationCount else {
                if count == navigationCount {
                    // Nothing further: Next stops here, arrow off (no wrap).
                    if forward, navigation.row(of: kind) != nil { navigation.reachedEnd(kind) }
                    NSSound.beep()
                    updateDetails()
                }
                return
            }
            navigation.visit(kind, row: place.row)
            let column = min(Int(place.column), max(0, model.columnCount - 1))
            if place.row < UInt64(model.headerRows) {
                // In the header row, which isn't a grid row: show the header
                // at that column, with no cell selected.
                showHeader(column: column)
            } else {
                grid.select(CellPosition(row: model.gridRow(ofPhysical: place.row), column: column))
            }
            updateDetails()
        }
    }

    /// Scrolls to the top, with `column` in view, and selects no cell: an
    /// occurrence in the header row.
    private func showHeader(column: Int) {
        grid.activeCell = nil
        let clip = grid.scrollView.contentView
        let cell = grid.geometry.cellRect(row: 0, column: column)
        let maxX = max(0, grid.gridView.frame.width - clip.bounds.width)
        let visible = clip.bounds
        let x = cell.minX >= visible.minX && cell.maxX <= visible.maxX ? visible.minX : min(cell.minX, maxX)
        clip.scroll(to: NSPoint(x: x, y: 0))
        grid.scrollView.reflectScrolledClipView(clip)
    }

    // MARK: Saving (ADR-0006)

    /// **Save As UTF-8…**, the UTF-16 banner's button (mockup 06a, task 2.3,
    /// ADR-0008 decision 7): a save panel, then the core writes the file in
    /// UTF-8 there, and the window shows that copy, which can be edited.
    /// Bytes that can't be converted are named, and nothing is written.
    /// The button is off while one is under way or a Reload is
    /// (`canSaveAsUTF8`); asked anyway, it beeps.
    @objc func saveAsUTF8(_ sender: Any?) {
        scheduler.noteUserInput()
        guard canSaveAsUTF8, let window = view.window else { return NSSound.beep() }
        chooseUTF8Destination(Self.utf8CopyName(of: model.url), model.url.deletingLastPathComponent(), window) { [weak self] url in
            guard let self, let url else { return }
            self.startSavingAsUTF8(to: url)
        }
    }

    /// Where Save As UTF-8 writes: the save panel, as a sheet, with `name`
    /// in `folder` suggested; `nil` if cancelled. Tests answer for
    /// themselves.
    var chooseUTF8Destination: (_ name: String, _ folder: URL, _ window: NSWindow, _ done: @escaping @MainActor (URL?) -> Void) -> Void = { name, folder, window, done in
        let panel = NSSavePanel()
        panel.nameFieldStringValue = name
        panel.directoryURL = folder
        panel.canCreateDirectories = true
        panel.isExtensionHidden = false
        panel.beginSheetModal(for: window) { response in
            let url = response == .OK ? panel.url : nil
            MainActor.assumeIsolated { done(url) }
        }
    }

    /// **Save As UTF-8**, through the `NSDocument`
    /// (`CSVDocument.saveAsUTF8(to:)`), so it follows the copy. Returns
    /// whether it saved. Without one, the model saves and reloads.
    var onSaveAsUTF8: ((URL) async throws -> Bool)?
    /// A Save As UTF-8, while it is under way. Reload, Treat As and Reopen
    /// with Encoding are off meanwhile: each would read the file again
    /// while the save replaces it.
    /// SEAM(2.5): the save's progress (`SaveJob.progress`) isn't shown yet;
    /// 2.5's save progress in the status bar shows this one's too.
    private(set) var savingAsUTF8: Task<Void, Never>? {
        didSet {
            updateSaveAsUTF8Button()
            updateSuggestionButtons()
        }
    }

    /// Whether Save As UTF-8 can start: not while one is under way, nor
    /// during a Reload.
    var canSaveAsUTF8: Bool {
        savingAsUTF8 == nil && reloading == nil && !model.isReloading
    }

    /// The banner's button is on only when Save As UTF-8 can start, so a
    /// second click visibly does nothing.
    private func updateSaveAsUTF8Button() {
        readOnlyBanner?.button?.isEnabled = canSaveAsUTF8
    }

    /// An alert that came while the view had no window, shown when it has
    /// one again.
    private var queuedAlert: NSAlert?

    /// The name Save As UTF-8 suggests: the file's own, with "(UTF-8)".
    static func utf8CopyName(of url: URL) -> String {
        let stem = url.deletingPathExtension().lastPathComponent
        let suffix = String(localized: "UTF-8", comment: "Save As UTF-8: added to the suggested file name, as in \"people (UTF-8).csv\"")
        let name = "\(stem) (\(suffix))"
        return url.pathExtension.isEmpty ? name : "\(name).\(url.pathExtension)"
    }

    private func startSavingAsUTF8(to url: URL) {
        let save = onSaveAsUTF8
        let model = model
        savingAsUTF8 = Task { [weak self] in
            do {
                if let save {
                    _ = try await save(url)
                } else if try await model.saveAsUTF8(to: url) != nil {
                    try await model.reloadInBackground(from: url)
                }
            } catch let failure as SaveFailure {
                // Including the core refusing to start it
                // (`DocumentModel.saveAsUTF8`).
                self?.showSaveAsUTF8Failure(failure)
            } catch is CancellationError {
                // The window is closing (`savingAsUTF8?.cancel()`): no alert.
            } catch {
                // Saved, but the copy couldn't be read back.
                self?.showReloadError(error)
            }
            self?.savingAsUTF8 = nil
        }
    }

    /// Why Save As UTF-8 didn't save. Nothing was written either way. The
    /// details go to the log; a failed document says so itself.
    private func showSaveAsUTF8Failure(_ failure: SaveFailure) {
        guard let message = SaveText.saveAsUTF8Failure(failure, headerRows: model.headerRows) else { return }
        Logger.document.error("Save As UTF-8 failed: \(String(describing: failure), privacy: .public)")
        guard !model.isFailed else { return }
        let alert = NSAlert()
        alert.messageText = message.title
        alert.informativeText = message.detail
        present(alert)
    }

    /// Shows `alert` on the window, or once the view has one again.
    private func present(_ alert: NSAlert) {
        guard let window = view.window else {
            queuedAlert = alert
            return
        }
        showAlert(alert, window)
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        if let alert = queuedAlert {
            queuedAlert = nil
            present(alert)
        }
    }

    /// Shows `alert` as a sheet on `window`. Tests replace it, so that no
    /// sheet is shown.
    var showAlert: (_ alert: NSAlert, _ window: NSWindow) -> Void = { alert, window in
        alert.beginSheetModal(for: window)
    }

    /// The drive banners' Save As…: Save is refused (`DocumentModel.canSave`)
    /// but a copy may be saved elsewhere. SEAM(2.5): Leal writes files from
    /// task 2.5; until then this says so.
    @objc func saveACopy(_ sender: Any?) {
        showNotYet(
            String(localized: "Save As isn’t available yet.", comment: "Alert: the drive banner's Save As button before task 2.5"),
            String(
                localized: "A later version of Leal saves a copy of what it shows, wherever you choose.",
                comment: "Alert: the drive banner's Save As button before task 2.5"
            )
        )
    }

    private func showNotYet(_ message: String, _ information: String) {
        guard let window = view.window else { return }
        let alert = NSAlert()
        alert.messageText = message
        alert.informativeText = information
        alert.beginSheetModal(for: window)
    }
}

/// A document's window. `NSDocument` keeps its title, proxy icon, tabs and
/// restoration in step.
@MainActor
final class DocumentWindowController: NSWindowController {
    /// The mockups' window: 1200 × 780 points with its title bar.
    static let contentSize = NSSize(width: 1200, height: 752)
    /// The smallest the window's content may be with no banners or panes;
    /// more chrome raises the height (`updateWindowMinimum`).
    static let minimumContentSize = NSSize(width: 480, height: 240)

    let content: DocumentViewController

    init(model: DocumentModel, scheduler: Scheduler) {
        content = DocumentViewController(model: model, scheduler: scheduler)
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: Self.contentSize),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: true
        )
        window.contentViewController = content
        window.setContentSize(Self.contentSize)
        window.contentMinSize = Self.minimumContentSize
        window.initialFirstResponder = content.grid.gridView
        // `.automatic` (the default), so documents open as tabs or windows
        // as the user's "Prefer tabs when opening documents" setting says
        // (phase 1 review, app-5). Window ▸ Merge All Windows and the tab
        // bar work either way.
        window.tabbingMode = .automatic
        super.init(window: window)
        shouldCascadeWindows = true
        window.center()
        content.onReadOnlyChanged = { [weak self] readOnly in self?.showLock(readOnly) }
        content.updateReadOnly()
    }

    /// The lock glyph by the title, while the file is read-only.
    private(set) var lock: NSTitlebarAccessoryViewController?

    private func showLock(_ readOnly: Bool) {
        guard readOnly != (lock != nil), let window else { return }
        if readOnly {
            let accessory = Self.lockAccessory()
            window.addTitlebarAccessoryViewController(accessory)
            lock = accessory
        } else if let lock, let index = window.titlebarAccessoryViewControllers.firstIndex(of: lock) {
            window.removeTitlebarAccessoryViewController(at: index)
            self.lock = nil
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    /// The lock glyph of a read-only (UTF-16) file, by the title (mockup
    /// 06a, ADR-0002 question 14).
    private static func lockAccessory() -> NSTitlebarAccessoryViewController {
        let label = String(localized: "Read-only", comment: "The lock glyph by a read-only file's title, for VoiceOver")
        let image = NSImageView(image: NSImage(systemSymbolName: "lock", accessibilityDescription: label) ?? NSImage())
        image.symbolConfiguration = .init(pointSize: 12, weight: .regular)
        image.contentTintColor = .secondaryLabelColor
        image.toolTip = String(
            localized: "UTF-16 files are read-only in this version of Leal.",
            comment: "Tooltip of the lock glyph by a read-only file's title"
        )
        // The glyph sits in a plain container, which is the accessory's
        // view. AppKit throws "changing the view's origin is not allowed"
        // if the layout engine ever moves an accessory's view, and with the
        // image view itself as the accessory it did, in a layout pass of
        // the hosted tests on a Mac with a second, 1× display (1.8 review
        // fixes). The container has no content size of its own to lay out.
        let container = NSView(frame: NSRect(x: 0, y: 0, width: 22, height: 22))
        image.frame = container.bounds
        image.autoresizingMask = [.width, .height]
        container.addSubview(image)
        let accessory = NSTitlebarAccessoryViewController()
        accessory.view = container
        accessory.layoutAttribute = .leading
        return accessory
    }
}
