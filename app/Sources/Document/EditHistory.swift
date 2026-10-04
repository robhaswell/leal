import AppKit
import LealFFI

/// How a reading split the file into cells, and whether row 1 is the
/// header row: what **Recover changes** opens the file afresh with (DESIGN
/// §3.6, ADR-0008 decision 5).
struct ReadingChoices: Equatable, Sendable {
    let delimiter: Delimiter
    let encoding: TextEncoding
    let header: Bool

    init(_ interpretation: Interpretation) {
        delimiter = interpretation.delimiter
        encoding = interpretation.encoding
        header = interpretation.header
    }
}

/// One command the document applied, in the recovery journal.
struct JournalEntry {
    /// The command as made (for an undo, the one undone).
    let command: EditCommand
    let direction: CommandDirection
    /// The core's `editVersion()` just after it applied: a save's
    /// `snapshotVersion` says which entries the saved file holds.
    let version: UInt64

    /// What the core applied: the command, or for an undo its inverse.
    var applied: EditCommand {
        direction == .undo ? inverseCommand(command: command) : command
    }
}

/// The document's undo manager. Leal groups each command itself, so no
/// event closes a group under it; and each undo or redo first commits an
/// edit still open in the window, so ⌘Z while typing undoes the typing.
@MainActor
final class DocumentUndoManager: UndoManager {
    /// Called before an undo or a redo: commits an open edit.
    var willUndoOrRedo: (() -> Void)?
    /// Called after one: the document deals with a command the core
    /// refused meanwhile.
    var didUndoOrRedo: (() -> Void)?
    /// Whether undo and redo are possible now: not once the document has
    /// failed (its journal recovers the edits), nor while a save replaces
    /// the file.
    var isAvailable: () -> Bool = { true }

    override init() {
        super.init()
        groupsByEvent = false
    }

    override var canUndo: Bool { isAvailable() && super.canUndo }
    override var canRedo: Bool { isAvailable() && super.canRedo }

    override func undo() {
        willUndoOrRedo?()
        guard canUndo else { return }
        super.undo()
        didUndoOrRedo?()
    }

    override func redo() {
        willUndoOrRedo?()
        guard canRedo else { return }
        super.redo()
        didUndoOrRedo?()
    }

    /// Registers `action` as one undo step named `name`. Inside an undo or
    /// a redo the manager has its group open already; otherwise the step is
    /// a group of its own.
    func registerStep(named name: String, _ action: @escaping @MainActor () -> Void) {
        let ownGroup = !isUndoing && !isRedoing
        if ownGroup { beginUndoGrouping() }
        registerUndo(withTarget: self) { _ in action() }
        setActionName(name)
        if ownGroup { endUndoGrouping() }
    }
}

/// A document's edit history (task 2.5.2): its undo manager over the
/// core's commands, and the recovery journal, an append-only record of
/// every command applied (edits, undos and redos, in order) with the
/// reading's choices, because an `NSUndoManager` can't be listed (DESIGN
/// §3.6). Also the `NSDocument` change-count tokens, noted with the core's
/// edit version after each command, for a save to apply (2.2, PLAN 2.5.2).
@MainActor
final class EditHistory {
    let undoManager = DocumentUndoManager()
    private(set) var journal: [JournalEntry] = []
    /// The reading's choices, as of the last command or reset.
    private(set) var choices: ReadingChoices?
    /// `NSDocument.changeCountToken(for: .saveOperation)` after each
    /// command, with the edit version then.
    private var tokens: [(version: UInt64, token: Any)] = []

    /// Appends a command the core applied to the journal.
    func record(_ command: EditCommand, as direction: CommandDirection, version: UInt64, choices: ReadingChoices) {
        journal.append(JournalEntry(command: command, direction: direction, version: version))
        self.choices = choices
    }

    /// The reading's choices changed without a command (the header-row
    /// toggle): the edits stay.
    func readingChanged(_ choices: ReadingChoices) {
        self.choices = choices
    }

