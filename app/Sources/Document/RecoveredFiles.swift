import Foundation

/// Where Leal keeps a file it replaced but may not delete (task 2.5.3b):
/// the core's `SaveOutcome.keptOldFile`, an old file the save's swap took
/// out that couldn't be checked, so it may be another app's version
/// (task 2.2). The core leaves it in the save's temporary folder, which the
/// system may empty, so the app moves it here at once, to Leal's Recovered
/// folder in Application Support (inside the sandbox's container), and
/// tells the user.
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

    /// Moves the file at `path` into `folder`, under its own name, or with
    /// " 2", " 3"… before its extension if that is taken, and returns where
    /// it is now. The folder it was in is removed if that leaves it empty
    /// (the save's temporary folder). Any thread: it may copy across
    /// volumes.
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
                try manager.moveItem(at: source, to: destination)
                _ = rmdir(source.deletingLastPathComponent().path(percentEncoded: false))
                return destination
            } catch CocoaError.fileWriteFileExists where number < 1000 {
                number += 1
            }
        }
    }
}
