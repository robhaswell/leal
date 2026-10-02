import LealFFI
import XCTest

/// Calls the Rust core through the generated Swift bindings. The bindings
/// (`Generated/leal_ffi.swift`) and `libleal_ffi.a` live in the LealFFI
/// framework, which this test bundle links, so these tests run without
/// launching the app. `OpenErrorText.swift` and `TemporaryFolders.swift` are
/// compiled into this bundle from `Sources/` too.
final class LealFFITests: XCTestCase {
    func testCoreVersionComesFromRust() {
        let version = coreVersion()
        XCTAssertFalse(version.isEmpty)
        XCTAssertEqual(version.split(separator: ".").count, 3, "expected a semver version, got \(version)")
    }

    func testNonASCIIPathAndContentsCrossTheBoundary() throws {
        let contents = "prénom,ville\nZoë,Zürich\n"
        let url = try temporaryFile(named: "café – résumé.csv", contents: contents)
        let source = try TemporaryFolders.open(url, temp: temporaryLocations())
        XCTAssertEqual(source.byteCount(), UInt64(contents.utf8.count))
        let document = try openDocument(
            path: url.path(percentEncoded: false),
            volume: VolumeInfo(),
            temp: temporaryLocations(),
            scheduler: Scheduler(),
            options: OpenOptions(),
            observer: nil
        )
        XCTAssertEqual(try document.firstScreen().rows.map { $0.map(\.text) }, [["prénom", "ville"], ["Zoë", "Zürich"]])
    }

    /// A Rust `Err` arrives in Swift as a thrown `LealError`, with the path
    /// and the OS error code.
    func testMissingFileThrowsNotFound() throws {
        let path = try temporaryDirectory().appending(path: "missing.csv").path(percentEncoded: false)
        XCTAssertThrowsError(try openSource(path: path, volume: VolumeInfo(), temp: temporaryLocations())) { error in
            XCTAssertEqual(error as? LealError, LealError.NotFound(path: path, code: ENOENT))
            XCTAssertEqual(OpenErrorText.describe(error), "The file doesn’t exist.")
        }
    }

    /// PLAN 1.1: a folder is told apart from other errors, and worded.
    func testDirectoryThrowsNotAFile() throws {
        let url = try temporaryDirectory().appending(path: "folder.csv")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        let path = url.path(percentEncoded: false)
        let expected = LealError.NotAFile(path: path, isDirectory: true)
        let volumeFolder = try XCTUnwrap(TemporaryFolders.volumeFolder(for: url))
        XCTAssertThrowsError(try openSource(path: path, volume: VolumeInfo(folder: volumeFolder), temp: temporaryLocations())) {
            error in
            XCTAssertEqual(error as? LealError, expected)
            XCTAssertEqual(OpenErrorText.describe(error), "It’s a folder, not a file.")
        }
        XCTAssertFalse(FileManager.default.fileExists(atPath: volumeFolder), "the core removes the unused folder")
    }

    /// PLAN 1.1: permission denied is told apart from other errors, and
    /// worded, instead of showing Rust's "Permission denied (os error 13)".
    func testUnreadableFileThrowsPermissionDenied() throws {
        let url = try temporaryFile(named: "locked.csv", contents: "a,b\n")
        try FileManager.default.setAttributes([.posixPermissions: 0o000], ofItemAtPath: url.path(percentEncoded: false))
        let path = url.path(percentEncoded: false)
        let expected = LealError.PermissionDenied(path: path, code: EACCES)
        XCTAssertThrowsError(try TemporaryFolders.open(url, temp: temporaryLocations())) { error in
            XCTAssertEqual(error as? LealError, expected)
            XCTAssertEqual(OpenErrorText.describe(error), "You don’t have permission to open it.")
        }
    }

    /// Errors the app doesn't word specially use the system's description of
    /// the error code. The English message from Rust is never shown.
    func testOtherErrorsAreWordedFromTheirCode() {
        let full = LealError.Io(path: "/x.csv", code: ENOSPC, message: "couldn't copy /x.csv: (os error 28)")
        XCTAssertEqual(OpenErrorText.describe(full), "No space left on device (error 28).")
        let noCode = LealError.Io(path: "/x.csv", code: nil, message: "something")
        XCTAssertEqual(OpenErrorText.describe(noCode), "An unexpected error occurred.")
        XCTAssertEqual(OpenErrorText.title(fileName: "data.csv"), "Leal couldn’t open “data.csv”.")
    }

