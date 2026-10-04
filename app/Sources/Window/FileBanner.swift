import Foundation
import LealFFI

/// The banner about the file itself, first in the window's stack: it
/// changed while Leal read it from a drive that can't make a snapshot
/// (1.1a), it was deleted on its network share while Leal read it
/// (ADR-0009), it changed or was deleted elsewhere (task 1.9, DESIGN §3.1),
/// or its removable drive was disconnected before Leal had read it all
/// (ADR-0006). At most one shows: the first that applies and wasn't
/// dismissed. Pure, so it is tested on its own.
enum FileBanner: Equatable, Sendable {
    /// Changed while it was read without a snapshot: what Leal holds may
    /// mix two versions, so only what was checked is shown. **Reload**.
    case changedWhileReading
    /// Deleted on its network share by another computer before Leal had
    /// read it all (ADR-0009): only the rows read are shown. **Save As…**.
    case deletedWhileReading
    /// Changed elsewhere (or replaced by another file). Leal shows the
    /// snapshot it took. **Reload** or **Keep Editing**.
    case changed
    /// Deleted, or moved to the Trash. **Save As…** or **Keep Editing**.
    case deleted
    /// Its removable drive was disconnected before the copy was complete.
    /// **Save As…**.
    case disconnected
    /// A read error stopped the index before the end of the file (phase 1
    /// review, app-8). **Reload**.
    case readStopped

    /// The banners that apply, most important first.
    static func applicable(
        changedWhileReading: Bool,
        original: OriginalState,
        storage: SourceStorage,
        readStopped: Bool = false
    ) -> [FileBanner] {
        var banners: [FileBanner] = []
        if changedWhileReading { banners.append(.changedWhileReading) }
        let deletedWhileReading = storage == .deleted
        if deletedWhileReading { banners.append(.deletedWhileReading) }
        switch original {
        case .changed: banners.append(.changed)
        // The same deletion, already said with what it means for the rows.
        case .deleted: if !deletedWhileReading { banners.append(.deleted) }
        case .unchanged, .unavailable: break
        }
        if storage == .disconnected { banners.append(.disconnected) }
        if readStopped { banners.append(.readStopped) }
        return banners
    }

    /// The key a dismissal is remembered by. It belongs to the file, not
    /// to a reading of it, so it lasts until **Reload** (see
    /// `DocumentViewController.updateBanners`).
    var key: String {
        switch self {
        case .changedWhileReading: "drive-changed"
        case .deletedWhileReading: "share-deleted"
        case .changed: "file-changed"
        case .deleted: "file-deleted"
        case .disconnected: "drive-disconnected"
        case .readStopped: "drive-read-stopped"
        }
    }

    var message: String {
        switch self {
        case .changedWhileReading: DiagnosticsText.changedWhileReading
        case .deletedWhileReading: Self.deletedWhileReadingMessage
        case .changed: Self.changedMessage
        case .deleted: Self.deletedMessage
        case .disconnected: DiagnosticsText.disconnected
        case .readStopped: Self.readStoppedMessage
        }
    }

    /// The prominent button: Reload where the file can be read again,
    /// otherwise Save As.
    var buttonTitle: String {
        switch self {
        case .changedWhileReading, .changed, .readStopped: Self.reload
        case .deletedWhileReading, .deleted, .disconnected: DiagnosticsText.saveAs
        }
    }

    /// The plain button after it, if any: Keep Editing, or, after a change
    /// while reading, Save As… (task 2.5.3c): Reload would discard the
    /// edits, and Save As keeps them in a copy of the rows Leal checked
    /// (ADR-0008 decision 6).
    var secondaryTitle: String? {
        switch self {
        case .changed, .deleted: Self.keepEditing
        case .changedWhileReading: DiagnosticsText.saveAs
        case .deletedWhileReading, .disconnected, .readStopped: nil
        }
    }

    /// Whether the plain button is Save As….
    var secondarySavesAs: Bool {
        self == .changedWhileReading
    }

    /// Whether the prominent button is Reload.
    var reloads: Bool {
        buttonTitle == Self.reload
    }

    static var changedMessage: String {
        String(
            localized: "This file changed on disk. Leal still shows it as it was when you opened it.",
            comment: "Banner: another app changed or replaced the open file (task 1.9, DESIGN §3.1)"
        )
    }

    static var deletedMessage: String {
        String(
            localized: "This file was deleted or moved to the Trash. Leal still shows it as it was; Save As keeps a copy.",
            comment: "Banner: the open file was deleted or moved to the Trash (task 1.9)"
        )
    }

    static var deletedWhileReadingMessage: String {
        String(
            localized: "This file was deleted on another computer while Leal was reading it. Leal shows the rows it had read; Save is off, and Save As keeps a copy.",
            comment: "Banner: another computer deleted the file on its network share before Leal had read it all (ADR-0009)"
        )
    }

    static var readStoppedMessage: String {
        String(
            localized: "Leal couldn’t read the rest of this file, so it shows only the rows it had read. Reload tries again.",
            comment: "Banner: a read error (a failing drive, or a full disk while copying) stopped Leal before the end of the file"
        )
    }

    static var reload: String {
        String(localized: "Reload", comment: "Banner button: open the file again as it is on disk now (task 1.9)")
    }

    static var keepEditing: String {
        String(localized: "Keep Editing", comment: "Banner button: keep the version Leal opened, and hide the banner (task 1.9)")
    }
}
