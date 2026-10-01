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

    func testInspectFileReadsSizeAndFirstLine() throws {
        let url = try temporaryFile(named: "people.csv", contents: "name,city\r\nAda,London\r\n")
        let summary = try inspectFile(path: url.path(percentEncoded: false))
        XCTAssertEqual(summary, FileSummary(byteCount: 23, firstLine: "name,city"))
    }

    func testNonASCIIPathAndContentsCrossTheBoundary() throws {
        let url = try temporaryFile(named: "café – résumé.csv", contents: "prénom,ville\nZoë,Zürich\n")
        let summary = try inspectFile(path: url.path(percentEncoded: false))
        XCTAssertEqual(summary.firstLine, "prénom,ville")
        XCTAssertEqual(summary.byteCount, UInt64("prénom,ville\nZoë,Zürich\n".utf8.count))
    }

    /// A Rust `Err` arrives in Swift as a thrown `LealError`, with the path
    /// and the OS error code.
    func testMissingFileThrowsNotFound() throws {
        let path = try temporaryDirectory().appending(path: "missing.csv").path(percentEncoded: false)
        XCTAssertThrowsError(try inspectFile(path: path)) { error in
            XCTAssertEqual(error as? LealError, LealError.NotFound(path: path, code: ENOENT))
        }
        XCTAssertThrowsError(try openSource(path: path, volumeFolder: nil, temp: temporaryLocations())) { error in
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
        XCTAssertThrowsError(try inspectFile(path: path)) { error in
            XCTAssertEqual(error as? LealError, expected)
        }
        let volumeFolder = try XCTUnwrap(TemporaryFolders.volumeFolder(for: url))
        XCTAssertThrowsError(try openSource(path: path, volumeFolder: volumeFolder, temp: temporaryLocations())) {
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
        XCTAssertThrowsError(try inspectFile(path: path)) { error in
            XCTAssertEqual(error as? LealError, expected)
        }
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
    func testOpenSourceClonesIntoTheFoldersFileManagerGives() throws {
        let url = try temporaryFile(named: "people.csv", contents: "name,city\nAda,London\n")
        let volumeFolder = try XCTUnwrap(TemporaryFolders.volumeFolder(for: url))
        let temp = try temporaryLocations()
        var source: Source? = try openSource(
            path: url.path(percentEncoded: false),
            volumeFolder: volumeFolder,
            temp: temp
        )
        XCTAssertEqual(source?.byteCount(), 21)
        XCTAssertEqual(source?.storage(), .clone)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: volumeFolder).count, 1)
        source = nil
        XCTAssertFalse(FileManager.default.fileExists(atPath: volumeFolder))
        XCTAssertEqual(try removeLeftoverTempFolders(temp: temp), 0)
    }

    /// ADR-0005 decision 7, end to end: a file on a second APFS volume (a
    /// disk image) is cloned into the folder `FileManager` makes on that
    /// volume, not read into memory or copied.
    func testFileOnASecondAPFSVolumeIsCloned() throws {
        let image = try DiskImage(temporaryDirectory: temporaryDirectory())
        addTeardownBlock { image.detach() }
        let url = image.root.appending(path: "on-image.csv")
        try Data("a,b\n1,2\n".utf8).write(to: url)

        let volumeFolder = try XCTUnwrap(TemporaryFolders.volumeFolder(for: url))
        let device = { (path: String) in
            try FileManager.default.attributesOfItem(atPath: path)[.systemNumber] as? Int
        }
        XCTAssertNotEqual(try device(url.path(percentEncoded: false)), try device(NSTemporaryDirectory()))
        XCTAssertEqual(
            try device(volumeFolder),
            try device(url.path(percentEncoded: false)),
            "FileManager's folder should be on the image, got \(volumeFolder)"
        )
        var source: Source? = try openSource(
            path: url.path(percentEncoded: false),
            volumeFolder: volumeFolder,
            temp: temporaryLocations()
        )
        XCTAssertEqual(source?.storage(), .clone)
        XCTAssertEqual(source?.byteCount(), 8)
        source = nil
        XCTAssertFalse(FileManager.default.fileExists(atPath: volumeFolder))
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
        }

        let url = try temporaryFile(named: "after-panic.csv", contents: "a,b\n")
        XCTAssertEqual(try inspectFile(path: url.path(percentEncoded: false)).firstLine, "a,b")
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
/// `/Volumes`) and hidden from Finder.
private struct DiskImage {
    let root: URL

    init(temporaryDirectory: URL) throws {
        let image = temporaryDirectory.appending(path: "volume.dmg")
        root = temporaryDirectory.appending(path: "mnt")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try Self.hdiutil(["create", "-quiet", "-size", "16m", "-fs", "APFS", "-volname", "LealTest", image.path])
        try Self.hdiutil(["attach", "-quiet", "-nobrowse", "-noverify", "-noautoopen", "-mountpoint", root.path, image.path])
    }

    func detach() {
        if (try? Self.hdiutil(["detach", "-quiet", root.path])) == nil {
            try? Self.hdiutil(["detach", "-quiet", "-force", root.path])
        }
    }

    private static func hdiutil(_ arguments: [String]) throws {
        let process = Process()
        process.executableURL = URL(filePath: "/usr/bin/hdiutil")
        process.arguments = arguments
        try process.run()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            throw NSError(
                domain: "DiskImage",
                code: Int(process.terminationStatus),
                userInfo: [NSLocalizedDescriptionKey: "hdiutil \(arguments.joined(separator: " ")) failed"]
            )
        }
    }
}
