import AppKit
import LealFFI

/// A plain window showing what the Rust core read from a file. Every value
/// shown comes from Rust; this file only lays it out.
@MainActor
enum SummaryWindow {
    static func make(url: URL, summary: FileSummary, coreVersion: String) -> NSWindow {
        let rows: [(String, String)] = [
            ("File", url.lastPathComponent),
            ("Size", "\(summary.byteCount.formatted()) bytes"),
            ("First line", summary.firstLine),
            ("Core", "leal-core \(coreVersion)"),
        ]

        let grid = NSGridView(views: rows.map { label, value in
            [labelField(label), valueField(value)]
        })
        grid.rowSpacing = 8
        grid.columnSpacing = 12
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.translatesAutoresizingMaskIntoConstraints = false

        let content = NSView()
        content.addSubview(grid)
        NSLayoutConstraint.activate([
            grid.topAnchor.constraint(equalTo: content.topAnchor, constant: 20),
            grid.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -20),
            grid.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 20),
            grid.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -20),
            grid.widthAnchor.constraint(greaterThanOrEqualToConstant: 360),
        ])

        let window = NSWindow(
            contentRect: .zero,
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.isReleasedWhenClosed = false
        window.title = url.lastPathComponent
        window.representedURL = url
        window.contentView = content
        window.setContentSize(content.fittingSize)
        window.center()
        return window
    }

    private static func labelField(_ text: String) -> NSTextField {
        let field = NSTextField(labelWithString: text)
        field.textColor = .secondaryLabelColor
        field.setContentHuggingPriority(.required, for: .horizontal)
        return field
    }

    private static func valueField(_ text: String) -> NSTextField {
        let field = NSTextField(wrappingLabelWithString: text)
        field.isSelectable = true
        field.font = .monospacedSystemFont(ofSize: NSFont.systemFontSize, weight: .regular)
        // Long first lines (up to 200 bytes) wrap at this width.
        field.preferredMaxLayoutWidth = 480
        return field
    }
}
