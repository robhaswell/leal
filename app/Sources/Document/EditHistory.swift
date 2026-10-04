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
    /// Called before an undo or a redo: commits an open edit. `false`
    /// (the core refused it, and it stays open, saying why) stops the
    /// step, with a beep, as Reload and Treat As stop.
    var willUndoOrRedo: (() -> Bool)?
    /// Called after one: the document deals with a command the core
    /// refused meanwhile.
    var didUndoOrRedo: (() -> Void)?
    /// Whether undo and redo are possible now: not once the document has
    /// failed (its journal recovers the edits), nor while a save replaces
    /// the file.
    var isAvailable: () -> Bool = { true }
    /// A step refused for now waits to go back where it came from once
    /// its undo or redo is over (`putBack`): `true` to its undo stack.
    private var putBackOnUndoStack: Bool?
    /// A refused step is being put back: the undo or redo that does it is
    /// allowed whatever `isAvailable` says.
    private var puttingBack = false

    override init() {
        super.init()
        groupsByEvent = false
    }

    override var canUndo: Bool { (puttingBack || isAvailable()) && super.canUndo }
    override var canRedo: Bool { (puttingBack || isAvailable()) && super.canRedo }

    override func undo() {
        guard willUndoOrRedo?() ?? true else { return NSSound.beep() }
        guard canUndo else { return }
        super.undo()
        finishPuttingBack()
        didUndoOrRedo?()
    }

    override func redo() {
        guard willUndoOrRedo?() ?? true else { return NSSound.beep() }
        guard canRedo else { return }
        super.redo()
        finishPuttingBack()
        didUndoOrRedo?()
    }

    /// Called during an undo or a redo the core refused for now (the file
    /// is still being read, a save runs): the step goes back where it came
    /// from, `action` named `name`, so the user can try again, and both
    /// histories stay as they were. A step registered during the undo
    /// lands on the redo stack (during a redo, the undo stack), so this
    /// registers one there whose own undo (or redo) registers `action`,
    /// and takes it straight back once the undo is over
    /// (`finishPuttingBack`): registered during that redo (or undo),
    /// `action` lands on the stack the refused step came from, and nothing
    /// clears the redo stack, as a new step would.
    func putBack(named name: String, _ action: @escaping @MainActor () -> Void) {
        guard isUndoing || isRedoing else { return }
        registerUndo(withTarget: self) { [unowned self] _ in registerStep(named: name, action) }
        setActionName(name)
        putBackOnUndoStack = isUndoing
    }

    private func finishPuttingBack() {
        guard let onUndoStack = putBackOnUndoStack else { return }
        putBackOnUndoStack = nil
        puttingBack = true
        defer { puttingBack = false }
        if onUndoStack {
            super.redo()
        } else {
            super.undo()
        }
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

    /// Puts back an undo or redo step of `command` the core refused for now
    /// (`direction` is the step's), to try again: called during that undo
    /// or redo. `apply` applies it.
    func putBack(_ command: EditCommand, as direction: CommandDirection, apply: @escaping @MainActor (EditCommand, CommandDirection) -> Void) {
        undoManager.putBack(named: Self.actionName(for: command)) { apply(command, direction) }
    }

    func noteToken(_ token: Any, version: UInt64) {
        tokens.append((version, token))
    }

    /// The change-count token noted at edit version `version` (the last
    /// one noted at or before it), for a save that wrote the edits up to
    /// that version. A save applies it with
    /// `updateChangeCount(withToken:for: .saveOperation)`, then calls
    /// `savedThrough(version:)` (`CSVDocument.saveFinished`).
    func token(atVersion version: UInt64) -> Any? {
        tokens.last { $0.version <= version }?.token
    }

    /// A save wrote the edits up to `version`: the journal keeps only the
    /// commands after it (the earlier ones are in the file now, so a replay
    /// into it would refuse them), and the tokens before it go.
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
    ///
    /// The pairing: the core's `replay` returns in `applied` one command
    /// for each command passed that wasn't refused, in the order passed,
    /// and `replayCommands` passes the journal's entries in order. So the
    /// journal's entries not named in `refused`, in order, pair with
    /// `applied` one for one.
    func rebuild(
        from report: ReplayReport,
        version: UInt64,
        choices: ReadingChoices,
        apply: @escaping @MainActor (EditCommand, CommandDirection) -> Void
    ) {
        let old = journal
        reset(choices: choices)
        let refused = Set(report.refused.map { Int($0.index) })
        assert(
            report.applied.count == old.count - refused.count,
            "a replay applies each command it doesn't refuse: \(report.applied.count) applied, \(refused.count) refused of \(old.count)"
        )
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
                return StructureText.deleteColumn
            case (false, true):
                return rows == 1
                    ? String(localized: "Insert Row", comment: "Undo menu: Undo Insert Row")
                    : String(localized: "Insert Rows", comment: "Undo menu: Undo Insert Rows")
            case (false, false):
                return rows == 1 ? StructureText.deleteRow : StructureText.deleteRows
            }
        }
        if command.changes.count == 1 {
            return String(localized: "Typing", comment: "Undo menu: Undo Typing, a cell's edit")
        }
        return String(localized: "Edit Cells", comment: "Undo menu: Undo Edit Cells, several cells changed at once")
    }
}
