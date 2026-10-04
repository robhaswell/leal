import Foundation

/// Where Leal keeps a file it replaced but may not delete (task 2.5.3b):
/// the core's `SaveOutcome.keptOldFile`, an old file the save's swap took
/// out that couldn't be checked, so it may be another app's version
/// (task 2.2). The core first tries to keep it next to the user's file
/// (`a (replaced, kept by Leal).csv`), where it stays. Otherwise it is in
/// the save's temporary folder, which the system may empty, so the app
/// moves it at once to Leal's Recovered folder in Application Support
/// (inside the sandbox's container), and tells the user.
enum RecoveredFiles {
    /// The Recovered folder. Tests replace it.
    nonisolated(unsafe) static var folder: () throws -> URL = {
        let support = try FileManager.default.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        let bundleID = Bundle.main.bundleIdentifier ?? "io.github.robhaswell.leal"
        return support.appending(path: bundleID).appending(path: "Recovered")
    }

    /// Where a kept old file ended up (`place`).
    enum Kept: Sendable {
        /// Next to the user's file, where the core put it: left there.
        case besideFile(URL)
        /// Moved to the Recovered folder, here.
        case moved(URL)
        /// It couldn't be moved: it is still where the core left it.
        case notMoved(any Error)
    }

    /// Leaves the kept file at `path` where it is if it is next to `file`,
    /// the file saved; otherwise moves it into the Recovered folder
    /// (`keep(_:in:)`). Any thread: it looks at folders and may copy
    /// across volumes.
    static func place(_ path: String, savedTo file: URL?) -> Kept {
        let kept = URL(filePath: path)
        if let file, sameFolder(kept, file) {
            return .besideFile(kept)
        }
        do {
            return try .moved(keep(path, in: folder()))
        } catch {
            return .notMoved(error)
        }
    }

    /// Whether `a` and `b` are in the same folder: the same folder on disk
    /// (its file system identity), however each path spells it.
    static func sameFolder(_ a: URL, _ b: URL) -> Bool {
        let parents = [a, b].map { $0.deletingLastPathComponent() }
        let ids = parents.map { try? $0.resourceValues(forKeys: [.fileResourceIdentifierKey]).fileResourceIdentifier }
        if let first = ids[0], let second = ids[1] {
            return first.isEqual(second)
        }
        return parents[0].standardizedFileURL.resolvingSymlinksInPath() == parents[1].standardizedFileURL.resolvingSymlinksInPath()
    }

    /// Moves the file at `path` into `folder`, under its own name, or with
    /// " 2", " 3"… before its extension if that is taken, and returns where
    /// it is now. The folder it was in is removed if that leaves it empty
    /// (the save's temporary folder). Any thread: it may copy across
    /// volumes.
    ///
    /// On the same volume it is renamed with `RENAME_EXCL`, so a name
    /// taken meanwhile (another move at the same time) is never written
    /// over: the next number is tried instead.
    ///
    /// - Throws: if the folder can't be made or the file can't be moved;
    ///   the file is then where it was.
    static func keep(_ path: String, in folder: URL) throws -> URL {
        let source = URL(filePath: path)
        let manager = FileManager.default
        try manager.createDirectory(at: folder, withIntermediateDirectories: true)
        let stem = source.deletingPathExtension().lastPathComponent
        let ext = source.pathExtension
        var number = 1
        while true {
            let name = number == 1 ? stem : "\(stem) \(number)"
            let destination = folder.appending(path: ext.isEmpty ? name : "\(name).\(ext)")
            do {
                try move(source, to: destination)
                _ = rmdir(source.deletingLastPathComponent().path(percentEncoded: false))
                return destination
            } catch let error as POSIXError where error.code == .EEXIST && number < 1000 {
                number += 1
            } catch CocoaError.fileWriteFileExists where number < 1000 {
                number += 1
            }
        }
    }

    /// Renames `source` to `destination` if nothing is there
    /// (`renamex_np` with `RENAME_EXCL`); across volumes, or where that
    /// isn't supported, copies it there (which fails if something is
    /// there) and removes it.
    ///
    /// - Throws: `POSIXError(.EEXIST)` or `CocoaError.fileWriteFileExists`
    ///   if `destination` is taken; otherwise why it couldn't be moved.
    private static func move(_ source: URL, to destination: URL) throws {
        let from = source.path(percentEncoded: false)
        let to = destination.path(percentEncoded: false)
        if renamex_np(from, to, UInt32(RENAME_EXCL)) == 0 { return }
        let code = errno
        // Across volumes, or on one that can't rename exclusively (some
        // network and FAT volumes: ENOTSUP, or EINVAL): copy instead.
        guard code == EXDEV || code == ENOTSUP || code == EINVAL else {
            throw POSIXError(POSIXErrorCode(rawValue: code) ?? .EIO)
        }
        let manager = FileManager.default
        try manager.copyItem(at: source, to: destination)
        // The copy is safe: if the temporary one can't be removed, the
        // system empties its folder in time.
        try? manager.removeItem(at: source)
    }
}
