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
    let scheduler: Scheduler
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
    /// Asks whether to throw the unsaved edits away (task 2.5.2), before a
    /// Reload or Revert to Saved, through the `NSDocument`: `answer` hears
    /// whether to go on. Without one, the edits go without asking.
    var confirmDiscardingEdits: ((_ reason: RereadReason, _ answer: @escaping @MainActor (Bool) -> Void) -> Void)?
    /// Whether the `NSDocument` has a save under way or queued
    /// (`CSVDocument.saving`): a Save As waiting for its turn or for file
    /// coordination, which the model doesn't know of yet (`isSaving`).
    /// Reload and Revert to Saved wait for it, so they never replace the
    /// core document a Save As is about to write from (task 2.5.3c review).
    var documentIsSaving: () -> Bool = { false }
    /// The document's undo manager (`CSVDocument.history`, the window's
    /// too: `windowWillReturnUndoManager`), for a step of several commands
    /// (Delete Columns, task 2.5a), or a step with its own name
    /// (Duplicate Row).
    var undoHistory: () -> DocumentUndoManager? = { nil }
    /// The key window, which may be a sheet over this one (Go to Row's):
    /// its text field keeps ⌘↩ and ⌘⌫ (`availability`). Tests stand one in.
    var keyWindow: () -> NSWindow? = { NSApp.keyWindow }
    /// **Save As…** (task 2.5.3c), through the `NSDocument`
    /// (`CSVDocument.chooseSaveAs`): the file banners' Save As….
    var onSaveAs: (() -> Void)?
    /// A Reload, while it is under way.
    private(set) var reloading: Task<Void, Never>? {
        didSet { updateSaveAsUTF8Button() }
    }
    /// Whether the file is read-only (UTF-16) changed: the window's lock
    /// glyph follows it.
    var onReadOnlyChanged: ((Bool) -> Void)?
    /// What the lock glyph was last told.
    private var shownReadOnly: Bool?
    /// The reading of the file the grid's strips were last drawn for: a
    /// new one (a Reload, Treat As, the Header row, a drive's file read
    /// again, a save's new snapshot) redraws them all, whatever the change
    /// that brought it (task 2.0b).
    private var drawnReading: ReadingID?

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

    /// The in-cell editor (task 2.5.1).
    private(set) var cellEditor: CellEditController!
    /// The inspector's edit of the active cell (task 2.5.1, mockup 05a),
    /// while its value can be edited.
    private(set) var inspectorEdit: InspectorEdit?
    /// A long value being read in full before the inspector can edit it
    /// (ADR-0008 decision 3).
    private(set) var inspectorLoading: Task<Void, Never>?
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
        cellEditor = CellEditController(model: model, grid: grid)
        grid.onUserInput = { [weak self] in
            scheduler.noteUserInput()
            // The user went somewhere: find doesn't move the selection now.
            self?.find.cancelPendingStep()
            self?.cellEditor.dismissNote()
        }
        grid.onEdit = { [weak self] in self?.editActiveCell() }
        grid.onTypeToEdit = { [weak self] event in self?.editActiveCell(typing: event) }
        grid.onRowCommandKey = { [weak self] key in self?.rowCommandKey(StructureCommand(key)) }
        grid.onScroll = { [weak self] in self?.cellEditor.gridScrolled() }
        grid.headerView.menuForColumn = { [weak self] column in self?.headerMenu(column: column) }
        inspector.textView.delegate = self
        inspector.textView.onCommit = { [weak self] in self?.commitInspector(refocus: true) }
        inspector.textView.onCancel = { [weak self] in self?.cancelInspector() }
        grid.onGesture = { [weak self] in self?.model.setInteracting($0) }
        grid.onColumnResized = { [weak self] column, width in
            self?.model.columnResized(column, width: width)
            self?.cellEditor.relayout()
        }
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
        grid.onPaste = { [weak self] in self?.pasteIntoSelection() }
        grid.onCut = { [weak self] in self?.cutSelection() }
        grid.onClear = { [weak self] in self?.clearSelection() }
        grid.validateCellItem = { [weak self] item in self?.validateCellItem(item) ?? false }
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
        drawnReading = model.readingID
        model.onChange = { [weak self] change in self?.modelChanged(change) }
        updateBanners()
        showStatus()
        if model.rowCount > 0 {
            grid.activeCell = CellPosition(row: 0, column: 0)
        }
    }

    private func modelChanged(_ change: DocumentChange) {
        switch change {
        case .content, .rows, .reloaded, .failed:
            // The values may be different, or gone: an open editor
            // commits nothing (task 2.5.1).
            discardEditing()
        default:
            break
        }
        if model.readingID != drawnReading {
            // Every value may be different: nothing drawn for the old
            // reading stays, in the strips or the laid-out lines.
            drawnReading = model.readingID
            grid.invalidateContent()
        }
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
            cellEditor.relayout()
        case .widths:
            // An edit widened its column (task 2.5.1).
            grid.setColumnWidths(model.columnWidths)
            cellEditor.relayout()
            return
        case .header:
            // An edit to the header row (task 2.5.1): its titles, and the
            // column count if the header grew.
            if grid.geometry.columnCount != model.columnCount {
                grid.setColumnWidths(model.columnWidths)
            }
            grid.headerView.invalidateContent()
            if isInspectorShown { updateInspector() }
            return
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
        case .saved:
            // The file Leal just wrote, read as a new reading (task
            // 2.5.3b): the same values, so the selection, scroll position,
            // widths, an open editor and the inspector stay. Every cell was
            // drawn again above (the reading changed), so the saved edits'
            // marks go. Find searches the new reading; the old reading's
            // search is let go either way. A copy promised to the
            // pasteboard keeps the cells it was made from (`CopyPromise`).
            if grid.geometry.columnCount != model.columnCount {
                grid.setColumnWidths(model.columnWidths)
            }
            grid.reloadData()
            grid.headerView.invalidateContent()
            detailsPopover?.close()
            navigation.reset()
            // Searched again without a step: nothing moves the selection,
            // which may be a range, or an editor still open.
            if isFindBarShown {
                find.searchAgain()
            } else {
                find.stop()
            }
        case .failed:
            grid.invalidateContent()
            detailsPopover?.close()
            // Nothing can be edited any more.
            inspector.textView.isEditable = false
            find.stop()
            onFailure?()
        case let .structure(structure):
            // Rows or a column inserted or deleted (task 2.5a, or an undo
            // or a redo of one): every row after it moved. The selection
            // goes where the change was; Find restarts after a column's,
            // and catches up after rows' (ADR-0014 decision 2).
            grid.setColumnWidths(model.columnWidths)
            grid.invalidateContent()
            grid.reloadData()
            grid.headerView.invalidateContent()
            selectChange(structure)
            cellEditor.relayout()
            if isFindBarShown { find.structureChanged(structure) }
            updateInspector()
        case let .cells(rows):
            // A long value being read for an editor, of a row whose
            // values changed, is stale.
            cellEditor.valuesChanged(rows: rows)
            if let edit = inspectorEdit, inspectorLoading != nil, rows.contains(edit.cell.row) {
                inspectorLoading?.cancel()
                inspectorLoading = nil
                inspectorEdit = nil
            }
            grid.cellsChanged(rows: rows)
            // Find and the inspector read the cells as they are now
            // (ADR-0008 decision 2).
            if isFindBarShown { find.valuesChanged(rows: rows) }
            if let cell = grid.activeCell, rows.contains(cell.row), inspectorEdit?.changed != true {
                updateInspector()
            }
            // An edit rarely changes what the banners or the status bar
            // show (a new row does, to the row count): skip them then.
            if !bannerInputsChanged(), model.status == shownStatus {
                updateDetails()
                updateReadOnly()
                return
            }
        }
        updateBanners()
        showStatus()
        updateDetails()
        updateReadOnly()
    }

    /// The status the status bar shows.
    private var shownStatus: StatusSummary?

    private func showStatus() {
        let status = model.status
        shownStatus = status
        statusBar.show(status)
    }

    /// Everything `updateBanners` reads, as it was when it last ran.
    private struct BannerInputs: Equatable {
        var generation: UInt64
        var failed: Bool
        var changedOnDisk: Bool
        var original: OriginalState
        var storage: SourceStorage
        var readStopped: Bool
        var readOnly: Bool
        var bannerKinds: Int
        var showsDiagnostics: Bool
        var delimiter: String?
        var encoding: String?
        var dismissed: Set<String>
        var canSaveAsUTF8: Bool
        var canReinterpret: Bool
        var minimumContentHeight: CGFloat
    }

    private var shownBannerInputs: BannerInputs?

    private var bannerInputs: BannerInputs {
        BannerInputs(
            generation: model.generation,
            failed: model.isFailed,
            changedOnDisk: model.changedOnDisk,
            original: model.original.state,
            storage: model.storage,
            readStopped: model.readStopped,
            readOnly: model.isReadOnly,
            bannerKinds: Int(model.diagnostics?.bannerKinds ?? 0),
            showsDiagnostics: model.diagnostics?.showsBanner == true,
            delimiter: model.review?.delimiterSuggestion.map { "\($0)" },
            encoding: model.review?.encodingSuggestion.map { "\($0)" },
            dismissed: dismissed,
            canSaveAsUTF8: canSaveAsUTF8,
            canReinterpret: canChangeSplit,
            minimumContentHeight: minimumContentHeight
        )
    }

    /// Whether anything the banners show has changed since they were last
    /// updated.
    private func bannerInputsChanged() -> Bool {
        shownBannerInputs != bannerInputs
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
        defer { shownBannerInputs = bannerInputs }
        if bannerGeneration != model.generation {
            // A new reading of the file: suggestions and the diagnostics
            // banner start afresh. The file's banners are about the file,
            // not the reading. Not after Leal's own save (task 2.5.3b): the
            // file is the one the user had, with their edits, so a banner
            // they closed stays closed. Rob's decision (task 2.5.3b
            // review): it comes back only after a Reload, Treat As or a
            // re-read, never after a save.
            if !model.readingFromSave {
                dismissed = dismissed.filter { $0.hasPrefix("drive") || $0.hasPrefix("file") }
            }
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
                    secondaryAction: file?.secondarySavesAs == true ? #selector(saveACopy(_:)) : #selector(keepEditing(_:))
                )
            }
        )
        driveBanner?.message = file?.message ?? ""

        // UTF-16 (mockup 06a).
        readOnlyBanner = banner(readOnlyBanner, key: model.isReadOnly ? "read-only" : nil) { key in
            makeBanner(
                kind: .info,
                message: String(
                    localized: "This file is UTF-16, which Leal can’t save. You can edit it, then save a UTF-8 copy.",
                    comment: "Banner on a UTF-16 file: it can be edited, and Save As UTF-8 is the only save (ADR-0013 decision 1, mockup 06a)"
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
        reload(.reload)
    }

    /// Reload, or **Revert to Saved** (task 2.5.3c, ADR-0008 decision 4),
    /// which is the same but for its question.
    func reload(_ reason: RereadReason) {
        scheduler.noteUserInput()
        // Opening the file reads it, which on a network share can block:
        // always off the main thread (task 2.0, ADR-0009). The window shows
        // the old snapshot until the new one is ready.
        guard reloading == nil, savingAsUTF8 == nil, !model.isReloading else { return }
        // Not while a save replaces the file (task 2.5.3a), or one is
        // queued or waits for its turn at the file (task 2.5.3c review).
        guard !isSaving else { return NSSound.beep() }
        // An edit still open is committed first (or, if the core refuses
        // it, stays open, and the file isn't read again).
        guard commitEditing() else { return NSSound.beep() }
        // Reload throws unsaved edits away: ask first (ADR-0008 decision 4).
        if model.hasUnsavedEdits, let confirm = confirmDiscardingEdits {
            confirm(reason) { [weak self] proceed in
                guard proceed, let self, reloading == nil, savingAsUTF8 == nil, !model.isReloading, !isSaving else { return }
                startReload()
            }
            return
        }
        startReload()
    }

    /// A save is under way, in the model or still queued in the
    /// `NSDocument` (`documentIsSaving`).
    private var isSaving: Bool { model.isSaving || documentIsSaving() }

    /// Reads the file again, off the main thread.
    private func startReload() {
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
            } catch is EditedDuringReload {
                self?.showEditedDuringReload()
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

    /// A Reload kept the window on the file it showed, because it was
    /// edited meanwhile (`EditedDuringReload`): say so.
    private func showEditedDuringReload() {
        let alert = NSAlert()
        alert.messageText = HistoryText.editedDuringReload(model.url.lastPathComponent)
        alert.informativeText = HistoryText.editedDuringReloadDetail
        present(alert)
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
        guard savingAsUTF8 == nil, commitEditing() else { return NSSound.beep() }
        model.setHeaderRow(header)
    }

    func treatAs(_ delimiter: Delimiter) {
        scheduler.noteUserInput()
        guard savingAsUTF8 == nil, commitEditing() else { return NSSound.beep() }
        model.treatAs(delimiter)
    }

    func reopen(encoding: TextEncoding) {
        scheduler.noteUserInput()
        guard savingAsUTF8 == nil, commitEditing() else { return NSSound.beep() }
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

    /// Whether the file can be read with another delimiter or encoding:
    /// as `canReinterpret`, and not while there are unsaved edits, which
    /// are tied to how the file is split (ADR-0008 decision 4).
    private var canChangeSplit: Bool {
        model.canChangeSplit && savingAsUTF8 == nil
    }

    /// Why Treat As and Reopen with Encoding are off, if for a reason the
    /// user can act on: Reload first, or save or revert the edits first.
    var splitReason: String? {
        if let reason = rereadReason { return reason }
        return model.hasUnsavedEdits && !model.isFailed ? StatusText.saveOrRevertFirst : nil
    }

    /// The suggestions read the file again another way too: off until a
    /// Reload, while there are unsaved edits, and while Save As UTF-8 is
    /// under way.
    private func updateSuggestionButtons() {
        for suggestion in [delimiterBanner, encodingBanner] {
            suggestion?.button?.isEnabled = canChangeSplit
            suggestion?.button?.toolTip = splitReason
        }
    }

    /// Why reading the file another way is off, if it is for a reason the
    /// user can act on: it changed while Leal read it, so Reload first. The
    /// core refuses to read it again then (`ChangedOnDisk`).
    private var rereadReason: String? {
        if model.isFailed { return nil }
        if model.changedOnDisk { return StatusText.reloadFirst }
        return model.isSaving ? StatusText.waitForSave : nil
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
            menuItem.toolTip = splitReason
            return canChangeSplit
        case #selector(reopenWithEncoding(_:)):
            let encoding = MainMenu.encoding(of: menuItem)
            menuItem.state = encoding == model.interpretation.encoding ? .on : .off
            menuItem.toolTip = splitReason
            return canChangeSplit
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
            // There must be a file to open again, and no Reload or save
            // under way.
            return !model.isFailed && !model.isReloading && reloading == nil && savingAsUTF8 == nil && !isSaving
                && model.original.state != .deleted && model.original.state != .unavailable
        case let action:
            if let command = Self.command(for: action) { return validateStructureItem(menuItem, command) }
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
        // An open edit is committed first, as a selection change does.
        guard commitEditing() else { return NSSound.beep() }
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

    func announce(_ text: String) {
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
        // An open edit is committed first, as a selection change does.
        guard commitEditing() else { return NSSound.beep() }
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
        guard bytes > askBeforeCopyBytes else {
            if let copy = prepareCopy(range, bytes: bytes) { place(copy) }
            return
        }
        confirmLargeCopy(bytes) { [weak self] go in
            guard go, let self, let copy = prepareCopy(range, bytes: bytes) else { return }
            place(copy)
        }
    }

    /// A copy made but not yet on the clipboard: Cut (task 2.6) makes it
    /// before its cells change and places it once they have.
    enum PreparedCopy {
        /// The text, read at once.
        case text(String)
        /// The core's job, building it off the main thread.
        case job(CopyJob)
    }

    /// Copies `range` (about `bytes` bytes): at once if it is small and the
    /// index has its rows, else as a P2 job in the core.
    func prepareCopy(_ range: CopyRange, bytes: UInt64) -> PreparedCopy? {
        if bytes <= Self.immediateCopyBytes, let text = model.copyCellsNow(range) { return .text(text) }
        return model.copyCells(range).map { .job($0) }
    }

    /// Puts `copy` on the clipboard: its text, or a promise of the job's.
    func place(_ copy: PreparedCopy) {
        copyTask = nil
        copyPromise = nil
        switch copy {
        case let .text(text):
            Clipboard.write(text, to: pasteboard)
        case let .job(job):
            copyPromise = Clipboard.promise(job, to: pasteboard)
            let waiter = job.job()
            // Only for tests to await: nothing cancels this task, since the
            // copy outlives the window (`CopyPromise`).
            copyTask = Task {
                try? await waiter.finish()
            }
        }
    }

    /// Lets go of `copy` without placing it: a job still running stops.
    func discard(_ copy: PreparedCopy) {
        if case let .job(job) = copy { job.cancel() }
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
        // An open edit is committed, as leaving it does; one the core
        // refuses goes. Neither editor's read of a long value goes on.
        commitEditing()
        discardEditing()
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
            // Hiding it commits its edit, as leaving it does. One the core
            // refuses is kept, to commit again when it shows.
            if commitInspector(refocus: false, confirmed: true) { inspectorEdit = nil }
        }
    }

    private func selectionChanged() {
        find.activeCellChanged(grid.activeCell)
        cellEditor.dismissNote()
        updateInspector()
    }

    /// Shows the active cell's whole value, read off the main thread. An
    /// edit in the inspector of the cell it showed is committed first.
    func updateInspector() {
        guard isInspectorShown else { return }
        // An edit the core refuses keeps the inspector on its cell, with
        // the text typed and the reason, until it commits or Esc.
        guard commitInspector(refocus: false, confirmed: true) else { return }
        inspectorEdit = nil
        inspectorLoading?.cancel()
        inspectorLoading = nil
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
            // The inspector edits what it shows: the core's full display
            // value (ADR-0008 decision 3), where the core allows an edit. A
            // missing (hatched) cell is edited in the grid.
            var editable: InspectorEdit?
            if case let .value(shown) = content, !isReplacingDocument, model.editRefusal(.cell(cell)) == nil {
                editable = InspectorEdit(cell: cell, original: shown.text, truncated: shown.truncated)
            }
            showInInspector(column: column, row: cell.row + 1, content: content, edit: editable)
        }
    }

    private func showInInspector(column: String, row: Int?, content: InspectorContent, edit: InspectorEdit? = nil) {
        inspectorContent = content
        inspectorEdit = edit
        inspector.textView.lineBreak = model.lineBreak
        inspector.show(column: column, row: row, content: content, editable: edit != nil)
    }

    // MARK: Editing (task 2.5.1)

    /// Return, a double-click or typing in the grid: the in-cell editor on
    /// the active cell.
    func editActiveCell(typing: NSEvent? = nil) {
        guard let cell = grid.activeCell, !model.isFailed else { return }
        guard !isReplacingDocument else { return NSSound.beep() }
        cellEditor.begin(.cell(cell), typing: typing)
    }

    /// A Reload is replacing the document shown: editing (the in-cell
    /// editor, the inspector, Rename Column) and undo are off meanwhile, as
    /// an edit made now would be thrown away with the old document.
    /// (`DocumentModel.reloadInBackground` also won't adopt the new one
    /// over an edit made all the same.) Save As UTF-8 isn't one (task
    /// 2.5.3c): as during any save, an edit made meanwhile carries over to
    /// the copy's reading, unsaved.
    var isReplacingDocument: Bool {
        reloading != nil || model.isReloading
    }

    /// A column header's context menu: "Rename Column…", which edits the
    /// header row's cell in place (docs/tasks/2.1.md, "The header row").
    func headerMenu(column: Int) -> NSMenu? {
        guard model.interpretation.header, !model.isFailed, !isReplacingDocument else { return nil }
        let menu = NSMenu()
        let item = NSMenuItem(title: EditText.renameColumn, action: #selector(renameColumn(_:)), keyEquivalent: "")
        item.target = self
        item.tag = column
        menu.addItem(item)
        return menu
    }

    @objc func renameColumn(_ sender: NSMenuItem) {
        rename(column: sender.tag)
    }

    /// Edits column `column`'s header-row cell in place.
    func rename(column: Int) {
        guard model.interpretation.header, !model.isFailed else { return }
        guard !isReplacingDocument else { return NSSound.beep() }
        cellEditor.begin(.header(column: column))
    }

    /// ⌘↩ in the inspector (or leaving it): commits its text to the cell it
    /// shows. As in the in-cell editor, an untouched value is no edit, and
    /// a character the encoding can't hold is named first. `confirmed`
    /// (leaving it, another cell, hiding it) doesn't wait on that check: a
    /// value too long to have been checked as it was typed is checked,
    /// committed, and then the character is named. Returns whether no
    /// edit is left: one the core refuses keeps its text, and says why.
    @discardableResult
    func commitInspector(refocus: Bool, confirmed: Bool = false) -> Bool {
        guard var edit = inspectorEdit else { return true }
        let value = inspector.textView.string
        guard edit.changed, !value.isIdentical(to: edit.original) else {
            if refocus { view.window?.makeFirstResponder(grid.gridView) }
            return true
        }
        var named: UnencodableCharacter?
        if !value.isIdentical(to: edit.warned) {
            if !confirmed, let bad = model.unencodable(value) {
                edit.warned = value
                inspectorEdit = edit
                inspector.showWarning(EditText.unencodable(bad))
                return false
            }
            if confirmed, value.utf16.count > CellEditController.liveCheckLimit {
                named = model.unencodable(value)
            }
        }
        // Not changed any more: the commit below redraws the inspector
        // from the cell.
        edit.changed = false
        inspectorEdit = edit
        if case let .refused(refusal) = model.setCell(.cell(edit.cell), to: value) {
            // The typed text stays, to commit again or cancel.
            edit.changed = true
            inspectorEdit = edit
            NSSound.beep()
            inspector.showWarning(EditText.refusal(refusal))
            return false
        }
        if let named {
            // Committed, as leaving it does: Save will refuse it.
            NSSound.beep()
            inspector.showWarning(EditText.unencodable(named))
        }
        if refocus { view.window?.makeFirstResponder(grid.gridView) }
        return true
    }

    /// Commits an open edit, in the in-cell editor or the inspector, as
    /// leaving it would (the encoding check doesn't stop it). Returns
    /// whether no edit is left open: one the core refuses stays open, with
    /// its text and the reason. (`NSEditor`'s, which `NSViewController`
    /// has.)
    ///
    /// Reload, Treat As, Reopen with Encoding, ⌘G, Go to Row and closing
    /// the window call it first. SEAM(2.5.2, 2.5.3): so must anything that
    /// asks whether there are unsaved changes (dirty state, `canClose`,
    /// Save, Revert), before it asks, so the edit being typed counts.
    @discardableResult
    override func commitEditing() -> Bool {
        let cell = cellEditor.commitOpenEdit()
        let inspector = commitInspector(refocus: false, confirmed: true)
        return super.commitEditing() && cell && inspector
    }

    /// Closes both editors, committing nothing, and stops their reads of
    /// a long value.
    override func discardEditing() {
        super.discardEditing()
        cellEditor.abandon()
        inspectorEdit = nil
        inspectorLoading?.cancel()
        inspectorLoading = nil
    }

    /// Esc in the inspector: its text goes back to the cell's value.
    func cancelInspector() {
        guard let edit = inspectorEdit else { return }
        inspectorLoading?.cancel()
        inspectorLoading = nil
        inspectorEdit = nil
        if edit.changed || edit.truncated {
            updateInspector()
        } else {
            inspectorEdit = edit
        }
        view.window?.makeFirstResponder(grid.gridView)
    }

    /// The inspector shows a long value's first 64,000 characters: before
    /// it can be edited it is read in full (ADR-0008 decision 3).
    private func loadWholeValueForEditing() {
        guard let edit = inspectorEdit, edit.truncated, inspectorLoading == nil else { return }
        inspector.showWarning(InspectorText.loadingWhole)
        inspectorLoading = Task { [weak self] in
            guard let self else { return }
            let start = await model.fullValueInBackground(.cell(edit.cell))
            guard !Task.isCancelled, grid.activeCell == edit.cell, inspectorEdit?.cell == edit.cell else { return }
            inspectorLoading = nil
            guard let value = start?.value else { return }
            inspectorEdit = InspectorEdit(cell: edit.cell, original: value, truncated: false)
            inspector.showWhole(value)
        }
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
    /// whether it saved. Without one, the model saves and adopts the
    /// copy's reading.
    var onSaveAsUTF8: ((URL) async throws -> Bool)?
    /// Stops the document's Saves under way (`CSVDocument.cancelSave`),
    /// and says whether there was one.
    var onCancelSave: (() -> Bool)?

    /// ⌘. or Escape while a Save runs, or waits to start (for its turn,
    /// for coordination with other apps, or for another app's save to
    /// settle), stops it (task 2.5.3a): nothing is written, and the edits
    /// stay unsaved. Otherwise as usual.
    override func cancelOperation(_ sender: Any?) {
        guard let onCancelSave, onCancelSave() else { return super.cancelOperation(sender) }
    }
    /// A Save As UTF-8, while it is under way. Reload, Treat As and Reopen
    /// with Encoding are off meanwhile: each would read the file again
    /// while the save replaces it.
    /// Its progress shows in the status bar, as Save's does (task 2.5.3a,
    /// `DocumentModel.save`).
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

    /// The alerts that came while the view had no window, shown in order
    /// when it has one again.
    private var queuedAlerts: [NSAlert] = []

    /// The name Save As UTF-8 suggests: the file's own, with "(UTF-8)".
    static func utf8CopyName(of url: URL) -> String {
        let stem = url.deletingPathExtension().lastPathComponent
        let suffix = String(localized: "UTF-8", comment: "Save As UTF-8: added to the suggested file name, as in \"people (UTF-8).csv\"")
        let name = "\(stem) (\(suffix))"
        return url.pathExtension.isEmpty ? name : "\(name).\(url.pathExtension)"
    }

    private func startSavingAsUTF8(to url: URL) {
        // An edit still open goes in the copy (or, refused, stays open,
        // and nothing is saved).
        guard commitEditing() else { return NSSound.beep() }
        let save = onSaveAsUTF8
        let model = model
        savingAsUTF8 = Task { [weak self] in
            do {
                if let save {
                    _ = try await save(url)
                } else {
                    _ = try await model.saveAsUTF8(to: url)
                }
            } catch let failure as SaveFailure {
                // Including the core refusing to start it
                // (`DocumentModel.saveAsUTF8`).
                self?.showSaveAsUTF8Failure(failure)
            } catch {
                // The window is closing (`savingAsUTF8?.cancel()`): no alert.
                Logger.document.error("Save As UTF-8 stopped: \(String(describing: error), privacy: .public)")
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
    func present(_ alert: NSAlert) {
        guard let window = view.window else {
            queuedAlerts.append(alert)
            return
        }
        showAlert(alert, window)
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        let alerts = queuedAlerts
        queuedAlerts = []
        for alert in alerts { present(alert) }
    }

    /// Shows `alert` as a sheet on `window`. Tests replace it, so that no
    /// sheet is shown.
    var showAlert: (_ alert: NSAlert, _ window: NSWindow) -> Void = { alert, window in
        alert.beginSheetModal(for: window)
    }

    /// The file banners' **Save As…** (task 2.5.3c): Save is refused
    /// (`DocumentModel.canSave`), or there is no file to save over, but the
    /// document may be saved elsewhere, through the `NSDocument`. If Leal
    /// couldn't read all of the file, the copy has the complete rows it
    /// has, and the save panel says so (ADR-0008 decision 6, ADR-0010).
    @objc func saveACopy(_ sender: Any?) {
        guard let onSaveAs else { return NSSound.beep() }
        onSaveAs()
    }
}

/// Why the file is read again with unsaved edits, which are asked about
/// first (ADR-0008 decision 4): a Reload, or Revert to Saved.
enum RereadReason: Sendable {
    case reload
    case revert
}

/// A document's window. `NSDocument` keeps its title, proxy icon, tabs and
/// restoration in step.
@MainActor
final class DocumentWindowController: NSWindowController, NSWindowDelegate {
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
        // For the document's undo manager (`windowWillReturnUndoManager`).
        window.delegate = self
        shouldCascadeWindows = true
        window.center()
        content.onReadOnlyChanged = { [weak self] readOnly in self?.showLock(readOnly) }
        content.updateReadOnly()
    }

    /// The window's undo manager is the document's edit history's (task
    /// 2.5.2): ⌘Z and ⇧⌘Z, and the Edit menu's "Undo Typing", reach the
    /// core's commands. Text fields keep their own while they are edited.
    func windowWillReturnUndoManager(_ window: NSWindow) -> UndoManager? {
        (document as? CSVDocument)?.history.undoManager
    }

    /// "orders.csv — Edited" while there are unsaved edits (mockup 05a).
    /// AppKit shows that itself only for documents that autosave in place,
    /// which Leal's don't (DESIGN §4.3); without it only the close button's
    /// dot would say so.
    override func windowTitle(forDocumentDisplayName displayName: String) -> String {
        (document as? NSDocument)?.isDocumentEdited == true ? HistoryText.editedTitle(displayName) : displayName
    }

    override func setDocumentEdited(_ dirtyFlag: Bool) {
        super.setDocumentEdited(dirtyFlag)
        synchronizeWindowTitleWithDocumentName()
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
            localized: "Leal can’t save UTF-16 files. Save As UTF-8 saves a copy with your edits.",
            comment: "Tooltip of the lock glyph by a UTF-16 file's title: Save is off (ADR-0013 decision 1)"
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

/// The inspector's edit of a cell (task 2.5.1).
struct InspectorEdit: Equatable {
    let cell: CellPosition
    /// The value shown when it opened: the whole value, or with
    /// `truncated` its first 64,000 characters, which must be read in full
    /// before an edit.
    var original: String
    var truncated: Bool
    /// The user changed the text since.
    var changed = false
    /// The text whose unencodable character was named.
    var warned: String?
}

/// The inspector's text view, as an editor (task 2.5.1).
extension DocumentViewController: NSTextViewDelegate {
    func textView(_ textView: NSTextView, shouldChangeTextIn affectedCharRange: NSRange, replacementString: String?) -> Bool {
        guard textView === inspector.textView, let edit = inspectorEdit else { return true }
        if isReplacingDocument {
            // A Reload or Save As UTF-8 would throw the edit away.
            NSSound.beep()
            return false
        }
        if edit.truncated {
            // Only the start is shown: read the whole value, then edit it.
            loadWholeValueForEditing()
            return false
        }
        return true
    }

    func textDidChange(_ notification: Notification) {
        guard notification.object as AnyObject? === inspector.textView, var edit = inspectorEdit else { return }
        edit.changed = true
        let text = inspector.textView.string
        if text.utf16.count <= CellEditController.liveCheckLimit {
            if let bad = model.unencodable(text) {
                edit.warned = text
                inspector.showWarning(EditText.unencodable(bad))
            } else {
                edit.warned = nil
                inspector.showWarning(nil)
            }
        }
        inspectorEdit = edit
    }

    func textDidEndEditing(_ notification: Notification) {
        guard notification.object as AnyObject? === inspector.textView, inspectorEdit?.changed == true else { return }
        commitInspector(refocus: false, confirmed: true)
    }
}
