import AppKit
import CryptoKit
import LealFFI
import XCTest

@testable import Leal

/// The removable-drive path (ADR-0006) in the sandboxed app, on real disk
/// images (phase 1 review, cons-12): a file on an ejectable volume is read
/// from the drive while it is copied to this Mac, then worked from the copy;
/// and a drive detached part-way through shows the disconnected banner
/// without a crash (the copy held part-way by the core's test hook until the
/// drive has gone, so the test doesn't race it).
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
        DocumentModel.openForTesting = nil
        // Closing a document whose copy is still held cancels the copy.
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

    /// Save on an APFS and an exFAT drive (task 2.5.3a): the new file is
    /// written in an item-replacement folder on the drive, then swapped in
    /// (APFS) or renamed over the file (exFAT, which can't swap); the bytes
    /// are right, the document is clean, and nothing of Leal's is left on
    /// the drive.
    func testSaveOnARemovableDriveWritesTheFileAndLeavesNothing() async throws {
        let contents = Data("id,name\n1,Marlow\n2,Ostrava\n".utf8)
        for fs in ["APFS", "ExFAT"] {
            let image = try await attach(fs: fs, format: "UDRW", size: "64m", file: "drive.csv", contents: contents)
            let url = image.root.appending(path: "drive.csv")
            let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
            document.makeWindowControllers()
            var alerts: [String] = []
            document.showSheet = { alert, _, done in
                alerts.append(alert.messageText)
                done(NSApplication.ModalResponse(rawValue: NSApplication.ModalResponse.alertFirstButtonReturn.rawValue + alert.buttons.count - 1))
            }
            let model = try XCTUnwrap(document.model)
            try await waitUntil("\(fs): copied and indexed") { model.isIndexComplete && model.storage == .copy }
            guard case .edited = model.setCell(.cell(CellPosition(row: 1, column: 1)), to: "Tromsø") else {
                return XCTFail("\(fs): not edited")
            }
            document.save(nil)
            let saving = try XCTUnwrap(document.saving)
            let saved = await saving.value

            XCTAssertTrue(saved, "\(fs): \(alerts)")
            XCTAssertEqual(alerts, [])
            XCTAssertEqual(try Data(contentsOf: url), Data("id,name\n1,Marlow\n2,Tromsø\n".utf8), fs)
            XCTAssertFalse(document.isDocumentEdited, fs)
            document.close()
            CoreRelease.finish()
            try await waitUntil("\(fs): the copies are removed") { records().isEmpty }
            XCTAssertEqual(Self.leftovers(on: image.root), [], "\(fs): nothing of Leal's is left on the drive")
            let hidden = (try? FileManager.default.contentsOfDirectory(atPath: image.root.path(percentEncoded: false))) ?? []
            XCTAssertEqual(hidden.filter { $0.hasPrefix(".leal-save") }, [], "\(fs): the item-replacement folder was used")
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

    /// A drive pulled out while Leal is still copying the file: the document
    /// shows the rows it had read, the disconnected banner and status note,
    /// Save is off, and nothing crashes, including closing it.
    ///
    /// The file is opened the real way (its volume looked at, read from the
    /// drive, copied to this Mac), except that the core's test hook holds
    /// the copy at `holdAt` (`debugOpenDocumentHoldingCopy`). The drive is
    /// pulled while it is held, and only once it has gone does the copy go
    /// on, and find it gone. So the copy is certainly part-way through when
    /// the drive goes, however long `hdiutil detach -force` takes: the test
    /// used to count on the pull beating a slow (compressed) image's copy,
    /// and failed when a detach took 1.7 s.
    func testADriveDetachedMidCopyShowsDisconnected() async throws {
        let rows = 400_000
        let image = try await attach(fs: "ExFAT", format: "UDRW", size: "64m", file: "drive.csv", contents: csv(rows: rows))
        let url = image.root.appending(path: "drive.csv")
        // 4 MiB of about 18 MB, past first paint's 64 KB: four of the copy's
        // 1 MiB chunks, then it waits.
        let holdAt: UInt64 = 4 << 20
        DocumentModel.openForTesting = { path, environment, options, observer in
            try debugOpenDocumentHoldingCopy(
                path: path,
                volume: TemporaryFolders.volume(for: URL(filePath: path)),
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer,
                holdAt: holdAt
            )
        }
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let model = try XCTUnwrap(document.model)
        let content = try XCTUnwrap((document.windowControllers.first as? DocumentWindowController)?.content)
        _ = content.view
        XCTAssertEqual(model.storage, .reading, "streamed from the drive at first paint")
        XCTAssertEqual(model.cell(row: 0, column: 0), .text("0", truncated: false))
        // Copied and indexed up to the hold (about 93,000 rows), and no
        // further: the copy is waiting.
        try await waitUntil("copied to the hold") { model.loadedRowCount > 50_000 }
        XCTAssertEqual(model.storage, .reading)
        XCTAssertFalse(model.isIndexComplete)

        // Pulled out (the helper's `hdiutil detach -force`), then the copy
        // goes on.
        let pulled = Date()
        try image.requestPull()
        try await image.waitUntilPulled()
        let took = Date().timeIntervalSince(pulled)
        model.backgroundHandle()?.debugReleaseHeldCopy()
        try await waitUntil("disconnected (detaching took \(took) s)") { model.storage == .disconnected || model.storage == .copy }
        XCTAssertEqual(model.storage, .disconnected, "the copy finished, though the drive went first (detaching took \(took) s)")
        XCTAssertFalse(model.isFailed)
        XCTAssertFalse(document.canSave)
        try await waitUntil("its banner") { content.driveBanner != nil }
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
    /// This checkout's folder: the helper of another worktree never sees
    /// it. Keyed, as the helper keys it, on the first 12 hex digits of the
    /// SHA-256 of the scheme's `$(PROJECT_DIR)`.
    static var work: URL? {
        guard let checkout = ProcessInfo.processInfo.environment["LEAL_DISK_IMAGE_CHECKOUT"], !checkout.isEmpty else { return nil }
        let key = SHA256.hash(data: Data(checkout.utf8)).map { String(format: "%02x", $0) }.joined().prefix(12)
        return FileManager.default.temporaryDirectory.appending(path: "leal-disk-images").appending(path: String(key))
    }
    /// How long `hdiutil detach -force` is tried for, and the wait before
    /// the first retry (doubled for each one after, up to a second).
    private static let detachForcedFor: TimeInterval = 6
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
        guard let work else {
            throw NSError(domain: "HelperDiskImage", code: 6, userInfo: [
                NSLocalizedDescriptionKey: "LEAL_DISK_IMAGE_CHECKOUT isn't set: run the tests with the Leal scheme, whose test action sets it.",
            ])
        }
        guard helperIsRunning(in: work) else {
            throw NSError(domain: "HelperDiskImage", code: 1, userInfo: [
                NSLocalizedDescriptionKey: """
                No disk-image helper is running. The Leal scheme's test pre-action starts it \
                (app/Scripts/disk-image-helper.sh; its log is build/disk-image-helper/helper.log): the sandboxed \
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
    private static func helperIsRunning(in work: URL) -> Bool {
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

    /// Asks the helper to pull the drive out, as when a drive is unplugged
    /// with a file on it open: it runs `hdiutil detach -force` on it. The test
    /// host could do it itself, but in the sandbox that takes about 10 s
    /// while a file on the volume is open (0.3 s outside). How long it
    /// takes varies, so a test that needs the copy part-way through when
    /// the drive goes holds the copy (`debugOpenDocumentHoldingCopy`)
    /// rather than racing it. `waitUntilPulled` throws with the helper's
    /// report if the image is still attached afterwards.
    func requestPull() throws {
        try Data().write(to: folder.appending(path: "detach"))
    }

    /// Waits until the helper has pulled the drive (`requestPull`).
    func waitUntilPulled() async throws {
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
    ///
    /// A device counts as detached once its `/dev` node has gone, even if
    /// `hdiutil info` still lists it: `hdiutil` lags behind the kernel for
    /// a moment after a detach (the helper's, or a drive's own), and then
    /// says "No such file or directory" to a detach of it.
    func forceDetach() throws {
        let started = Date()
        var backoff = Self.firstBackoff
        var trouble: [String] = []
        while true {
            let devices = presentDevices()
            if devices.isEmpty {
                if !trouble.isEmpty {
                    print("HelperDiskImage: detached \(image.path) after:\n  \(trouble.joined(separator: "\n  "))")
                }
                return
            }
            guard Date().timeIntervalSince(started) < Self.detachForcedFor else { break }
            // One device per attachment: detaching it detaches the rest.
            for device in devices {
                let (status, output) = Self.hdiutil(["detach", device, "-force"])
                if status != 0 {
                    let gone = output.contains("No such file or directory") && !Self.inDev(device)
                    let said = output.split(separator: "\n").filter { !$0.contains("is deprecated") }.joined(separator: " / ")
                    trouble.append("at \(String(format: "%.1f s", Date().timeIntervalSince(started))), hdiutil detach \(device) -force failed\(gone ? " (the device had gone already)" : ""), exit \(status): \(said)")
                }
            }
            if presentDevices().isEmpty { continue }
            // Still attached (hdiutil busy, or the volume still in use):
            // try again after a pause, as the Rust tests' `force_detach`.
            Thread.sleep(forTimeInterval: backoff)
            backoff = min(backoff * 2, 1)
        }
        let left = presentDevices()
        if !left.isEmpty {
            let report = "Couldn't detach \(image.path) (\(left.joined(separator: ", "))):\n  \(trouble.joined(separator: "\n  "))"
            print("HelperDiskImage: \(report)")
            throw NSError(domain: "HelperDiskImage", code: 4, userInfo: [NSLocalizedDescriptionKey: report])
        }
    }

    /// Whether the image is attached: listed in `hdiutil info` with a
    /// device still in /dev.
    func isAttached() -> Bool {
        !presentDevices().isEmpty
    }

    /// For each attachment of the image in `hdiutil info`, the first of
    /// its whole-disk devices (`/dev/diskN`: the disk, then any APFS
    /// container on it) still in /dev. Empty if `hdiutil info` fails three
    /// times.
    private func presentDevices() -> [String] {
        for _ in 1...3 {
            let (status, output) = Self.hdiutil(["info", "-plist"])
            guard status == 0,
                  let plist = try? PropertyListSerialization.propertyList(from: Data(output.utf8), format: nil),
                  let entries = (plist as? [String: Any])?["images"] as? [[String: Any]]
            else {
                print("HelperDiskImage: hdiutil info failed (exit \(status)): \(output)")
                Thread.sleep(forTimeInterval: Self.firstBackoff)
                continue
            }
            let path = image.path(percentEncoded: false)
            let resolved = image.resolvingSymlinksInPath().path(percentEncoded: false)
            return entries.compactMap { entry in
                let attached = entry["image-path"] as? String
                guard attached == path || attached == resolved,
                      let entities = entry["system-entities"] as? [[String: Any]]
                else { return nil }
                return entities
                    .compactMap { $0["dev-entry"] as? String }
                    .first { Self.isWholeDisk($0) && Self.inDev($0) }
            }
        }
        return []
    }

    /// Whether `device` is a whole disk (`/dev/disk7`, not `/dev/disk7s1`).
    private static func isWholeDisk(_ device: String) -> Bool {
        let number = device.dropFirst("/dev/disk".count)
        return device.hasPrefix("/dev/disk") && !number.isEmpty && number.allSatisfy(\.isASCII) && number.allSatisfy(\.isNumber)
    }

    /// Whether the device node is still in /dev. Only "not found" counts as
    /// gone: any other error (the sandbox not letting the host look, say)
    /// doesn't.
    private static func inDev(_ device: String) -> Bool {
        var info = stat()
        return stat(device, &info) == 0 || errno != ENOENT
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
