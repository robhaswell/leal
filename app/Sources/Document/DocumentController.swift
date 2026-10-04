import AppKit
import LealFFI
import os
import UniformTypeIdentifiers

/// Leal's `NSDocumentController` (task 2.0): it opens each file's core
/// document, which reads the file for first paint, **off the main thread**,
/// before AppKit makes the `NSDocument` on the main thread.
///
/// A file on a network share must never be read on the main thread, where a
/// share that stops answering would freeze the app (ADR-0009). AppKit's own
/// way to read off the main thread (`canConcurrentlyReadDocuments`) makes the
/// `NSDocument` on a background queue, which Swift 6 forbids for a
/// main-actor class (the app traps). So every open that comes through the
/// controller (Finder, File ▸ Open and Open Recent, Reopen after a failure,
/// restoring windows) first runs `DocumentModel.openCore` on `FileWork`'s
/// queue, with the other facts AppKit would ask the file system for on the
/// main thread (the file's type and modification date), and leaves them in
/// `PendingOpens`. `CSVDocument` takes them there instead of opening the
/// file itself. Every file goes this way, not only shares: the open is the
/// same either way, a few milliseconds, and telling a share from a local
/// file would itself ask the volume.
///
/// An open of a file already being opened waits for that one, and an open
/// that takes longer than half a second shows "Opening…"
/// (`OpeningIndicator`).
///
/// The first instance of `NSDocumentController` is the shared one, so
/// `main.swift` makes this one before the app runs.
final class DocumentController: NSDocumentController {
    typealias Completion = (NSDocument?, Bool, (any Error)?) -> Void

    /// Opens under way, by `PendingOpens.key`: the callers that asked for
    /// the same file meanwhile, and whether each wanted it shown.
    private var waiting: [String: [(display: Bool, completion: Completion)]] = [:]

    override func openDocument(
        withContentsOf url: URL,
        display displayDocument: Bool,
        completionHandler: @escaping Completion
    ) {
        openCoreFirst(url, display: displayDocument, completion: completionHandler) { done in
            super.openDocument(withContentsOf: url, display: displayDocument, completionHandler: done)
        }
    }

    override func reopenDocument(
        for urlOrNil: URL?,
        withContentsOf contentsURL: URL,
        display displayDocument: Bool,
        completionHandler: @escaping Completion
    ) {
        openCoreFirst(contentsURL, display: displayDocument, completion: completionHandler) { done in
            super.reopenDocument(for: urlOrNil, withContentsOf: contentsURL, display: displayDocument, completionHandler: done)
        }
    }

    // MARK: Quitting with unsaved edits (task 2.5.2)

    /// Stands in for AppKit's review once the open edits are committed, for
    /// tests: with two or more edited documents AppKit asks in an app-modal
    /// alert. `answer` is the review's answer: whether to go on quitting.
    var reviewForTesting: ((_ answer: @escaping @MainActor (Bool) -> Void) -> Void)?

    /// Answers the review `shouldTerminate` started.
    private var quitReply: ((Bool) -> Void)?

    /// Commits every document's open edit (in the in-cell editor or the
    /// inspector). Returns whether none is left open: one the core refuses
    /// stays open, saying why.
    func commitOpenEdits() -> Bool {
        documents.compactMap { $0 as? CSVDocument }.reduce(true) { $1.commitEditing() && $0 }
    }

    /// **Quit** with an edit still being typed (task 2.5.2 review). Quit
    /// (and Log Out and Restart) reviews the documents with unsaved
    /// changes before it asks the app delegate's `applicationShouldTerminate`,
    /// but only if `hasEditedDocuments` says there are some, and an edit
    /// not yet committed isn't one. So the app delegate asks here: every
    /// open edit is committed, and if that left a document edited that
    /// wasn't before, the documents are reviewed now (Save, Don't Save,
    /// Cancel), and `reply` hears whether to quit
    /// (`NSApplication.reply(toApplicationShouldTerminate:)`). A document
    /// edited before was reviewed already, by AppKit, which committed every
    /// open edit (`reviewUnsavedDocuments`). An open edit the core refuses
    /// stops the quit, as it stops a close.
    ///
    /// `reply` is never called before this returns, whatever the review
    /// does (it may answer at once: an open edit the core refuses, a test's
    /// stand-in): `NSApplication.reply` must come after `.terminateLater`,
    /// so the answer is delivered on the main queue's next turn. A quit
    /// already waiting for its answer keeps it: a second terminate neither
    /// replaces its `reply` nor starts a second review, and waits for it.
    func shouldTerminate(reply: @escaping (Bool) -> Void) -> NSApplication.TerminateReply {
        guard quitReply == nil else { return .terminateLater }
        let csv = documents.compactMap { $0 as? CSVDocument }
        let editedBefore = csv.filter(\.isDocumentEdited)
        guard commitOpenEdits() else {
            NSSound.beep()
            return .terminateCancel
        }
        let newlyEdited = csv.contains { document in
            document.isDocumentEdited && !editedBefore.contains { $0 === document }
        }
        guard newlyEdited else { return .terminateNow }
        quitReply = reply
        reviewUnsavedDocuments(
            withAlertTitle: nil,
            cancellable: true,
            delegate: self,
            didReviewAllSelector: #selector(quitReviewEnded(_:didReviewAll:contextInfo:)),
            contextInfo: nil
        )
        return .terminateLater
    }

