import AppKit
import LealFFI
import XCTest

@testable import Leal

/// The removable-drive path (ADR-0006) in the sandboxed app, on real disk
/// images (phase 1 review, cons-12): a file on an ejectable volume is read
/// from the drive while it is copied to this Mac, then worked from the copy;
/// and a drive detached part-way through shows the disconnected banner
/// without a crash.
///
/// The sandbox stops the test host attaching images: `hdiutil create` and
/// `hdiutil attach` fail with "Device not configured" in it. The Leal
/// scheme's test pre-action starts `app/Scripts/disk-image-helper.sh`,
/// which attaches them from outside the sandbox when a test asks
/// (`HelperDiskImage`). Detaching does work in the sandbox, so the test
/// pulls the drive itself. The images are mounted inside the app's
/// container, where the host may reach them by path; a file a user opens on
/// a real drive comes with a sandbox extension for that file instead.
@MainActor
final class RemovableDriveTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?
    private var images: [HelperDiskImage] = []

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-removable-\(UUID().uuidString)")
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
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        CoreRelease.finish()
        // Every image is detached, whatever happened.
        for image in images { image.finish() }
        images.removeAll()
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    private func waitUntil(_ what: String, timeout: TimeInterval = 30, _ condition: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() {
            if Date() > deadline {
                XCTFail("timed out waiting until \(what)")
                return
            }
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    /// A CSV of `rows` data rows after a header, about 45 bytes a row: the
    /// row's number, then pseudo-random numbers (so it compresses only so
    /// far). Built as bytes: string interpolation is slow in Debug builds.
    private func csv(rows: Int) -> Data {
        var bytes = Array("id,name,amount,note\n".utf8)
        bytes.reserveCapacity(rows * 48)
        var value: UInt64 = 0x9E37_79B9_7F4A_7C15
        func append(_ number: UInt64) {
            var digits: [UInt8] = []
            var rest = number
            repeat {
                digits.append(UInt8(48 + rest % 10))
                rest /= 10
            } while rest > 0
            bytes += digits.reversed()
        }
        for row in 0..<rows {
            value = value &* 6_364_136_223_846_793_005 &+ 1_442_695_040_888_963_407
            append(UInt64(row))
            bytes += Array(",name ".utf8)
            append(value >> 40)
            bytes.append(UInt8(ascii: ","))
            append((value >> 20) % 100_000)
            bytes.append(UInt8(ascii: "."))
            append(value % 100)
            bytes += Array(",n".utf8)
            append(value >> 50)
            bytes.append(UInt8(ascii: "\n"))
        }
        return Data(bytes)
    }

    private func records() -> [String] {
        (try? FileManager.default.contentsOfDirectory(atPath: environment.temp.recordsDir)) ?? []
    }

    private func attach(fs: String, format: String, size: String?, file: String, contents: Data) async throws -> HelperDiskImage {
        let image = try await HelperDiskImage.attach(fs: fs, format: format, size: size, files: [file: contents])
        images.append(image)
        return image
    }

    // MARK: Streamed, then copied

    /// On an APFS and an exFAT drive: the volume is ejectable, so Leal
    /// treats it as removable; the sandbox lets it make its folder in the
    /// volume's `.TemporaryItems`; the file is read from the drive and
    /// copied to this Mac, then worked from the copy; the copy is recorded
    /// while the file is open and gone once it is closed; and nothing is
    /// left on the drive.
    func testAFileOnARemovableDriveIsCopiedThenWorkedFrom() async throws {
        let rows = 50_000
        let contents = csv(rows: rows)
        for fs in ["APFS", "ExFAT"] {
            let image = try await attach(fs: fs, format: "UDRW", size: "64m", file: "drive.csv", contents: contents)
            let url = image.root.appending(path: "drive.csv")
            let volume = TemporaryFolders.volume(for: url)
            XCTAssertEqual(volume.isEjectable, true, "\(fs): a disk image is ejectable")
            let folder = try XCTUnwrap(volume.folder, "\(fs): the sandbox lets Leal make a folder on the volume")
            XCTAssertTrue(folder.hasPrefix(image.root.path(percentEncoded: false)) || folder.contains("/.TemporaryItems/"), "\(fs): \(folder)")
            try? FileManager.default.removeItem(atPath: folder)

            let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
            document.makeWindowControllers()
            let model = try XCTUnwrap(document.model)
            XCTAssertTrue([.reading, .copy].contains(model.storage), "\(fs): read from the drive, not mapped (\(model.storage))")
            XCTAssertEqual(model.cell(row: 0, column: 0), .text("0", truncated: false))
            try await waitUntil("\(fs): copied and indexed") { model.isIndexComplete && model.storage == .copy }
            XCTAssertEqual(model.rowCount, rows)
            XCTAssertEqual(model.cell(row: rows - 1, column: 0), .text("\(rows - 1)", truncated: false))
            XCTAssertTrue(StatusText.segments(model.status).contains("Working from a copy"))
            XCTAssertTrue(document.canSave)
            XCTAssertFalse(records().isEmpty, "\(fs): the copy is recorded while the file is open")

            document.close()
            CoreRelease.finish()
            try await waitUntil("\(fs): the copy is removed") { records().isEmpty }
            let scratch = (try? FileManager.default.contentsOfDirectory(atPath: environment.temp.scratchDir)) ?? []
            XCTAssertEqual(scratch.filter { !$0.hasPrefix(".") }, [], "\(fs): the internal copy is deleted")
            let left = Self.leftovers(on: image.root)
            XCTAssertEqual(left, [], "\(fs): nothing of Leal's is left on the drive")
            XCTAssertEqual(try Data(contentsOf: url), contents, "\(fs): the file is untouched")
            try image.forceDetach()
        }
    }

    /// Leal's temporary folders on a volume (`NSIRD_…` in `.TemporaryItems`).
    private static func leftovers(on root: URL) -> [String] {
        let items = root.appending(path: ".TemporaryItems")
        guard let walker = FileManager.default.enumerator(atPath: items.path(percentEncoded: false)) else { return [] }
        return walker.compactMap { $0 as? String }.filter { ($0 as NSString).lastPathComponent.hasPrefix("NSIRD_") }
    }

    // MARK: Pulled out part-way

    /// A drive detached while Leal is still copying the file: the document
    /// shows the rows it had read, the disconnected banner and status note,
    /// Save is off, and nothing crashes, including closing it. The image is
    /// compressed (bzip2), so reading it is slow enough that the copy is
    /// still going when the drive goes.
    func testADriveDetachedMidCopyShowsDisconnected() async throws {
        let rows = 1_500_000
        let image = try await attach(fs: "ExFAT", format: "UDBZ", size: nil, file: "drive.csv", contents: csv(rows: rows))
        let url = image.root.appending(path: "drive.csv")
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let model = try XCTUnwrap(document.model)
        let content = try XCTUnwrap((document.windowControllers.first as? DocumentWindowController)?.content)
        _ = content.view
        XCTAssertEqual(model.storage, .reading, "streamed from the drive at first paint")
        XCTAssertEqual(model.cell(row: 0, column: 0), .text("0", truncated: false))

        // At once: the copy of a file this size takes a second or more.
        let pulled = Date()
        try await image.pull()
        let took = Date().timeIntervalSince(pulled)
        try await waitUntil("disconnected (detaching took \(took) s)") { model.storage == .disconnected || model.storage == .copy }
        XCTAssertEqual(model.storage, .disconnected, "the copy finished before the drive went (detaching took \(took) s)")
        XCTAssertFalse(model.isFailed)
        XCTAssertFalse(document.canSave)
        XCTAssertEqual(content.driveBanner?.message, DiagnosticsText.disconnected)
        XCTAssertTrue(StatusText.segments(model.status).contains("Drive disconnected"))
        XCTAssertLessThan(model.loadedRowCount, rows, "not all of it was read")
        XCTAssertEqual(model.cell(row: 0, column: 0), .text("0", truncated: false), "rows read are still shown")
        // Looking at the file again while the drive is away changes nothing.
        await model.checkOriginal()?.value
        XCTAssertEqual(model.storage, .disconnected)
        XCTAssertFalse(model.isFailed)

        document.close()
        CoreRelease.finish()
        try await waitUntil("the copy is removed") { records().isEmpty }
    }
}

/// A disk image attached for a test by `app/Scripts/disk-image-helper.sh`,
/// outside the sandbox (see `RemovableDriveTests`), with the same retries
/// and full error reports as the Rust tests' `DiskImage`.
struct HelperDiskImage {
    /// Where requests go: the container's temporary folder, which the
    /// helper watches.
    static let work = FileManager.default.temporaryDirectory.appending(path: "leal-disk-images")
    /// How many times `hdiutil detach` is tried, and the wait before the
    /// first retry (doubled for each one after).
    private static let attempts = 5
    private static let firstBackoff: TimeInterval = 0.25

    let folder: URL
    /// The whole disk the image is attached as, such as `/dev/disk7`.
    let device: String
    /// The mounted volume.
    var root: URL { folder.appending(path: "volume") }
    var image: URL { folder.appending(path: "volume.dmg") }

    /// Asks the helper for an image of `fs` (`APFS`, `ExFAT`) in `format`
    /// (`UDRW` read-write, `UDBZ` compressed and read-only) holding `files`,
    /// and waits until it is attached.
    static func attach(fs: String, format: String, size: String?, files: [String: Data]) async throws -> HelperDiskImage {
        guard helperIsRunning() else {
            throw NSError(domain: "HelperDiskImage", code: 1, userInfo: [
                NSLocalizedDescriptionKey: """
                No disk-image helper is running. The Leal scheme's test pre-action starts it \
                (app/Scripts/disk-image-helper.sh; its log is build/disk-image-helper.log): the sandboxed \
                test host can't attach disk images itself (hdiutil attach fails with "Device not configured").
                """,
            ])
        }
        // At most 11 characters: the exFAT volume name is the id.
        let id = "L" + UUID().uuidString.replacingOccurrences(of: "-", with: "").prefix(10)
        let folder = work.appending(path: id)
        let source = folder.appending(path: "source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: true)
        for (name, contents) in files {
            try contents.write(to: source.appending(path: name))
        }
        let request = "fs=\(fs) format=\(format)" + (size.map { " size=\($0)" } ?? "")
        // Written whole, then renamed, so the helper never reads half of it.
        try Data(request.utf8).write(to: folder.appending(path: "request.tmp"))
        try FileManager.default.moveItem(at: folder.appending(path: "request.tmp"), to: folder.appending(path: "request"))

        let ready = folder.appending(path: "ready")
        let deadline = Date().addingTimeInterval(180)
        while !FileManager.default.fileExists(atPath: ready.path(percentEncoded: false)) {
            guard Date() < deadline else {
                try? Data().write(to: folder.appending(path: "done"))
                throw NSError(domain: "HelperDiskImage", code: 2, userInfo: [NSLocalizedDescriptionKey: "The disk-image helper didn't attach \(id) in 180 s"])
            }
            try await Task.sleep(for: .milliseconds(50))
        }
        // "ok /dev/diskN", or "failed" and hdiutil's output.
        let answer = try String(contentsOf: ready, encoding: .utf8).split(separator: " ", maxSplits: 1)
        guard answer.first == "ok", answer.count == 2 else {
            try? Data().write(to: folder.appending(path: "done"))
            throw NSError(domain: "HelperDiskImage", code: 3, userInfo: [NSLocalizedDescriptionKey: "The disk-image helper couldn't attach \(id): \(answer.joined(separator: " "))"])
        }
        return HelperDiskImage(folder: folder, device: answer[1].trimmingCharacters(in: .whitespacesAndNewlines))
    }

    /// Whether a helper has looked for requests in the last few seconds.
    private static func helperIsRunning() -> Bool {
        // The helper may be waiting for this folder to exist.
        try? FileManager.default.createDirectory(at: work, withIntermediateDirectories: true)
        let deadline = Date().addingTimeInterval(10)
        repeat {
            let beats = (try? FileManager.default.contentsOfDirectory(at: work, includingPropertiesForKeys: [.contentModificationDateKey])) ?? []
            let recent = beats.contains { url in
                guard url.lastPathComponent.hasPrefix(".helper-"),
                      let date = try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate
                else { return false }
                return Date().timeIntervalSince(date) < 5
            }
            if recent { return true }
            Thread.sleep(forTimeInterval: 0.2)
        } while Date() < deadline
        return false
    }

    /// Pulls the drive out while Leal has the file open, as when a drive is
    /// unplugged: the helper runs `hdiutil detach -force` on it. The test
    /// host could do it itself, but in the sandbox that takes about 10 s
    /// while a file on the volume is open (0.3 s outside), by which time
    /// the copy has finished. Throws with the helper's report if the image
    /// is still attached afterwards.
    func pull() async throws {
        try Data().write(to: folder.appending(path: "detach"))
        let detached = folder.appending(path: "detached")
        let deadline = Date().addingTimeInterval(60)
        while !FileManager.default.fileExists(atPath: detached.path(percentEncoded: false)) {
            guard Date() < deadline else { break }
            try await Task.sleep(for: .milliseconds(5))
        }
        if isAttached() {
            let report = (try? String(contentsOf: detached, encoding: .utf8)) ?? "no answer in 60 s"
            throw NSError(domain: "HelperDiskImage", code: 5, userInfo: [NSLocalizedDescriptionKey: "The helper couldn't detach \(image.path): \(report)"])
        }
    }

    /// Detaches the image from inside the sandbox, which `hdiutil` allows
    /// (`hdiutil detach -force`, by device), once no file on it is open.
    /// Throws with `hdiutil`'s output if the image is still attached
    /// afterwards.
    func forceDetach() throws {
        var backoff = Self.firstBackoff
        var last = ""
        for _ in 1...Self.attempts {
            guard isAttached() else { return }
            let (status, output) = Self.hdiutil(["detach", device, "-force"])
            last = "exit \(status): \(output)"
            if !isAttached() { return }
            // Still attached (hdiutil busy, or the volume still in use):
            // try again after a pause, as the Rust tests' `force_detach`.
            Thread.sleep(forTimeInterval: backoff)
            backoff *= 2
        }
        if isAttached() {
            throw NSError(domain: "HelperDiskImage", code: 4, userInfo: [NSLocalizedDescriptionKey: "Couldn't detach \(image.path): \(last)"])
        }
    }

    /// Whether the image is attached, from `hdiutil info`.
    func isAttached() -> Bool {
        let (status, output) = Self.hdiutil(["info", "-plist"])
        guard status == 0,
              let plist = try? PropertyListSerialization.propertyList(from: Data(output.utf8), format: nil),
              let entries = (plist as? [String: Any])?["images"] as? [[String: Any]]
        else { return false }
        let path = image.path(percentEncoded: false)
        let resolved = image.resolvingSymlinksInPath().path(percentEncoded: false)
        return entries.contains { entry in
            let attached = entry["image-path"] as? String
            return attached == path || attached == resolved
        }
    }

    /// Tells the helper the test is done with the image: it detaches
    /// whatever is still attached and removes the folder.
    func finish() {
        if isAttached() { try? forceDetach() }
        try? Data().write(to: folder.appending(path: "done"))
    }

    private static func hdiutil(_ arguments: [String]) -> (Int32, String) {
        let process = Process()
        process.executableURL = URL(filePath: "/usr/bin/hdiutil")
        process.arguments = arguments
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        do {
            try process.run()
        } catch {
            return (-1, "couldn't run hdiutil: \(error)")
        }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return (process.terminationStatus, String(decoding: data, as: UTF8.self))
    }
}
