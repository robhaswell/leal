import AppKit
import LealFFI

/// A document window's content (DESIGN §4.1): banners at the top, the grid,
/// and the status bar. It binds the grid to the `DocumentModel` and passes
/// the user's scrolling and typing to the core's scheduler.
@MainActor
final class DocumentViewController: NSViewController, NSMenuItemValidation {
    let model: DocumentModel
    private let scheduler: Scheduler
    let grid = GridContainerView()
    let statusBar = StatusBarView()
    /// The banners, top to bottom. SEAM(1.7): the diagnostics and drive
    /// banners join the UTF-16 one here.
    let banners = NSStackView()
    /// The document failed (DESIGN §3.9).
    var onFailure: (() -> Void)?

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
        model.onChange = { [weak self] change in self?.modelChanged(change) }
        if model.isReadOnly {
            showReadOnlyBanner()
        }
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
        case .failed:
            grid.invalidateContent()
            onFailure?()
        }
        statusBar.show(model.status)
    }

    // MARK: The Header row toggle (ADR-0002 question 13)

    private func setHeaderRow(_ header: Bool) {
        scheduler.noteUserInput()
        model.setHeaderRow(header)
    }

    /// View > Use First Row as Header.
    @objc func toggleHeaderRow(_ sender: Any?) {
        setHeaderRow(!model.interpretation.header)
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(toggleHeaderRow(_:)) {
            menuItem.state = model.interpretation.header ? .on : .off
            return !model.isFailed
        }
        return true
    }

    // MARK: UTF-16 (mockup 06a)

    private func showReadOnlyBanner() {
        let banner = BannerView(
            kind: .info,
            message: String(
                localized: "This file is UTF-16, so Leal shows it read-only. Save a UTF-8 copy to edit it.",
                comment: "Banner on a UTF-16 file (DESIGN §4.3, mockup 06a)"
            ),
            buttonTitle: String(localized: "Save As UTF-8…", comment: "Banner button on a UTF-16 file"),
            target: self,
            action: #selector(saveAsUTF8(_:))
        )
        banner.onDismiss = { [weak banner] in banner?.removeFromSuperview() }
        banners.addArrangedSubview(banner)
    }

    /// SEAM(2.3): Save As UTF-8 is built in task 2.3.
    @objc func saveAsUTF8(_ sender: Any?) {
        guard let window = view.window else { return }
        let alert = NSAlert()
        alert.messageText = String(localized: "Save As UTF-8 isn’t available yet.", comment: "Alert: the UTF-16 banner's button before task 2.3")
        alert.informativeText = String(
            localized: "A later version of Leal saves a UTF-8 copy of the file that you can edit.",
            comment: "Alert: the UTF-16 banner's button before task 2.3"
        )
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
