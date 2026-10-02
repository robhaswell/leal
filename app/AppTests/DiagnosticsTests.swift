import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 1.7 against the real core, hosted in Leal.app: the diagnostics
/// banner and its details (mockups 03a and 03b), each kind's Previous and
/// Next past the report's first 1,000 locations, gutter markers and
/// hatched cells, Treat As and Reopen with Encoding, the review's
/// suggestions, and the removable-drive states (ADR-0006), simulated with
/// the core's test hooks.
@MainActor
final class DiagnosticsTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-diagnostics-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        environment = DocumentEnvironment(
            scheduler: try Scheduler(),
            temp: TempLocations(
                scratchDir: directory.appending(path: "scratch").path(percentEncoded: false),
                recordsDir: directory.appending(path: "records").path(percentEncoded: false)
            )
        )
        savedEnvironment = CSVDocument.environment
        let environment = environment!
        CSVDocument.environment = { environment }
    }

    override func tearDown() async throws {
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        DocumentModel.openForTesting = nil
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    private func file(_ name: String, _ bytes: Data) throws -> URL {
        let url = directory.appending(path: name)
        try bytes.write(to: url)
        return url
    }

    private func file(_ name: String, _ text: String) throws -> URL {
        try file(name, Data(text.utf8))
    }

    /// The corpus's diagnostics folder, copied into the test bundle.
    private var corpusFolder: URL {
        get throws {
            try XCTUnwrap(Bundle(for: Self.self).url(forResource: "diagnostics", withExtension: nil))
        }
    }

    /// A corpus file (`tests/corpus/diagnostics/`), copied to the test's
    /// folder.
    private func corpus(_ name: String) throws -> URL {
        let url = directory.appending(path: name)
        try FileManager.default.copyItem(at: try corpusFolder.appending(path: name), to: url)
        return url
    }

    private func corpusNames() throws -> [String] {
        try FileManager.default.contentsOfDirectory(atPath: try corpusFolder.path(percentEncoded: false))
            .filter { !$0.hasSuffix(".expected.toml") }
            .sorted()
    }

    private func open(_ url: URL) throws -> (CSVDocument, DocumentModel, DocumentViewController) {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        _ = controller.window
        return (document, try XCTUnwrap(document.model), controller.content)
    }

    private func waitUntil(_ what: String, timeout: TimeInterval = 20, _ condition: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() {
            if Date() > deadline {
                XCTFail("timed out waiting until \(what)")
                return
            }
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    /// Indexed, and the review finished.
    private func settle(_ model: DocumentModel) async throws {
        try await waitUntil("indexed") { model.isIndexComplete && model.diagnostics?.complete == true }
        try await waitUntil("reviewed") { model.review != nil }
    }

    // MARK: The banner (mockup 03a)

    func testAMessyFileShowsTheBannerUntilDismissedAndTheBadgeStays() async throws {
        let (document, model, content) = try open(try corpus("ragged-rows.csv"))
        try await settle(model)
        let banner = try XCTUnwrap(content.diagnosticsBanner)
        XCTAssertEqual(banner.message, "This file has 1 kind of irregularity. It’s shown exactly as written.")
        XCTAssertEqual(banner.button?.title, "Details")
        XCTAssertTrue(content.banners.arrangedSubviews.contains(banner))
        XCTAssertFalse(content.statusBar.badge.isHidden)
        XCTAssertEqual(content.statusBar.badge.title, "1")

        banner.dismiss(nil)
        XCTAssertNil(content.diagnosticsBanner)
        XCTAssertFalse(content.banners.arrangedSubviews.contains(banner))
        XCTAssertFalse(content.statusBar.badge.isHidden, "the badge stays (ADR-0002 question 7)")
        // More progress doesn't bring it back.
        content.updateBanners()
        XCTAssertNil(content.diagnosticsBanner)
        document.close()
    }

    func testTheBannerCountsKindsOfWarningAndErrorOnly() async throws {
        // Ragged, text after a quote, a NUL: three kinds; a blank line and
        // a mixed line ending are info only.
        let url = try file("messy.csv", Data("a,b\r\n1,2\r\n3\r\n\"x\"y,\0\r\n\r\n5,6\n".utf8))
        let (document, model, content) = try open(url)
        try await settle(model)
        XCTAssertEqual(content.diagnosticsBanner?.message, "This file has 3 kinds of irregularity. It’s shown exactly as written.")
        XCTAssertEqual(content.statusBar.badge.title, "3")
        // Info-level kinds are in the status bar (mockup 03a).
        let segments = StatusText.segments(model.status)
        XCTAssertTrue(segments.contains("Mixed line endings"), "\(segments)")
        XCTAssertTrue(segments.contains("Blank lines"), "\(segments)")
        document.close()
    }

    func testInfoOnlyAndCleanFilesShowNoBanner() async throws {
        for name in ["blank-lines.csv", "mixed-line-endings.csv", "bom-present.csv"] {
            let (document, model, content) = try open(try corpus(name))
            try await settle(model)
            XCTAssertFalse(model.diagnostics?.diagnostics.isEmpty ?? true, name)
            XCTAssertNil(content.diagnosticsBanner, name)
            XCTAssertTrue(content.statusBar.badge.isHidden, name)
            document.close()
        }
        let (document, model, content) = try open(try file("clean.csv", "a,b\n1,2\n"))
        try await settle(model)
        XCTAssertNil(content.diagnosticsBanner)
        XCTAssertTrue(content.banners.arrangedSubviews.isEmpty)
        document.close()
    }

    // MARK: The details popover (mockup 03b)

    /// For every file in the diagnostics corpus, the popover lists exactly
    /// the core report's kinds, warnings and errors first, with its counts.
    func testThePopoverListsTheCoreReportsKindsAndCounts() async throws {
        let names = try corpusNames()
        XCTAssertGreaterThanOrEqual(names.count, 13)
        for name in names {
            let (document, model, content) = try open(try corpus(name))
            try await settle(model)
            let report = try XCTUnwrap(model.diagnostics, name)
            content.showDetails(nil)
            let details = try XCTUnwrap(content.details, name)
            let warnings = report.diagnostics.filter { $0.severity != .info }
            let info = report.diagnostics.filter { $0.severity == .info }
            XCTAssertEqual(details.entries.map(\.kind), (warnings + info).map(\.kind), name)
            for diagnostic in report.diagnostics {
                let entry = try XCTUnwrap(details.entry(diagnostic.kind), name)
                XCTAssertEqual(entry.title, DiagnosticsText.title(diagnostic.kind, encoding: model.interpretation.encoding))
                if diagnostic.severity == .info {
                    XCTAssertEqual(entry.positionLabel.stringValue, "", name)
                } else {
                    XCTAssertEqual(entry.positionLabel.stringValue, "1 of \(diagnostic.count)", "\(name): \(diagnostic.kind)")
                }
                if diagnostic.count > 1, diagnostic.kind != .bomPresent, diagnostic.kind != .unterminatedQuote {
                    XCTAssertTrue(entry.subtitle.contains(Int(diagnostic.count).formatted()), "\(name): \(entry.subtitle)")
                }
            }
            document.close()
        }
    }

    func testThePopoversWordsFollowTheMockup() async throws {
        let (document, model, content) = try open(try corpus("ragged-rows.csv"))
        try await settle(model)
        content.showDetails(nil)
        let details = try XCTUnwrap(content.details)
        let ragged = try XCTUnwrap(details.entry(.raggedRows))
        XCTAssertEqual(ragged.title, "Ragged rows")
        // Five rows: a header and two common rows have 3 fields.
        XCTAssertEqual(ragged.subtitle, "2 rows have a different number of fields to the other 3")
        XCTAssertEqual(ragged.positionLabel.stringValue, "1 of 2")
        document.close()

        let (other, otherModel, otherContent) = try open(try corpus("text-after-closing-quote.csv"))
        try await settle(otherModel)
        otherContent.showDetails(nil)
        let quote = try XCTUnwrap(otherContent.details?.entry(.textAfterClosingQuote))
        XCTAssertEqual(quote.title, "Text after closing quote")
        XCTAssertTrue(quote.subtitle.hasPrefix("1 cell, at row ") || quote.subtitle.contains("cells, the first at row"), quote.subtitle)
        other.close()
    }

    // MARK: Previous and Next (mockup 03b)

    /// A file with 3,600 ragged rows, 1,200 rows with a NUL and 1,200 with
    /// text after a closing quote: each kind's Next visits exactly its own
    /// rows, past the report's first 1,000, and Previous goes back.
    func testNextAndPreviousVisitEachKindsRowsPastTheFirstThousand() async throws {
        var text = "id,name,notes\n"
        var ragged: [Int] = []
        var nul: [Int] = []
        for i in 0..<7_200 {
            if i % 2 == 0 {
                text += "\(i),short\n"
                ragged.append(i)
            } else if i % 3 == 0 {
                text += "\(i),n\u{0}l,x\n"
                nul.append(i)
            } else {
                text += "\(i),\"q\"after,y\n"
            }
        }
        let (document, model, content) = try open(try file("many.csv", text))
        try await settle(model)
        let report = try XCTUnwrap(model.diagnostics)
        XCTAssertEqual(report.diagnostics.first { $0.kind == .raggedRows }?.count, 3_600)
        XCTAssertEqual(report.diagnostics.first { $0.kind == .nulBytes }?.count, 1_200)
        content.showDetails(nil)

        // Every NUL row, in order (grid row i is data row i), each in the
        // field that has it.
        var visited: [Int] = []
        for _ in 0..<nul.count {
            await content.navigate(.nulBytes, forward: true).value
            let cell = try XCTUnwrap(content.grid.activeCell)
            XCTAssertEqual(cell.column, 1)
            visited.append(cell.row)
        }
        XCTAssertEqual(visited, nul)
        let entry = try XCTUnwrap(content.details?.entry(.nulBytes))
        XCTAssertEqual(entry.positionLabel.stringValue, "1,000+ of 1,200")
        XCTAssertTrue(entry.arrows.isEnabled(forSegment: 0))
        // Past the last: nothing moves.
        await content.navigate(.nulBytes, forward: true).value
        XCTAssertEqual(content.grid.activeCell?.row, nul.last)
        XCTAssertFalse(entry.arrows.isEnabled(forSegment: 1), "Next stops at the last, without wrapping")
        XCTAssertTrue(entry.arrows.isEnabled(forSegment: 0))

        // Previous, back across the 1,000th.
        for expected in nul.reversed().dropFirst().prefix(250) {
            await content.navigate(.nulBytes, forward: false).value
            XCTAssertEqual(content.grid.activeCell?.row, expected)
        }
        XCTAssertEqual(entry.positionLabel.stringValue, "950 of 1,200")

        // Ragged rows, a kind of their own: past the 1,000th too, in the
        // first missing cell.
        for index in 0..<1_100 {
            await content.navigate(.raggedRows, forward: true).value
            XCTAssertEqual(content.grid.activeCell, CellPosition(row: ragged[index], column: 2), "ragged row \(index + 1)")
        }
        let raggedEntry = try XCTUnwrap(content.details?.entry(.raggedRows))
        XCTAssertEqual(raggedEntry.positionLabel.stringValue, "1,000+ of 3,600")
        XCTAssertTrue(raggedEntry.isHighlighted)
        XCTAssertFalse(entry.isHighlighted)
        document.close()
    }

    func testTheFirstNextShowsTheFirstOccurrence() async throws {
        let (document, model, content) = try open(try corpus("ragged-rows.csv"))
        try await settle(model)
        content.showDetails(nil)
        let entry = try XCTUnwrap(content.details?.entry(.raggedRows))
        XCTAssertFalse(entry.arrows.isEnabled(forSegment: 0), "Previous starts off")
        XCTAssertTrue(entry.arrows.isEnabled(forSegment: 1))
        // Physical rows 2 and 3 are grid rows 1 and 2 (the header is row 0).
        entry.navigate(forward: true)
        try await waitUntil("moved") { content.grid.activeCell?.row == 1 }
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 1, column: 2))
        XCTAssertEqual(entry.positionLabel.stringValue, "1 of 2")
        await content.navigate(.raggedRows, forward: true).value
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 2, column: 3))
        XCTAssertEqual(entry.positionLabel.stringValue, "2 of 2")
        XCTAssertFalse(entry.arrows.isEnabled(forSegment: 1), "Next stops at the last")
        XCTAssertTrue(entry.arrows.isEnabled(forSegment: 0))
        await content.navigate(.raggedRows, forward: false).value
        XCTAssertEqual(content.grid.activeCell?.row, 1)
        document.close()
    }

    func testAnOccurrenceInTheHeaderRowShowsTheHeader() async throws {
        // Text after a closing quote in the header's second field.
        var text = "id,\"na\"me\n"
        for i in 0..<200 { text += "\(i),x\n" }
        let (document, model, content) = try open(try file("header.csv", text))
        try await settle(model)
        XCTAssertTrue(model.interpretation.header)
        content.view.layoutSubtreeIfNeeded()
        content.grid.select(CellPosition(row: 150, column: 0))
        XCTAssertGreaterThan(content.grid.scrollView.contentView.bounds.minY, 0)
        content.showDetails(nil)
        await content.navigate(.textAfterClosingQuote, forward: true).value
        XCTAssertNil(content.grid.activeCell, "no data row is selected for the header's occurrence")
        XCTAssertEqual(content.grid.scrollView.contentView.bounds.minY, 0, "scrolled to the header")
        XCTAssertEqual(content.details?.entry(.textAfterClosingQuote)?.positionLabel.stringValue, "1 of 1")
        document.close()
    }

    /// A file with only info-level kinds has no banner or badge, but its
    /// details are still reachable from its status-bar note.
    func testInfoOnlyDetailsOpenFromTheStatusBarNote() async throws {
        let (document, model, content) = try open(try corpus("blank-lines.csv"))
        try await settle(model)
        XCTAssertNil(content.diagnosticsBanner)
        XCTAssertTrue(content.statusBar.badge.isHidden)
        let note = try XCTUnwrap(content.statusBar.infoButton)
        XCTAssertEqual(note.title, "Blank lines")
        XCTAssertTrue(content.detailsAnchor === note)
        XCTAssertFalse(note.isHidden)
        note.performClick(nil)
        XCTAssertNotNil(content.detailsPopover)
        XCTAssertEqual(content.details?.entries.map(\.kind), [.blankLines])
        XCTAssertTrue(content.validateMenuItem(NSMenuItem(title: "", action: #selector(DocumentViewController.showDetails(_:)), keyEquivalent: "")))
        document.close()
    }

    func testTheMenuButtonsAndArrowsAreNamedForVoiceOver() async throws {
        let (document, model, content) = try open(try corpus("ragged-rows.csv"))
        try await settle(model)
        XCTAssertEqual(content.statusBar.delimiterButton?.accessibilityLabel(), "Delimiter: Comma, menu")
        XCTAssertEqual(content.statusBar.encodingButton?.accessibilityLabel(), "Encoding: UTF-8, menu")
        XCTAssertEqual(content.statusBar.delimiterButton?.accessibilityRole(), .popUpButton)
        content.showDetails(nil)
        let entry = try XCTUnwrap(content.details?.entry(.raggedRows))
        XCTAssertEqual(entry.arrowNames, ["Previous: Ragged rows", "Next: Ragged rows"])
        document.close()
    }

    // MARK: Gutter markers and hatched cells

    func testMarkedRowsHaveAGutterMarkerAndShortRaggedRowsAreHatched() async throws {
        let (document, model, content) = try open(try corpus("ragged-rows.csv"))
        try await settle(model)
        // Grid rows: 0 "1,2,3", 1 "4,5" (short), 2 "6,7,8,9" (long), 3.
        XCTAssertEqual((0..<4).map(model.rowHasMarker), [false, true, true, false])
        XCTAssertTrue(model.isHatched(row: 1, column: 2))
        XCTAssertFalse(model.isHatched(row: 0, column: 3), "the long row's extra column isn't hatched elsewhere")
        XCTAssertFalse(model.isHatched(row: 3, column: 3))

        let window = try XCTUnwrap(content.view.window)
        window.appearance = NSAppearance(named: .aqua)
        let grid = content.grid
        content.view.layoutSubtreeIfNeeded()
        let gutter = try render(grid.gutterView)
        XCTAssertGreaterThan(orangePixels(gutter, in: NSRect(x: 0, y: 22, width: 16, height: 22)), 4, "grid row 1's marker")
        XCTAssertEqual(orangePixels(gutter, in: NSRect(x: 0, y: 0, width: 16, height: 22)), 0, "grid row 0 has none")
        let cells = try render(grid.gridView)
        let hatched = grid.geometry.cellRect(row: 1, column: 2).insetBy(dx: 2, dy: 3)
        let blank = grid.geometry.cellRect(row: 0, column: 3).insetBy(dx: 2, dy: 3)
        XCTAssertGreaterThan(greyPixels(cells, in: hatched), 10, "the short row's missing cell is hatched")
        XCTAssertEqual(greyPixels(cells, in: blank), 0)
        document.close()
    }

    /// The gutter draws a marker for the rows its source marks, and only
    /// for them (drawn offscreen, as `GridRenderingTests` does).
    func testTheGutterDrawsMarkersForMarkedRows() throws {
        let source = MarkedSource(rows: 30, columns: 3, marked: [1, 4])
        let size = NSSize(width: 600, height: 26 + 20 * 22)
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.appearance = NSAppearance(named: .aqua)
        addTeardownBlock { window.close() }
        let grid = GridContainerView(frame: NSRect(origin: .zero, size: size))
        window.contentView = grid
        grid.dataSource = source
        grid.setColumnWidths([100, 100, 100])
        grid.layoutSubtreeIfNeeded()
        let gutter = try render(grid.gutterView)
        for row in 0..<6 {
            let pixels = orangePixels(gutter, in: NSRect(x: 0, y: CGFloat(row) * 22, width: 16, height: 22))
            if source.marked.contains(row) {
                XCTAssertGreaterThan(pixels, 4, "row \(row)'s marker")
            } else {
                XCTAssertEqual(pixels, 0, "row \(row) has none")
            }
        }
    }

    private func render(_ view: NSView) throws -> NSBitmapImageRep {
        let rect = view.bounds
        let rep = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: rect))
        view.cacheDisplay(in: rect, to: rep)
        return rep
    }

    private func pixels(_ rep: NSBitmapImageRep, in rect: NSRect, where test: (NSColor) -> Bool) -> Int {
        let scale = CGFloat(rep.pixelsWide) / rep.size.width
        var count = 0
        for y in Int(rect.minY * scale)..<Int(rect.maxY * scale) {
            for x in Int(rect.minX * scale)..<Int(rect.maxX * scale) {
                if let color = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), test(color) { count += 1 }
            }
        }
        return count
    }

    private func orangePixels(_ rep: NSBitmapImageRep, in rect: NSRect) -> Int {
        // Orange, after colour matching: much more red than blue. Grey text
        // and backgrounds have as much of each.
        pixels(rep, in: rect) { $0.redComponent > 0.7 && $0.redComponent - $0.blueComponent > 0.4 }
    }

    /// Pixels noticeably darker than the row backgrounds: the hatch lines.
    private func greyPixels(_ rep: NSBitmapImageRep, in rect: NSRect) -> Int {
        pixels(rep, in: rect) { $0.brightnessComponent < 0.9 }
    }

    // MARK: Treat As and Reopen with Encoding (ADR-0005 decision 8)

    func testTreatAsReindexesWithTheChosenDelimiter() async throws {
        let (document, model, content) = try open(try file("semi.csv", "a;b;c\n1;2,5;3\n4;5,5;6\n"))
        try await settle(model)
        XCTAssertEqual(model.interpretation.delimiter, .semicolon)
        XCTAssertEqual(model.columnCount, 3)
        let generation = model.generation
        XCTAssertEqual(content.statusBar.delimiterButton?.title, "Semicolon")

        // The status bar's menu.
        let menu = content.statusBar.treatAsMenu()
        XCTAssertEqual(menu.items.map(\.title), ["Treat As", "Comma", "Semicolon", "Tab", "Pipe"])
        XCTAssertEqual(menu.items[2].state, .on)
        menu.performActionForItem(at: 1)
        XCTAssertEqual(model.interpretation.delimiter, .comma)
        XCTAssertEqual(model.interpretation.delimiterSource, .user)
        XCTAssertGreaterThan(model.generation, generation)
        try await settle(model)
        XCTAssertEqual(model.diagnostics?.generation, model.generation, "new diagnostics for the new reading")
        // "a;b;c" is one field and "1;2,5;3" two: two columns, and ragged.
        XCTAssertEqual(model.fileColumnCount, 2)
        XCTAssertNotNil(model.diagnostics?.diagnostics.first { $0.kind == .raggedRows })
        XCTAssertEqual(content.statusBar.delimiterButton?.title, "Comma")

        // The View menu's Treat As.
        let item = NSMenuItem(title: "Tab", action: #selector(DocumentViewController.treatAsDelimiter(_:)), keyEquivalent: "")
        item.representedObject = DelimiterBox(.semicolon)
        XCTAssertTrue(content.validateMenuItem(item))
        XCTAssertEqual(item.state, .off)
        content.treatAsDelimiter(item)
        XCTAssertEqual(model.interpretation.delimiter, .semicolon)
        try await settle(model)
        XCTAssertEqual(model.columnCount, 3)
        XCTAssertNil(model.diagnostics?.diagnostics.first { $0.kind == .raggedRows })
        document.close()
    }

    func testReopenWithEncodingReindexes() async throws {
        let (document, model, content) = try open(try corpus("invalid-utf8.csv"))
        try await settle(model)
        XCTAssertEqual(model.interpretation.encoding, .utf8)
        XCTAssertNotNil(model.diagnostics?.diagnostics.first { $0.kind == .invalidEncoding })

        let menu = content.statusBar.reopenMenu()
        XCTAssertEqual(menu.items.first?.title, "Reopen with Encoding")
        let titles = menu.items.dropFirst().map(\.title)
        XCTAssertEqual(titles.count, 14, "every encoding but UTF-16, which needs a BOM")
        XCTAssertFalse(titles.contains("UTF-16 LE"))
        let index = try XCTUnwrap(menu.items.firstIndex { $0.title == "Windows-1252" })
        menu.performActionForItem(at: index)
        XCTAssertEqual(model.interpretation.encoding, .windows1252)
        XCTAssertEqual(model.interpretation.encodingSource, .user)
        try await settle(model)
        XCTAssertNil(model.diagnostics?.diagnostics.first { $0.kind == .invalidEncoding }, "every byte is Windows-1252")
        XCTAssertTrue(StatusText.segments(model.status).contains("Windows-1252 (chosen)"))

        // The File menu: UTF-16 isn't offered for a file without its BOM.
        let utf16 = NSMenuItem(title: "UTF-16 LE", action: #selector(DocumentViewController.reopenWithEncoding(_:)), keyEquivalent: "")
        utf16.representedObject = EncodingBox(.utf16Le)
        XCTAssertFalse(content.validateMenuItem(utf16))
        document.close()
    }

    func testReopenWithEncodingOverridesTheFilesAttribute() async throws {
        let url = try file("attr.csv", Data("name,city\nJos\u{E9},Gen\u{E8}ve\n".utf8))
        try setAttribute("com.apple.TextEncoding", "windows-1252;1280", on: url)
        let (document, model, content) = try open(url)
        try await settle(model)
        XCTAssertEqual(model.interpretation.encoding, .windows1252)
        XCTAssertEqual(model.interpretation.encodingSource, .attribute)
        XCTAssertTrue(StatusText.segments(model.status).contains("Windows-1252 (file attribute)"))
        content.reopen(encoding: .utf8)
        XCTAssertEqual(model.interpretation.encoding, .utf8)
        XCTAssertEqual(model.interpretation.encodingSource, .user)
        try await settle(model)
        XCTAssertEqual(model.cell(row: 0, column: 0), .text("José", truncated: false))
        document.close()
    }

    func testAnUnsupportedAttributeHasAStatusBarNote() async throws {
        let url = try file("japanese.csv", "a,b\n1,2\n")
        try setAttribute("com.apple.TextEncoding", "shift_jis;2561", on: url)
        let (document, model, _) = try open(url)
        try await settle(model)
        XCTAssertEqual(model.interpretation.notes, [.textEncodingUnsupported(cfStringEncoding: 2561)])
        XCTAssertTrue(StatusText.segments(model.status).contains("Encoding attribute ignored"))
        XCTAssertTrue(StatusText.help(model.status).contains("number 2561"))
        document.close()
    }

    private func setAttribute(_ name: String, _ value: String, on url: URL) throws {
        let data = Data(value.utf8)
        let result = data.withUnsafeBytes { bytes in
            setxattr(url.path(percentEncoded: false), name, bytes.baseAddress, bytes.count, 0, 0)
        }
        XCTAssertEqual(result, 0, "setxattr: \(errno)")
    }

    // MARK: Suggestions (DESIGN §3.2, ADR-0005 decision 4)

    func testTheEncodingSuggestionReopensTheFile() async throws {
        // ASCII for the first 64 KB, then Windows-1252.
        var bytes = Data(String(repeating: "id,name\n", count: 64 * 1024 / 8 + 100).utf8)
        bytes.append(contentsOf: Array("99,caf".utf8) + [0xE9, 0x0A])
        let (document, model, content) = try open(try file("late.csv", bytes))
        try await settle(model)
        XCTAssertEqual(model.interpretation.encoding, .utf8)
        let banner = try XCTUnwrap(content.encodingBanner)
        XCTAssertEqual(banner.message, "This file looks like Windows-1252.")
        XCTAssertEqual(banner.button?.title, "Reopen as Windows-1252")
        banner.button?.performClick(nil)
        XCTAssertEqual(model.interpretation.encoding, .windows1252)
        XCTAssertEqual(model.interpretation.encodingSource, .user)
        try await settle(model)
        XCTAssertNil(content.encodingBanner, "a chosen encoding isn't second-guessed")
        document.close()
    }

    func testTheDelimiterSuggestionSwitches() async throws {
        // One column with a comma in the first row for the first 64 KB,
        // then three times as many semicolon-separated rows.
        var text = "name,\n"
        while text.utf8.count < 64 * 1024 + 10 { text += "x\n" }
        for i in 0..<100_000 { text += "\(i);a;b\n" }
        let (document, model, content) = try open(try file("later.csv", text))
        try await settle(model)
        XCTAssertEqual(model.interpretation.delimiter, .comma)
        let banner = try XCTUnwrap(content.delimiterBanner)
        XCTAssertEqual(banner.message, "This file looks semicolon-separated.")
        XCTAssertEqual(banner.button?.title, "Switch")
        // Dismissing it keeps it away for this reading.
        banner.dismiss(nil)
        XCTAssertNil(content.delimiterBanner)
        content.updateBanners()
        XCTAssertNil(content.delimiterBanner)
        content.acceptDelimiterSuggestion(nil)
        XCTAssertEqual(model.interpretation.delimiter, .semicolon)
        try await settle(model)
        XCTAssertNil(content.delimiterBanner)
        document.close()
    }

    // MARK: Removable drives (ADR-0006, 1.1a)

    /// Opens files as if on a removable drive, with `fault` part-way
    /// through the copy (the core's test hooks).
    private func simulateDrive(_ fault: SimulatedFault?) {
        DocumentModel.openForTesting = { path, environment, options, observer in
            try debugOpenDocumentWithFault(
                path: path,
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer,
                chunkBytes: 4096,
                fault: fault
            )
        }
    }

    private func bigText(rows: Int) -> String {
        var text = "id,name\n"
        for i in 0..<rows { text += "\(i),name \(i)\n" }
        return text
    }

    func testADisconnectedDriveShowsItsBannerRefusesSaveAndOffersSaveAs() async throws {
        simulateDrive(.disconnect(at: 100_000))
        let (document, model, content) = try open(try file("usb.csv", bigText(rows: 20_000)))
        try await waitUntil("disconnected") { model.storage == .disconnected }
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, DiagnosticsText.disconnected)
        XCTAssertEqual(banner.button?.title, "Save As…")
        XCTAssertEqual(content.banners.arrangedSubviews.first, banner, "the drive's banner comes first")
        XCTAssertTrue(StatusText.segments(model.status).contains("Drive disconnected"))
        XCTAssertFalse(document.canSave)
        XCTAssertFalse(document.validateUserInterfaceItem(menuItem(#selector(NSDocument.save(_:)))))
        XCTAssertTrue(document.validateUserInterfaceItem(menuItem(#selector(NSDocument.saveAs(_:)))))
        // The rows read before the drive went stay readable.
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("name 0", truncated: false))
        XCTAssertGreaterThan(model.loadedRowCount, 1_000)
        document.close()
    }

    func testAFileThatChangesWhileReadShowsItsBannerAndRefusesSave() async throws {
        simulateDrive(.change(at: 50_000))
        let (document, model, content) = try open(try file("exfat.csv", bigText(rows: 20_000)))
        try await waitUntil("changed") { model.changedOnDisk }
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, DiagnosticsText.changedWhileReading)
        XCTAssertTrue(StatusText.segments(model.status).contains("Changed while reading"))
        XCTAssertFalse(document.canSave)
        XCTAssertFalse(document.validateUserInterfaceItem(menuItem(#selector(NSDocument.save(_:)))))
        XCTAssertTrue(document.validateUserInterfaceItem(menuItem(#selector(NSDocument.saveAs(_:)))))
        document.close()
    }

    func testARemovableFileIsReadThenWorkedFromACopy() async throws {
        simulateDrive(nil)
        let (document, model, content) = try open(try file("copied.csv", bigText(rows: 5_000)))
        try await settle(model)
        XCTAssertEqual(model.storage, .copy)
        XCTAssertTrue(StatusText.segments(model.status).contains("Working from a copy"))
        XCTAssertNil(content.driveBanner)
        XCTAssertTrue(document.canSave)
        document.close()
    }

    private func menuItem(_ action: Selector) -> NSMenuItem {
        NSMenuItem(title: "", action: action, keyEquivalent: "")
    }

    // MARK: The status bar's words

    func testStorageNotesAndTheirTooltips() {
        var status = StatusSummary(
            rows: 3, columns: 2, indexing: false, fractionIndexed: 1, delimiter: .comma, lineEnding: .lf,
            encoding: .utf8, encodingSource: .guess, header: true, headerSource: .guess, readOnly: false
        )
        let notes: [(SourceStorage, String, String)] = [
            (.reading, "Reading from the drive", "while it copies it to this Mac"),
            (.copy, "Working from a copy", "copied the file to this Mac"),
            (.disconnected, "Drive disconnected", "Save is off"),
            (.deleted, "Deleted", "deleted on another computer while Leal was reading it"),
            (.memory, "Read into memory", "into memory"),
        ]
        for (storage, segment, help) in notes {
            status.storage = storage
            XCTAssertEqual(StatusText.segments(status), ["3 rows × 2 columns", "Comma", "LF", "UTF-8", segment])
            XCTAssertTrue(StatusText.help(status).contains(help), "\(storage)")
        }
        // A file on a network share says so while it is read (ADR-0009).
        status.storage = .reading
        status.onNetworkShare = true
        XCTAssertEqual(StatusText.segments(status).last, "Reading from the network")
        XCTAssertTrue(StatusText.help(status).contains("on a network share"))
        // Deleted on its share: one "Deleted", not two.
        status.storage = .deleted
        status.original = .deleted
        XCTAssertEqual(StatusText.segments(status).filter { $0 == "Deleted" }.count, 1)
        status.original = .unchanged
        status.onNetworkShare = false
        status.storage = .clone
        XCTAssertEqual(StatusText.segments(status).count, 4)
        status.changedOnDisk = true
        XCTAssertEqual(StatusText.segments(status).last, "Changed while reading")
        // Replaced elsewhere mid-copy (a share's file, task 2.0 review): the
        // "Changed on disk" note stays beside it.
        status.original = .changed
        XCTAssertEqual(StatusText.segments(status).suffix(2), ["Changed while reading", "Changed on disk"])
        status.original = .unchanged
        status.changedOnDisk = false
        status.notes = [.interpretationNotSensible(delimiter: .pipe)]
        XCTAssertEqual(StatusText.segments(status).last, "Remembered settings ignored")
        XCTAssertTrue(StatusText.help(status).contains("(Pipe)"))
        status.infoKinds = [.mixedLineEndings, .bomPresent]
        XCTAssertEqual(StatusText.segments(status).suffix(2), ["Remembered settings ignored", "Mixed line endings"])
        XCTAssertEqual(StatusText.items(status)[1].role, .delimiter)
        XCTAssertEqual(StatusText.items(status)[3].role, .encoding)
    }

    func testKindPositions() {
        let at = { (row: UInt64) in DiagnosticLocation(row: row, offset: row * 10) }
        let all = Diagnostic(kind: .nulBytes, severity: .warning, count: 3, first: [at(2), at(2), at(9)])
        var navigation = KindNavigation()
        XCTAssertEqual(navigation.position(of: all), .at(1))
        XCTAssertFalse(navigation.canGoBack(all))
        XCTAssertTrue(navigation.canGoForward(all))
        XCTAssertEqual(navigation.nextStart(.nulBytes), 0)
        XCTAssertNil(navigation.previousEnd(.nulBytes))
        navigation.visit(.nulBytes, row: 2)
        XCTAssertEqual(navigation.position(of: all), .at(1))
        XCTAssertEqual(navigation.nextStart(.nulBytes), 3)
        navigation.visit(.nulBytes, row: 9)
        XCTAssertEqual(navigation.position(of: all), .at(3), "two in row 2, so row 9's is the third")
        XCTAssertFalse(navigation.canGoForward(all))
        XCTAssertTrue(navigation.canGoBack(all))
        // Past the listed locations.
        let many = Diagnostic(kind: .raggedRows, severity: .warning, count: 5_000, first: (0..<1_000).map { at(UInt64($0)) })
        navigation.visit(.raggedRows, row: 4_000)
        XCTAssertEqual(navigation.position(of: many), .beyond(known: 1_000))
        XCTAssertTrue(navigation.canGoForward(many))
        XCTAssertEqual(DiagnosticsText.position(.beyond(known: 1_000), count: 5_000), "1,000+ of 5,000")
        XCTAssertEqual(DiagnosticsText.position(.at(2), count: 3), "2 of 3")
        navigation.reset()
        XCTAssertNil(navigation.current)
    }
}

/// A grid source with some rows marked.
@MainActor
private final class MarkedSource: GridDataSource {
    let rowCount: Int
    let loadedRowCount: Int
    let columnCount: Int
    let marked: Set<Int>

    init(rows: Int, columns: Int, marked: Set<Int>) {
        rowCount = rows
        loadedRowCount = rows
        columnCount = columns
        self.marked = marked
    }

    func headerTitle(column: Int) -> HeaderTitle { HeaderTitle(text: "c\(column)", style: .name) }
    func isNumeric(column: Int) -> Bool { false }
    func cell(row: Int, column: Int) -> GridCell { .text("x", truncated: false) }
    func prepare(rows: Range<Int>, columns: Range<Int>) {}
    func rowHasMarker(_ row: Int) -> Bool { marked.contains(row) }
}
