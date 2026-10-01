import Foundation
import LealFFI

/// Where the Rust core may put temporary files: the macOS side of
/// `leal_core::source` (DESIGN §3.1, §4.3).
///
/// Only Foundation knows where a (sandboxed) app may make a temporary folder
/// on another volume, so the app asks `FileManager` and passes the folders to
/// Rust, which does the rest.
///
/// This file is also compiled into the `LealTests` bundle (see
/// `project.yml`).
enum TemporaryFolders {
    /// Leal's temporary directory, and the folder in Application Support
    /// where the core records each temporary folder it makes. Both are
    /// places a sandboxed app may write.
    static func locations() throws -> TempLocations {
        let support = try FileManager.default.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        let bundleID = Bundle.main.bundleIdentifier ?? "io.github.robhaswell.leal"
        let records = support.appending(path: bundleID).appending(path: "Temporary folders")
        return TempLocations(
            scratchDir: FileManager.default.temporaryDirectory.path(percentEncoded: false),
            recordsDir: records.path(percentEncoded: false)
        )
    }

    /// A new, empty folder on the same volume as `url`, where the core can
    /// clone it (ADR-0005 decision 7), or `nil` if `FileManager` can't make
    /// one. The core takes the folder over and deletes it.
    static func volumeFolder(for url: URL) -> String? {
        let folder = try? FileManager.default.url(
            for: .itemReplacementDirectory,
            in: .userDomainMask,
            appropriateFor: url,
            create: true
        )
        return folder?.path(percentEncoded: false)
    }

    /// Opens `url` with the core: a clone on the file's own volume where it
    /// can, otherwise a fallback (`Source.storage()`).
    static func open(_ url: URL, temp: TempLocations) throws -> Source {
        try openSource(path: url.path(percentEncoded: false), volumeFolder: volumeFolder(for: url), temp: temp)
    }
}