    /// The production path: `FileManager` makes a folder on the file's
    /// volume, the core clones into it, and releasing the source deletes it.
    /// Foundation says the boot volume is internal and not ejectable, so the
    /// clone is mapped.
    func testOpenSourceClonesIntoTheFoldersFileManagerGives() throws {
        let url = try temporaryFile(named: "people.csv", contents: "name,city\nAda,London\n")
        let volume = TemporaryFolders.volume(for: url)
        let volumeFolder = try XCTUnwrap(volume.folder)
        XCTAssertEqual(volume.isInternal, true)
        XCTAssertEqual(volume.isEjectable, false)
        let temp = try temporaryLocations()
        var source: Source? = try openSource(path: url.path(percentEncoded: false), volume: volume, temp: temp)
        XCTAssertEqual(source?.byteCount(), 21)
        XCTAssertEqual(source?.storage(), .clone)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: volumeFolder).count, 1)
        source = nil
        XCTAssertFalse(FileManager.default.fileExists(atPath: volumeFolder))
        XCTAssertEqual(try removeLeftoverTempFolders(temp: temp), 0)
    }

    /// ADR-0005 decision 7 and ADR-0006, end to end: a file on a second APFS
    /// volume (a disk image) is cloned into the folder `FileManager` makes on
    /// that volume, not read into memory. Foundation says a disk image is
    /// ejectable, so the core treats it as a removable drive: the clone is
    /// read, not mapped, until it has been copied to the internal disk.
    func testFileOnADiskImageIsClonedThereAndReadNotMapped() throws {
        let image = try DiskImage(temporaryDirectory: temporaryDirectory())
        addTeardownBlock { image.detach() }
        let url = image.root.appending(path: "on-image.csv")
        try Data("a,b\n1,2\n".utf8).write(to: url)

        let volume = TemporaryFolders.volume(for: url)
        let volumeFolder = try XCTUnwrap(volume.folder)
        XCTAssertNotEqual(volume.isInternal, true)
        XCTAssertEqual(volume.isEjectable, true)
        let device = { (path: String) in
            try FileManager.default.attributesOfItem(atPath: path)[.systemNumber] as? Int
        }
        XCTAssertNotEqual(try device(url.path(percentEncoded: false)), try device(NSTemporaryDirectory()))
        XCTAssertEqual(
            try device(volumeFolder),
            try device(url.path(percentEncoded: false)),
            "FileManager's folder should be on the image, got \(volumeFolder)"
        )
        let temp = try temporaryLocations()
        var source: Source? = try openSource(path: url.path(percentEncoded: false), volume: volume, temp: temp)
        XCTAssertEqual(source?.storage(), .reading)
        XCTAssertEqual(source?.byteCount(), 8)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: volumeFolder).count, 1)
        source = nil
        XCTAssertFalse(FileManager.default.fileExists(atPath: volumeFolder))
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: temp.scratchDir), [])
        XCTAssertEqual(try removeLeftoverTempFolders(temp: temp), 0)
    }

    /// A Rust panic in an export that returns `Result` arrives in Swift as a
    /// thrown error instead of crashing the app, and the core keeps working
    /// afterwards. The app relies on this to survive a bug in the core with
    /// unsaved edits. `debugPanic` is a test-only export (leal-ffi's
    /// `test-exports` feature). `just app-test release` runs this against the
    /// Rust release profile, so `panic = "abort"` there would fail it.
    func testRustPanicThrowsInsteadOfCrashing() throws {
        XCTAssertThrowsError(try debugPanic(message: "deliberate test panic")) { error in
            // UniFFI's `rustPanic` error is fileprivate to the bindings, so
            // the app sees a panic as "an error that isn't a LealError".
            XCTAssertNil(error as? LealError, "a panic should not arrive as a LealError: \(error)")
            XCTAssertTrue(
                error.localizedDescription.contains("deliberate test panic"),
                "expected the panic message, got \(error.localizedDescription)"
            )
            // The message is for logs: the words shown come from the catalog
            // (phase 1 review, app-1).
            XCTAssertEqual(
                OpenErrorText.describe(error),
                "Something went wrong inside Leal. Close the file and open it again."
            )
        }

        let url = try temporaryFile(named: "after-panic.csv", contents: "a,b\n")
        XCTAssertEqual(try TemporaryFolders.open(url, temp: temporaryLocations()).byteCount(), 4)
    }

    /// A new directory that is deleted after the test.
    private func temporaryDirectory() throws -> URL {
        let directory = FileManager.default.temporaryDirectory.appending(path: "leal-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: directory)
        }
        return directory
    }

    /// Writes `contents` to a new file in a temporary directory that is
    /// deleted after the test.
    private func temporaryFile(named name: String, contents: String) throws -> URL {
        let url = try temporaryDirectory().appending(path: name)
        try Data(contents.utf8).write(to: url)
        return url
    }

    /// Scratch and records folders of the test's own, so tests never touch
    /// the app's real Application Support folder.
    private func temporaryLocations() throws -> TempLocations {
        let directory = try temporaryDirectory()
        return TempLocations(
            scratchDir: directory.appending(path: "scratch").path(percentEncoded: false),
            recordsDir: directory.appending(path: "records").path(percentEncoded: false)
        )
    }
}