    @objc private func quitReviewEnded(_ controller: NSDocumentController, didReviewAll: Bool, contextInfo: UnsafeMutableRawPointer?) {
        // The next turn, not now: the review may have ended before
        // `shouldTerminate` returned `.terminateLater`. The reply stays
        // pending until then, so a second terminate can't overwrite it.
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated {
                guard let self, let reply = self.quitReply else { return }
                self.quitReply = nil
                reply(didReviewAll)
            }
        }
    }

    /// AppKit reviews the documents with unsaved changes (Quit, when some
    /// are edited) by `hasEditedDocuments`, before any document's
    /// `canClose` runs, so an edit still being typed in another document
    /// wouldn't count. Every document commits its open edit first. One
    /// the core refuses stays open, saying why, and stops a review that
    /// can be cancelled, as it stops a close.
    override func reviewUnsavedDocuments(
        withAlertTitle title: String?,
        cancellable: Bool,
        delegate: Any?,
        didReviewAllSelector: Selector?,
        contextInfo: UnsafeMutableRawPointer?
    ) {
        let committed = commitOpenEdits()
        let answer: @MainActor (Bool) -> Void = { [weak self] reviewed in
            guard let self else { return }
            Self.answer(delegate, didReviewAllSelector, controller: self, didReviewAll: reviewed, contextInfo: contextInfo)
        }
        if !committed, cancellable {
            NSSound.beep()
            return answer(false)
        }
        if let review = reviewForTesting {
            return review(answer)
        }
        super.reviewUnsavedDocuments(
            withAlertTitle: title,
            cancellable: cancellable,
            delegate: delegate,
            didReviewAllSelector: didReviewAllSelector,
            contextInfo: contextInfo
        )
    }

    /// Calls the review's delegate, as AppKit does:
    /// `documentController:didReviewAll:contextInfo:`.
    private static func answer(
        _ delegate: Any?,
        _ selector: Selector?,
        controller: NSDocumentController,
        didReviewAll: Bool,
        contextInfo: UnsafeMutableRawPointer?
    ) {
        guard let selector, let object = delegate as? NSObject, object.responds(to: selector) else { return }
        typealias Callback = @convention(c) (NSObject, Selector, NSDocumentController, Bool, UnsafeMutableRawPointer?) -> Void
        let callback = unsafeBitCast(object.method(for: selector), to: Callback.self)
        callback(object, selector, controller, didReviewAll, contextInfo)
    }

    /// The file's type from its name, so AppKit needn't ask the file system
    /// on the main thread; failing that, the type the background open read
    /// (`PendingOpens`); only then AppKit's own answer.
    override func typeForContents(of url: URL) throws -> String {
        if let type = UTType(filenameExtension: url.pathExtension.lowercased()) {
            for declared in [UTType.commaSeparatedText, .tabSeparatedText] where type.conforms(to: declared) {
                return declared.identifier
            }
        }
        if let type = PendingOpens.typeIdentifier(for: url) {
            return type
        }
        return try super.typeForContents(of: url)
    }

    /// Makes the document as `NSDocument.init(contentsOf:ofType:)` would
    /// (read it, then set its URL, type and modification date), but with the
    /// modification date the background open read, so the main thread
    /// doesn't `stat` the file.
    override func makeDocument(withContentsOf url: URL, ofType typeName: String) throws -> NSDocument {
        try makeCSVDocument(at: url, contents: url, ofType: typeName)
            ?? super.makeDocument(withContentsOf: url, ofType: typeName)
    }

    /// As `makeDocument(withContentsOf:ofType:)`, for restoring a window.
    /// An autosaved copy in place of the file (the two URLs differ) is left
    /// to AppKit; Leal doesn't autosave.
    override func makeDocument(for urlOrNil: URL?, withContentsOf contentsURL: URL, ofType typeName: String) throws -> NSDocument {
        if urlOrNil == contentsURL, let document = try makeCSVDocument(at: contentsURL, contents: contentsURL, ofType: typeName) {
            return document
        }
        return try super.makeDocument(for: urlOrNil, withContentsOf: contentsURL, ofType: typeName)
    }

    /// A `CSVDocument` for `url`, or `nil` for another type.
    private func makeCSVDocument(at url: URL, contents: URL, ofType typeName: String) throws -> NSDocument? {
        guard documentClass(forType: typeName) == CSVDocument.self else { return nil }
        let document = CSVDocument()
        try document.read(from: contents, ofType: typeName)
        document.fileURL = url
        document.fileType = typeName
        document.fileModificationDate = document.openedModificationDate
        return document
    }

    /// Opens `url`'s core document on `FileWork`'s queue, leaves it in
    /// `PendingOpens`, then calls `open` (AppKit's own open) on the main
    /// thread. A file already being opened waits for that open instead. A
    /// file that is already open, or an environment that can't be made,
    /// goes straight to `open`: AppKit shows the open document, or
    /// `CSVDocument` reports the error as it would.
    private func openCoreFirst(
        _ url: URL,
        display: Bool,
        completion: @escaping Completion,
        open: @escaping (@escaping Completion) -> Void
    ) {
        let key = PendingOpens.key(url)
        if waiting[key] != nil {
            waiting[key]?.append((display, completion))
            return
        }
        guard document(for: url) == nil else {
            open(completion)
            return
        }
        guard let environment = try? CSVDocument.environment() else {
            // `CSVDocument` reports why, as it would; it isn't an open that
            // missed the controller.
            PendingOpens.skip(url)
            open(completion)
            return
        }
        waiting[key] = []
        // The open to first rows budget (DESIGN §1) counts from here, the
        // background open included.
        let started = CACurrentMediaTime()
        let opening = Signposts.opening()
        let indicator = OpeningIndicator.show(for: url)
        Task { @MainActor in
            // One hop, the facts first: the modification date is read
            // before the core's snapshot, as a Reload reads it, so a change
            // in between is still a change by the time Leal saves.
            let (facts, core) = await FileWork.run {
                (PendingOpens.Facts(of: url), Result { try DocumentModel.openCore(url: url, environment: environment) })
            }
            indicator.finish()
            PendingOpens.put(core, facts: facts, started: started, opening: opening, environment: environment, for: url)
            open { document, wasOpen, error in
                MainActor.assumeIsolated {
                    PendingOpens.discard(url)
                    completion(document, wasOpen, error)
                    for joiner in self.waiting.removeValue(forKey: key) ?? [] {
                        if joiner.display, let document {
                            if document.windowControllers.isEmpty { document.makeWindowControllers() }
                            document.showWindows()
                        }
                        joiner.completion(document, document != nil, error)
                    }
                }
            }
        }
    }
}

