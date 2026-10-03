import Foundation
import LealFFI

/// The wording of the alert shown when a file can't be opened.
///
/// The Rust core reports what went wrong as a structured `LealError` (the
/// path, the kind and the OS error code); the words are chosen here, from the
/// String Catalog (DESIGN §4.4). `LealError.Io`'s `message` is English for
/// logs and never shown.
///
/// This file is also compiled into the `LealTests` bundle (see
/// `project.yml`), so the wording is tested against real errors from Rust.
enum OpenErrorText {
    /// The alert's title, for example "Leal couldn’t open “data.csv”."
    static func title(fileName: String) -> String {
        String(localized: "Leal couldn’t open “\(fileName)”.", comment: "Alert title when a file can't be opened")
    }

    /// A sentence explaining an error thrown by the Rust core.
    ///
    /// The switch over `LealError` is exhaustive, with no `default`, so a new
    /// variant doesn't compile until it has wording here. Otherwise it would
    /// show UniFFI's debug-style description (`LealError.X(...)`).
    ///
    /// Whatever the error's static type, the words come from the catalog:
    /// a `JobFailure` (a background job's end) has its own wording, and an
    /// error from Foundation has the system's. Anything else is one of
    /// UniFFI's internal errors, most often a Rust panic in a call, whose
    /// description is the panic message in English (for logs only, never
    /// shown; phase 1 review, app-1): it gets the after-a-panic wording.
    static func describe(_ error: any Error) -> String {
        if let failure = error as? JobFailure {
            return describe(failure)
        }
        guard let error = error as? LealError else {
            let system = error as NSError
            if [NSCocoaErrorDomain, NSPOSIXErrorDomain, NSOSStatusErrorDomain].contains(system.domain) {
                // Such as making the temporary folders: the system's words.
                return system.localizedDescription
            }
            return documentFailed
        }
        return switch error {
        case .NotFound:
            String(localized: "The file doesn’t exist.", comment: "Open error: nothing at the path")
        case .PermissionDenied:
            String(
                localized: "You don’t have permission to open it.",
                comment: "Open error: the file can't be read (EACCES or EPERM)"
            )
        case .NotAFile(_, isDirectory: true):
            String(localized: "It’s a folder, not a file.", comment: "Open error: the path is a folder")
        case .NotAFile(_, isDirectory: false):
            String(
                localized: "It isn’t a regular file.",
                comment: "Open error: the path is a named pipe, socket or device"
            )
        case let .Io(_, code?, _):
            systemReason(code: code)
        case .Io(_, nil, _), .Internal:
            String(localized: "An unexpected error occurred.", comment: "Open error with no OS error code")
        case .TooLarge:
            String(
                localized: "It’s 4 GB or larger, more than Leal can open.",
                comment: "Open error: the file is 4 GiB or more (DESIGN §1)"
            )
        case .EncodingDoesNotFit:
            String(
                localized: "That encoding doesn’t match the file’s byte order mark.",
                comment: "Open error: Reopen with Encoding chose an encoding the file's BOM rules out"
            )
        case .DriveDisconnected:
            String(
                localized: "The drive it’s on was disconnected.",
                comment: "Open error: the removable drive holding the file vanished"
            )
        case .ChangedOnDisk:
            String(
                localized: "Another app changed it while Leal was reading it.",
                comment: "Open error: the file changed while it was being read without a snapshot"
            )
        case .DeletedElsewhere:
            deletedElsewhere
        case .UnsavedEdits:
            // ADR-0008 decision 4's reason. Task 2.5 disables Treat As and
            // Reopen with Encoding while there are edits; this is the core
            // refusing if one gets through.
            String(
                localized: "Save or revert your changes first.",
                comment: "Error: the file can't be read with another delimiter or encoding while it has unsaved edits"
            )
        case .Saving:
            // Task 2.5 disables re-reading while it saves; this is the core
            // refusing if one gets through.
            String(
                localized: "Wait for the save to finish.",
                comment: "Error: the file can't be read another way while it is being saved"
            )
        case .EditRefused:
            // Task 2.5 words each refusal where the edit is made.
            String(localized: "The change couldn’t be made.", comment: "Error: an edit, undo or redo wasn't applied")
        case .DocumentFailed:
            documentFailed
        }
    }

    /// A sentence explaining why a background job (indexing, the review)
    /// didn't finish. The switch is exhaustive, as for `LealError`.
    static func describe(_ failure: JobFailure) -> String {
        switch failure {
        case .Cancelled:
            String(localized: "It was stopped.", comment: "Background job error: the job was cancelled")
        case .DriveDisconnected:
            String(
                localized: "The drive it’s on was disconnected.",
                comment: "Open error: the removable drive holding the file vanished"
            )
        case .ChangedOnDisk:
            String(
                localized: "Another app changed it while Leal was reading it.",
                comment: "Open error: the file changed while it was being read without a snapshot"
            )
        case .DeletedElsewhere:
            deletedElsewhere
        case .Panicked:
            documentFailed
        case .Failed:
            String(localized: "An unexpected error occurred.", comment: "Open error with no OS error code")
        }
    }

    /// The file on a network share was deleted by another computer while
    /// Leal read it (ADR-0009).
    private static var deletedElsewhere: String {
        String(
            localized: "Another computer deleted it while Leal was reading it.",
            comment: "Open error: the file on a network share was deleted elsewhere while it was being read (ADR-0009)"
        )
    }

    /// After a panic in the core the document has failed (DESIGN §3.9): the
    /// app makes no more calls on it and offers to reopen the file.
    private static var documentFailed: String {
        String(
            localized: "Something went wrong inside Leal. Close the file and open it again.",
            comment: "Error after a panic in the Rust core: the document can't be used any more"
        )
    }

    /// The system's own description of a POSIX error code, such as "No space
    /// left on device", with the code for bug reports.
    private static func systemReason(code: Int32) -> String {
        let error = NSError(domain: NSPOSIXErrorDomain, code: Int(code))
        let reason = error.localizedFailureReason ?? error.localizedDescription
        return String(
            localized: "\(reason) (error \(code)).",
            comment: "Open error: the system's description of an error, then its POSIX error code"
        )
    }
}