    /// Registers the step that takes `command` back: after an edit or a
    /// redo, its undo; after an undo, its redo. `apply` applies it.
    func register(_ command: EditCommand, as direction: CommandDirection, apply: @escaping @MainActor (EditCommand, CommandDirection) -> Void) {
        let next: CommandDirection = direction == .undo ? .redo : .undo
        undoManager.registerStep(named: Self.actionName(for: command)) { apply(command, next) }
    }

    func noteToken(_ token: Any, version: UInt64) {
        tokens.append((version, token))
    }

    /// The change-count token noted at edit version `version` (the last
    /// one noted at or before it), for a save that wrote the edits up to
    /// that version. SEAM(2.5.3): a save applies it with
    /// `updateChangeCount(withToken:for: .saveOperation)`, then calls
    /// `savedThrough(version:)`.
    func token(atVersion version: UInt64) -> Any? {
        tokens.last { $0.version <= version }?.token
    }

    /// A save wrote the edits up to `version`: the journal keeps only the
    /// commands after it (the earlier ones are in the file now, so a replay
    /// into it would refuse them), and the tokens before it go.
    /// SEAM(2.5.3).
    func savedThrough(version: UInt64) {
        journal.removeAll { $0.version <= version }
        if let index = tokens.lastIndex(where: { $0.version <= version }) {
            tokens.removeFirst(index)
        }
    }

    /// The edits are gone (a Reload, Revert, or a new split): no undo
    /// history, journal or tokens.
    func reset(choices: ReadingChoices?) {
        undoManager.removeAllActions()
        journal.removeAll()
        tokens.removeAll()
        self.choices = choices
    }

    /// The commands to replay, oldest first: each as the core applied it.
    var replayCommands: [EditCommand] {
        journal.map(\.applied)
    }

    /// After a replay (Recover changes), the history of the recovered
    /// document: the journal becomes the commands that applied, as edits,
    /// in the new document's lineage, and the undo history the steps still
    /// done, oldest first (a redo history isn't kept). `apply` applies a
    /// step's undo or redo.
    func rebuild(
        from report: ReplayReport,
        version: UInt64,
        choices: ReadingChoices,
        apply: @escaping @MainActor (EditCommand, CommandDirection) -> Void
    ) {
        let old = journal
        reset(choices: choices)
        let refused = Set(report.refused.map { Int($0.index) })
        var applied = report.applied.makeIterator()
        var done: [EditCommand] = []
        for (index, entry) in old.enumerated() where !refused.contains(index) {
            guard let command = applied.next() else { break }
            journal.append(JournalEntry(command: command, direction: .edit, version: version))
            switch entry.direction {
            case .edit, .redo:
                done.append(command)
            case .undo:
                // The step it undid comes off, if that applied.
                if !done.isEmpty { done.removeLast() }
            }
        }
        for command in done {
            register(command, as: .edit, apply: apply)
        }
    }

    /// The Edit menu's name for undoing or redoing `command`: "Undo
    /// Typing", "Undo Delete Row" and so on.
    static func actionName(for command: EditCommand) -> String {
        if let structural = command.structural {
            let rows = structural.rowCount()
            switch (structural.isColumn(), structural.inserts()) {
            case (true, true):
                return String(localized: "Insert Column", comment: "Undo menu: Undo Insert Column")
            case (true, false):
                return String(localized: "Delete Column", comment: "Undo menu: Undo Delete Column")
            case (false, true):
                return rows == 1
                    ? String(localized: "Insert Row", comment: "Undo menu: Undo Insert Row")
                    : String(localized: "Insert Rows", comment: "Undo menu: Undo Insert Rows")
            case (false, false):
                return rows == 1
                    ? String(localized: "Delete Row", comment: "Undo menu: Undo Delete Row")
                    : String(localized: "Delete Rows", comment: "Undo menu: Undo Delete Rows")
            }
        }
        if command.changes.count == 1 {
            return String(localized: "Typing", comment: "Undo menu: Undo Typing, a cell's edit")
        }
        return String(localized: "Edit Cells", comment: "Undo menu: Undo Edit Cells, several cells changed at once")
    }
}