/// A small APFS disk image, attached inside a temporary directory (not in
/// `/Volumes`) and hidden from Finder. The same approach as `DiskImage` in
/// crates/leal-core/src/source/tests/mod.rs: a unique path and volume name,
/// retries on `hdiutil`'s transient errors, and a failure that reports
/// `hdiutil`'s exit status, stdout and stderr.
private struct DiskImage {
    let image: URL
    let root: URL

    /// How many times `hdiutil create` and `attach` are tried, and the wait
    /// before the first retry (doubled for each one after).
    private static let attempts = 5
    private static let firstBackoff: TimeInterval = 0.25

    /// Errors `hdiutil` reports when another image is being created,
    /// attached or detached at the same moment. Trying again shortly
    /// afterwards usually works.
    private static let transientErrors = [
        "Resource busy",
        "Resource temporarily unavailable",
        "no mountable file systems",
        "Device not configured",
    ]

    /// Creates and attaches the image. On failure, anything attached is
    /// detached before this throws; otherwise the caller detaches it with
    /// `detach()`.
    init(temporaryDirectory: URL) throws {
        image = temporaryDirectory.appending(path: "volume.dmg")
        root = temporaryDirectory.appending(path: "mnt")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        // `temporaryDirectory` is unique to this test; so is the volume name.
        let volumeName = "LealTest-\(UUID().uuidString.prefix(8))"
        let image = self.image
        let root = self.root
        try Self.withRetries {
            // A failed attempt may leave a partial image behind.
            try? FileManager.default.removeItem(at: image)
            return ["create", "-size", "16m", "-fs", "APFS", "-volname", volumeName, image.path]
        }
        do {
            try Self.withRetries {
                // A failed attempt may leave the image attached but not mounted.
                Self.detachAll(imagePath: image.path)
                return ["attach", "-nobrowse", "-noverify", "-noautoopen", "-mountpoint", root.path, image.path]
            }
        } catch {
            Self.detachAll(imagePath: image.path)
            throw error
        }
    }

    /// Detaches every device attached from the image, trying a normal detach
    /// first and then `-force`, even if the test failed.
    func detach() {
        let left = Self.detachAll(imagePath: image.path)
        if !left.isEmpty {
            print("warning: couldn't detach \(image.path) (\(left.joined(separator: ", "))); run `hdiutil detach -force` on it")
        }
    }

    /// Detaches the devices attached from `imagePath`. Returns the ones
    /// still attached.
    @discardableResult
    private static func detachAll(imagePath: String) -> [String] {
        for force in [false, false, true, true] {
            let devices = attachedDevices(imagePath: imagePath)
            if devices.isEmpty {
                return devices
            }
            for device in devices {
                // A failure here shows up as a device still attached.
                _ = try? run(["detach", device] + (force ? ["-force"] : []))
            }
            Thread.sleep(forTimeInterval: firstBackoff)
        }
        return attachedDevices(imagePath: imagePath)
    }