/// Core documents `DocumentController` opened off the main thread, waiting
/// for the `CSVDocument` AppKit makes for them (task 2.0, ADR-0009).
@MainActor
enum PendingOpens {
    /// What the background open found out about the file, which AppKit
    /// would otherwise ask the file system for on the main thread.
    struct Facts: Sendable {
        /// Its modification date, read before the core's snapshot.
        /// SEAM(2.5): `nil` if the `stat` failed; Save's check before writing
        /// (ADR-0008 decision 9) must then look at the file afresh rather
        /// than take it as unchanged.
        let modified: Date?
        /// Its type identifier, for a file whose name doesn't say CSV.
        let typeIdentifier: String?

        /// Reads them. It touches the file: call it off the main thread.
        nonisolated init(of url: URL) {
            let path = url.path(percentEncoded: false)
            modified = (try? FileManager.default.attributesOfItem(atPath: path))?[.modificationDate] as? Date
            typeIdentifier = try? url.resourceValues(forKeys: [.typeIdentifierKey]).typeIdentifier
        }
    }

    /// One open, waiting: the core's document (or why it couldn't be
    /// opened), the file's facts, when the open began and its "Open to first
    /// rows" signpost (DESIGN §1), and the environment it was opened with.
    final class Entry {
        let core: Result<DocumentModel.OpenedCore, any Error>
        let facts: Facts
        let started: CFTimeInterval
        let opening: OSSignpostIntervalState
        let environment: DocumentEnvironment
        /// Let go of (its signpost ended, its document released).
        fileprivate var released = false

        init(core: Result<DocumentModel.OpenedCore, any Error>, facts: Facts, started: CFTimeInterval, opening: OSSignpostIntervalState, environment: DocumentEnvironment) {
            self.core = core
            self.facts = facts
            self.started = started
            self.opening = opening
            self.environment = environment
        }
    }

    private static var entries: [String: Entry] = [:]
    /// Opens the controller passed straight to AppKit, on purpose (the
    /// environment couldn't be made), so `CSVDocument` opens them itself
    /// without calling that a missed open.
    private static var skipped: Set<String> = []

