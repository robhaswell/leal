import XCTest

/// Calls the Rust core through the generated Swift bindings. The bindings
/// (`Generated/leal_ffi.swift`) are compiled into this test bundle and it
/// links `libleal_ffi.a`, so these tests run without launching the app.
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

    /// A Rust `Err` arrives in Swift as a thrown `LealError`.
    func testMissingFileThrowsNotFound() {
        let path = FileManager.default.temporaryDirectory
            .appending(path: "leal-\(UUID().uuidString)/missing.csv")
            .path(percentEncoded: false)
        XCTAssertThrowsError(try inspectFile(path: path)) { error in
            XCTAssertEqual(error as? LealError, LealError.NotFound(path: path))
        }
    }

    func testDirectoryThrowsIoError() {
        let path = FileManager.default.temporaryDirectory.path(percentEncoded: false)
        XCTAssertThrowsError(try inspectFile(path: path)) { error in
            guard case LealError.Io(let errorPath, let message)? = error as? LealError else {
                return XCTFail("expected LealError.Io, got \(error)")
            }
            XCTAssertEqual(errorPath, path)
            XCTAssertFalse(message.isEmpty)
        }
    }

    /// Writes `contents` to a new file in a temporary directory that is
    /// deleted after the test.
    private func temporaryFile(named name: String, contents: String) throws -> URL {
        let directory = FileManager.default.temporaryDirectory.appending(path: "leal-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: directory)
        }
        let url = directory.appending(path: name)
        try Data(contents.utf8).write(to: url)
        return url
    }
}