    /// The whole-disk devices (`/dev/diskN`) attached from `imagePath`, from
    /// `hdiutil info -plist`. Empty if none are, or if that fails.
    private static func attachedDevices(imagePath: String) -> [String] {
        guard
            let result = try? run(["info", "-plist"]), result.status == 0,
            let plist = try? PropertyListSerialization.propertyList(from: result.stdout, format: nil),
            let images = (plist as? [String: Any])?["images"] as? [[String: Any]]
        else {
            return []
        }
        let resolved = URL(filePath: imagePath).resolvingSymlinksInPath().path
        return images.compactMap { entry in
            guard let path = entry["image-path"] as? String,
                  path == imagePath || path == resolved,
                  let entities = entry["system-entities"] as? [[String: Any]]
            else {
                return nil
            }
            // The whole disk is the shortest device name (`/dev/disk7`, not
            // `/dev/disk7s1`); detaching it detaches the rest.
            return entities.compactMap { $0["dev-entry"] as? String }.min { $0.count < $1.count }
        }
    }

    /// Runs `hdiutil` with the arguments `makeArguments` returns (called
    /// again before each attempt), retrying with a backoff while it fails
    /// with one of `transientErrors`.
    private static func withRetries(_ makeArguments: () -> [String]) throws {
        var backoff = firstBackoff
        for attempt in 1...attempts {
            let arguments = makeArguments()
            let result = try run(arguments)
            if result.status == 0 {
                return
            }
            let output = result.outputText
            let transient = transientErrors.contains { output.contains($0) }
            if !transient || attempt == attempts {
                throw NSError(
                    domain: "DiskImage",
                    code: Int(result.status),
                    userInfo: [
                        NSLocalizedDescriptionKey: """
                        hdiutil \(arguments.joined(separator: " ")) failed on attempt \(attempt) of \(attempts) \
                        (\(transient ? "transient error" : "not a transient error, so not retried")), \
                        exit status \(result.status)
                        \(result.report)
                        """,
                    ]
                )
            }
            print("hdiutil \(arguments[0]) failed on attempt \(attempt) of \(attempts), retrying in \(backoff)s\n\(result.report)")
            Thread.sleep(forTimeInterval: backoff)
            backoff *= 2
        }
    }

    /// One `hdiutil` run's exit status and output.
    private struct Run {
        let status: Int32
        let stdout: Data
        let stderr: Data

        var outputText: String {
            String(decoding: stdout, as: UTF8.self) + String(decoding: stderr, as: UTF8.self)
        }

        /// stdout and stderr, for a failure message.
        var report: String {
            """
            --- stdout ---
            \(String(decoding: stdout, as: UTF8.self))
            --- stderr ---
            \(String(decoding: stderr, as: UTF8.self))
            """
        }
    }

    /// Runs `/usr/bin/hdiutil` and collects its exit status and output.
    private static func run(_ arguments: [String]) throws -> Run {
        let process = Process()
        process.executableURL = URL(filePath: "/usr/bin/hdiutil")
        process.arguments = arguments
        let stdout = Pipe()
        let stderr = Pipe()
        process.standardOutput = stdout
        process.standardError = stderr
        try process.run()
        // Read both pipes to the end before waiting, so a full pipe buffer
        // can't block `hdiutil`: stderr on another thread, stdout here.
        let errorData = ReadBuffer()
        let errorRead = DispatchSemaphore(value: 0)
        let errorHandle = stderr.fileHandleForReading
        DispatchQueue.global().async {
            errorData.data = errorHandle.readDataToEndOfFile()
            errorRead.signal()
        }
        let outputData = stdout.fileHandleForReading.readDataToEndOfFile()
        errorRead.wait()
        process.waitUntilExit()
        return Run(status: process.terminationStatus, stdout: outputData, stderr: errorData.data)
    }

    /// Data read on another thread. The semaphore in `run` orders the write
    /// before the read, so no lock is needed.
    private final class ReadBuffer: @unchecked Sendable {
        var data = Data()
    }
}