    /// The key for `url`: its standardized path. Only string work: it never
    /// touches the file system (the main thread looks entries up). AppKit
    /// makes the document for the URL it was asked to open, as given, so
    /// one key is enough.
    nonisolated static func key(_ url: URL) -> String {
        url.standardizedFileURL.path(percentEncoded: false)
    }

    static func put(
        _ core: Result<DocumentModel.OpenedCore, any Error>,
        facts: Facts,
        started: CFTimeInterval,
        opening: OSSignpostIntervalState,
        environment: DocumentEnvironment,
        for url: URL
    ) {
        let entry = Entry(core: core, facts: facts, started: started, opening: opening, environment: environment)
        release(entries.updateValue(entry, forKey: key(url)))
    }

    /// The open waiting for `url`, which the caller now owns.
    static func take(_ url: URL) -> Entry? {
        entries.removeValue(forKey: key(url))
    }

    /// The controller passes `url` to AppKit without opening it first.
    static func skip(_ url: URL) {
        skipped.insert(key(url))
    }

    /// Whether the controller passed `url` on without opening it first (and
    /// forget it).
    static func wasSkipped(_ url: URL) -> Bool {
        skipped.remove(key(url)) != nil
    }

    /// The type the background open read for `url`, if one is waiting.
    static func typeIdentifier(for url: URL) -> String? {
        entries[key(url)]?.facts.typeIdentifier
    }

    /// Lets go of an open nobody took: AppKit showed a document that was
    /// already open, or failed before making one.
    static func discard(_ url: URL) {
        release(take(url))
    }

    /// Ends an unused open's signpost and releases its core document off
    /// the main thread (`CoreRelease`), which deletes its snapshot. Once.
    private static func release(_ entry: Entry?) {
        guard let entry, !entry.released else { return }
        entry.released = true
        Signposts.firstRows(entry.opening)
        if case let .success(core) = entry.core {
            core.release()
        }
    }
}

/// "Opening “name”…", for an open that takes longer than `delay` (a slow or
/// hung network share, task 2.0): a small panel that doesn't take the
/// focus, with a spinner, gone once the open is done.
@MainActor
final class OpeningIndicator {
    /// How long an open may take before the indicator shows. Tests shorten
    /// it.
    static var delay: Duration = .milliseconds(500)
    /// Indicators on screen now, for tests.
    private(set) static var shown: [String] = []

    private var panel: NSPanel?
    private var waiting: Task<Void, Never>?
    private var name = ""
    /// How far each panel already showing moves a new one down and right,
    /// so several slow opens don't hide each other.
    private static let stagger: CGFloat = 24

    static func show(for url: URL) -> OpeningIndicator {
        let indicator = OpeningIndicator()
        indicator.name = url.lastPathComponent
        indicator.waiting = Task { @MainActor [weak indicator] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            indicator?.present()
        }
        return indicator
    }

    /// The open is done: no indicator, or none any more.
    func finish() {
        waiting?.cancel()
        waiting = nil
        guard let panel else { return }
        panel.close()
        self.panel = nil
        if let index = Self.shown.firstIndex(of: name) { Self.shown.remove(at: index) }
    }

    private func present() {
        let panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 300, height: 56),
            styleMask: [.titled, .utilityWindow, .nonactivatingPanel],
            backing: .buffered,
            defer: true
        )
        panel.title = String(localized: "Opening", comment: "Title of the small panel shown while a slow file opens (task 2.0)")
        panel.isReleasedWhenClosed = false
        panel.hidesOnDeactivate = false
        panel.becomesKeyOnlyIfNeeded = true
        let spinner = NSProgressIndicator()
        spinner.style = .spinning
        spinner.controlSize = .small
        spinner.startAnimation(nil)
        let label = NSTextField(labelWithString: String(
            localized: "Opening “\(name)”…",
            comment: "Shown while a file takes long to open, such as one on a slow network share (task 2.0); the file's name"
        ))
        label.lineBreakMode = .byTruncatingMiddle
        let row = NSStackView(views: [spinner, label])
        row.orientation = .horizontal
        row.spacing = 8
        row.edgeInsets = NSEdgeInsets(top: 12, left: 16, bottom: 12, right: 16)
        panel.contentView = row
        panel.center()
        let offset = Self.stagger * CGFloat(Self.shown.count)
        panel.setFrameOrigin(NSPoint(x: panel.frame.minX + offset, y: panel.frame.minY - offset))
        panel.orderFront(nil)
        self.panel = panel
        Self.shown.append(name)
        // VoiceOver says it: the panel doesn't take the focus.
        NSAccessibility.post(
            element: panel,
            notification: .announcementRequested,
            userInfo: [
                .announcement: label.stringValue,
                .priority: NSAccessibilityPriorityLevel.high.rawValue,
            ]
        )
    }
}
