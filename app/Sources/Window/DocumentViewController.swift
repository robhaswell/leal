import AppKit
import LealFFI

/// A document window's content (DESIGN §4.1): banners at the top, the grid,
/// and the status bar. It binds the grid to the `DocumentModel` and passes
/// the user's scrolling and typing to the core's scheduler.
///
/// The banners, top to bottom: a removable drive that was disconnected or
/// whose file changed while it was read (ADR-0006), the UTF-16 notice
/// (mockup 06a), the diagnostics banner (03a), and the delimiter and
/// encoding suggestions (DESIGN §3.2, ADR-0005 decision 4). Each stays
/// until dismissed; reading the file again (Treat As, Reopen with
/// Encoding, the Header row toggle) starts them afresh.
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

    init(model: DocumentModel, scheduler: Scheduler) {
        self.model = model
        self.scheduler = scheduler
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    override func loadView() {
        let root = NSView(frame: NSRect(x: 0, y: 0, width: 1200, height: 752))
        banners.orientation = .vertical
        banners.spacing = 0
        banners.alignment = .width
        for view in [banners, grid, statusBar] as [NSView] {
            view.translatesAutoresizingMaskIntoConstraints = false
            root.addSubview(view)
        }
        // With no banners the stack has no height of its own; keep it at
        // zero rather than letting it take the grid's space.
        let noBanners = banners.heightAnchor.constraint(equalToConstant: 0)
        noBanners.priority = .defaultLow
        NSLayoutConstraint.activate([
            noBanners,
            banners.topAnchor.constraint(equalTo: root.topAnchor),
            banners.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            banners.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            grid.topAnchor.constraint(equalTo: banners.bottomAnchor),
            grid.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            grid.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            grid.bottomAnchor.constraint(equalTo: statusBar.topAnchor),
            statusBar.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            statusBar.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            statusBar.bottomAnchor.constraint(equalTo: root.bottomAnchor),
        ])
        view = root
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        let scheduler = scheduler
        grid.onUserInput = { scheduler.noteUserInput() }
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
        case .columns:
            grid.setColumnWidths(model.columnWidths)
            grid.invalidateContent()
        case .content:
            grid.setColumnWidths(model.columnWidths)
            grid.invalidateContent()
            grid.activeCell = model.rowCount > 0 ? CellPosition(row: 0, column: 0) : nil
            navigation.reset()
        case .failed:
            grid.invalidateContent()
            detailsPopover?.close()
            onFailure?()
        }
        updateBanners()
        statusBar.show(model.status)
        updateDetails()
    }

    // MARK: Banners

    /// Shows the banners the document's state calls for, in their order.
    func updateBanners() {
        if bannerGeneration != model.generation {
            // A new reading of the file: suggestions and the diagnostics
            // banner start afresh. The drive's state is the file's, not the
            // reading's.
            dismissed = dismissed.filter { $0.hasPrefix("drive") }
            bannerGeneration = model.generation
        }
        guard !model.isFailed else {
            for banner in [driveBanner, diagnosticsBanner, delimiterBanner, encodingBanner] { banner?.removeFromSuperview() }
            driveBanner = nil
            diagnosticsBanner = nil
            delimiterBanner = nil
            encodingBanner = nil
            return
        }

        // A removable drive (ADR-0006, 1.1a).
        let drive: (key: String, message: String)? = if model.changedOnDisk {
            ("drive-changed", DiagnosticsText.changedWhileReading)
        } else if model.storage == .disconnected {
            ("drive-disconnected", DiagnosticsText.disconnected)
        } else {
            nil
        }
        driveBanner = banner(
            driveBanner,
            key: drive?.key,
            make: { key in
                makeBanner(kind: .warning, message: drive?.message ?? "", button: DiagnosticsText.saveAs, action: #selector(saveACopy(_:)), key: key)
            }
        )
        driveBanner?.message = drive?.message ?? ""

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

        let order = [driveBanner, readOnlyBanner, diagnosticsBanner, delimiterBanner, encodingBanner].compactMap { $0 }
        if banners.arrangedSubviews != order {
            for view in banners.arrangedSubviews { banners.removeArrangedSubview(view); view.removeFromSuperview() }
            for view in order { banners.addArrangedSubview(view) }
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
        key: String
    ) -> BannerView {
        let banner = BannerView(kind: kind, message: message, buttonTitle: button, prominent: prominent, target: self, action: action)
        banner.identifier = NSUserInterfaceItemIdentifier(key)
        banner.onDismiss = { [weak self] in
            self?.dismissed.insert(key)
            self?.updateBanners()
        }
        return banner
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
        model.setHeaderRow(header)
    }

    func treatAs(_ delimiter: Delimiter) {
        scheduler.noteUserInput()
        model.treatAs(delimiter)
    }

    func reopen(encoding: TextEncoding) {
        scheduler.noteUserInput()
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

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case #selector(toggleHeaderRow(_:)):
            menuItem.state = model.interpretation.header ? .on : .off
            return !model.isFailed
        case #selector(treatAsDelimiter(_:)):
            menuItem.state = MainMenu.delimiter(of: menuItem) == model.interpretation.delimiter ? .on : .off
            return !model.isFailed
        case #selector(reopenWithEncoding(_:)):
            let encoding = MainMenu.encoding(of: menuItem)
            menuItem.state = encoding == model.interpretation.encoding ? .on : .off
            return !model.isFailed && encoding.map(model.interpretation.encodingChoices.contains) == true
        case #selector(showDetails(_:)):
            return model.diagnostics?.diagnostics.isEmpty == false
        default:
            return true
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
        guard model.diagnostics?.diagnostics.isEmpty == false else { return }
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

    /// SEAM(2.3): Save As UTF-8 is built in task 2.3.
    @objc func saveAsUTF8(_ sender: Any?) {
        showNotYet(
            String(localized: "Save As UTF-8 isn’t available yet.", comment: "Alert: the UTF-16 banner's button before task 2.3"),
            String(
                localized: "A later version of Leal saves a UTF-8 copy of the file that you can edit.",
                comment: "Alert: the UTF-16 banner's button before task 2.3"
            )
        )
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
        window.contentMinSize = NSSize(width: 480, height: 240)
        window.initialFirstResponder = content.grid.gridView
        window.tabbingMode = .preferred
        super.init(window: window)
        shouldCascadeWindows = true
        window.center()
        if model.isReadOnly {
            window.addTitlebarAccessoryViewController(Self.lockAccessory())
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
        image.frame = NSRect(x: 0, y: 0, width: 22, height: 22)
        let accessory = NSTitlebarAccessoryViewController()
        accessory.view = image
        accessory.layoutAttribute = .leading
        return accessory
    }
}
