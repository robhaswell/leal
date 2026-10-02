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

    /// What Foundation knows about the volume `url` is on: a new folder there
    /// for the clone (`volumeFolder(for:)`), and whether the volume is
    /// internal and ejectable. The core treats a volume that isn't known to
    /// be internal, or is ejectable, as a removable drive: it reads the file
    /// without mapping it until it has copied it to the internal disk
    /// (ADR-0006). Either value is `nil` if Foundation doesn't know it; a
    /// disk image, for example, has no "is internal" value.
    ///
    /// A network volume gets no folder: asking `FileManager` for one would
    /// make a `.TemporaryItems` folder on the user's share, and a share is
    /// never cloned (task 2.0). Call it off the main thread: it asks the
    /// volume.
    static func volume(for url: URL) -> VolumeInfo {
        let values = try? url.resourceValues(forKeys: [.volumeIsInternalKey, .volumeIsEjectableKey, .volumeIsLocalKey])
        let onShare = values?.volumeIsLocal == false
        return VolumeInfo(
            folder: onShare ? nil : volumeFolder(for: url),
            isInternal: values?.volumeIsInternal,
            isEjectable: values?.volumeIsEjectable
        )
    }

    /// Opens `url` with the core: a clone on the file's own volume where it
    /// can, otherwise a fallback (`Source.storage()`).
    static func open(_ url: URL, temp: TempLocations) throws -> Source {
        try openSource(path: url.path(percentEncoded: false), volume: volume(for: url), temp: temp)
    }
}
