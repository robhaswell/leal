import AppKit
import QuartzCore

/// The grid's cells, or the gutter's numbers, drawn into strips of rows
/// that scrolling moves (ADR-0011, task 2.0b).
///
/// AppKit backs a view in a scroll view with a layer the size of the
/// visible rectangle, and on every scroll step rebuilds it from the view's
/// recorded drawing: the whole visible grid, though only a strip of it is
/// new (docs/tasks/1.6.md, "Scroll performance"; docs/tasks/2.0a.md). With
/// strips the view (`StripContentView`) draws nothing on screen itself: its
/// drawing (`drawContent`) goes into strips `rows` rows tall and as wide as
/// the view, each a `CALayer` with its own backing store, drawn
/// asynchronously (`drawsAsynchronously`: recorded on the main thread,
/// rasterised on Core Animation's threads). They are in a layer-hosting
/// view (`GridStripView`) over the clip view. A scroll only moves them; a
/// strip is drawn when it comes into view, when something in it changes,
/// or a strip's height ahead of the scroll, `ahead` a frame.
///
/// A horizontal scroll draws nothing: the strips are as wide as the view.
/// So their drawing and memory grow with the grid's width, where AppKit's
/// grow with the visible width; that is why grids much wider than the
/// visible area (`GridContainerView.stripLine`) keep AppKit's drawing.
///
/// Strips are placed relative to the visible rectangle, not the view's
/// origin: a grid of 40M rows is 880M points tall, past where Core
/// Animation's single-precision positions are exact. The drawing itself is
/// in the view's coordinates, as AppKit's is (`GridRenderingTests` checks
/// the last rows of such a grid).
@MainActor
final class GridStrips: NSObject, CALayerDelegate {
    /// Rows in a strip.
    static let rows = 4
    /// Strips drawn ahead of the scroll a frame, beyond those in view.
    static let ahead = 2
    /// Strips kept, with their backing stores, for the next to come into
    /// view, so a fling doesn't allocate one a frame.
    static let spares = 2

    /// How many times every strip was asked to redraw (`invalidateAll`),
    /// and how many times for a new width or scale: for the scroll
    /// benchmark.
    private(set) var wholeRedraws = 0
    private(set) var resizes = 0
    /// The grid's strips are as wide as the grid rounded up to this, so a
    /// live window resize of a grid narrower than the window redraws them
    /// only every so often, not on every step.
    static let gridWidthStep: CGFloat = 256

    /// The layer-hosting view the strips are in, over the clip view.
    let view = GridStripView()
    private unowned let content: StripContentView
    /// The strips' width is the view's rounded up to a multiple of this.
    private let widthStep: CGFloat
    /// The strips placed, by index (the first row is `index * rows`).
    private var strips: [Int: StripLayer] = [:]
    private var spare: [StripLayer] = []
    /// The rectangle of the view the strips were last placed for, in its
    /// coordinates: the clip view's bounds.
    private(set) var visible: CGRect = .zero
    /// The strips' width and scale, as last placed.
    private(set) var stripWidth: CGFloat = 0
    private(set) var scale: CGFloat = 0
    /// How many strips have been drawn, wholly or in part, for tests.
    private(set) var stripsDrawn = 0
    /// How many strip layers were made, for tests.
    private(set) var stripsMade = 0

    init(content: StripContentView, widthStep: CGFloat = 1) {
        self.content = content
        self.widthStep = widthStep
        super.init()
        view.strips = self
    }

    var stripHeight: CGFloat { CGFloat(Self.rows) * content.stripRowHeight }

    /// The strips' indexes that are placed, for tests.
    var placedIndexes: [Int] { strips.keys.sorted() }

    /// The strip at `index`, if placed, for tests.
    func strip(at index: Int) -> CALayer? { strips[index] }

    /// The rectangle of the view strip `index` shows, in its coordinates:
    /// the last strip ends with the view.
    func rect(ofStrip index: Int) -> CGRect {
        let top = CGFloat(index) * stripHeight
        let height = max(0, min(stripHeight, content.bounds.height - top))
        return CGRect(x: 0, y: top, width: stripWidth, height: height)
    }

    /// Places the strips for `visible` (the clip view's bounds, in the
    /// view's coordinates). Strips in view are drawn at the next commit if
    /// they are new; the next ones in the scroll's direction are drawn
    /// ahead, `ahead` a frame.
    func update(visible: CGRect) {
        let previous = self.visible
        self.visible = visible
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        let viewWidth = content.bounds.width
        let width = Self.width(for: viewWidth, step: widthStep)
        // The strips are cut where the view ends, so that scrolling past
        // its right edge (an elastic bounce) shows what AppKit shows there.
        let rootHeight = view.bounds.height
        let edge = CGRect(x: -visible.minX, y: 0, width: viewWidth, height: rootHeight)
        if view.clip.frame != edge { view.clip.frame = edge }
        let scale = view.backingScale
        if width != stripWidth || scale != self.scale {
            // Every strip is drawn again, at its new width or scale.
            resizes += 1
            stripWidth = width
            self.scale = scale
            for strip in strips.values {
                strip.contentsScale = scale
                strip.setNeedsDisplay()
            }
            for strip in spare {
                strip.contentsScale = scale
            }
        }
        let needed = indexes(in: visible)
        // Ahead: a strip's height past the visible area, both ways, the
        // scroll's direction first.
        var ahead = indexes(in: visible.insetBy(dx: 0, dy: -stripHeight)).filter { !needed.contains($0) }
        if visible.minY < previous.minY {
            ahead.sort(by: <)
        } else {
            ahead.sort(by: >)
        }
        for (index, strip) in strips where !needed.contains(index) && !ahead.contains(index) {
            strip.isHidden = true
            strips[index] = nil
            spare.append(strip)
        }
        for index in needed where strips[index] == nil {
            place(index)
        }
        var budget = Self.ahead
        for index in ahead where strips[index] == nil && budget > 0 {
            place(index)
            budget -= 1
        }
        for (index, strip) in strips {
            let rect = rect(ofStrip: index)
            if strip.bounds.size != rect.size {
                // The view grew or shrank under its last strip, or the
                // strips' width changed: drawn again whole.
                strip.bounds = CGRect(origin: .zero, size: rect.size)
                strip.setNeedsDisplay()
            }
            // In the layer that cuts them at the view's edge, which starts
            // at the view's left edge. Its coordinates, like the root
            // layer's, go up from the strips' view's bottom; positions are
            // relative to the visible rectangle.
            let frame = CGRect(
                x: rect.minX,
                y: rootHeight - (rect.minY - visible.minY) - rect.height,
                width: rect.width,
                height: rect.height
            )
            if strip.frame != frame { strip.frame = frame }
        }
        if spare.count > Self.spares {
            for strip in spare[Self.spares...] { strip.removeFromSuperlayer() }
            spare.removeSubrange(Self.spares...)
        }
    }

    /// The strips' width for a view `width` wide.
    static func width(for width: CGFloat, step: CGFloat) -> CGFloat {
        max(step, (width / step).rounded(.up) * step)
    }

    /// Shows strip `index` in a spare strip or a new one.
    private func place(_ index: Int) {
        let strip = spare.popLast() ?? makeStrip()
        strip.index = index
        strip.isHidden = false
        strip.contentsScale = scale
        strip.bounds = CGRect(origin: .zero, size: rect(ofStrip: index).size)
        strip.setNeedsDisplay()
        strips[index] = strip
        if strip.superlayer == nil { view.clip.addSublayer(strip) }
    }

    /// How many times strips were asked to redraw, for snapshots.
    private var invalidations = 0
    /// The strip being drawn.
    private var drawing: Int?

    /// Redraws the part of `rect` (in the view's coordinates) that strips
    /// show.
    func invalidate(_ rect: CGRect) {
        invalidations += 1
        for (index, strip) in strips {
            let part = rect.intersection(self.rect(ofStrip: index))
            guard !part.isNull, !part.isEmpty else { continue }
            if index == drawing {
                // Asked while this strip is being drawn (the ink of a row
                // just drawn spills into rows of the strip not drawn this
                // time): Core Animation drops that, and draws only its own
                // dirty rectangle, so it is asked again once the drawing
                // is over (the next frame; a snapshot shows it).
                deferred.append((index, part))
                scheduleDeferred()
                continue
            }
            markDirty(strip, part: part)
        }
    }

    /// `part` (in the view's coordinates) of the strip is to be drawn.
    private func markDirty(_ strip: StripLayer, part: CGRect) {
        let stripRect = rect(ofStrip: strip.index)
        // In the strip's own coordinates, which go up.
        strip.setNeedsDisplay(CGRect(
            x: part.minX - stripRect.minX,
            y: stripRect.maxY - part.maxY,
            width: part.width,
            height: part.height
        ))
    }

    /// Parts of the strip being drawn, asked for meanwhile.
    private var deferred: [(index: Int, part: CGRect)] = []
    private var deferredScheduled = false

    private func scheduleDeferred() {
        guard !deferredScheduled else { return }
        deferredScheduled = true
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { _ = self?.flushDeferred() }
        }
    }

    /// Asks again for what was deferred. Whether there was any.
    @discardableResult
    func flushDeferred() -> Bool {
        deferredScheduled = false
        let parts = deferred
        deferred = []
        for (index, part) in parts {
            if let strip = strips[index] { markDirty(strip, part: part) }
        }
        return !parts.isEmpty
    }

    /// Redraws every strip.
    func invalidateAll() {
        invalidations += 1
        wholeRedraws += 1
        for (index, strip) in strips {
            if index == drawing {
                // As in `invalidate`: asked while this strip is being drawn
                // (`GridView.noteSpill` forgetting the spills), which Core
                // Animation drops: all of it is asked again afterwards.
                deferred.append((index, rect(ofStrip: index)))
                scheduleDeferred()
                continue
            }
            strip.setNeedsDisplay()
        }
    }

    /// The window moved to a display of another scale, or the strips' view
    /// to another window.
    func backingChanged() {
        update(visible: visible)
    }

    /// The strips' indexes that cover `rect`, inside the view.
    private func indexes(in rect: CGRect) -> [Int] {
        let rect = rect.intersection(CGRect(x: 0, y: 0, width: max(1, content.bounds.width), height: content.bounds.height))
        guard !rect.isNull, rect.height > 0, stripHeight > 0 else { return [] }
        let first = Int((rect.minY / stripHeight).rounded(.down))
        let end = max(first + 1, Int((rect.maxY / stripHeight).rounded(.up)))
        return Array(first..<end)
    }

    private func makeStrip() -> StripLayer {
        let strip = StripLayer()
        strip.delegate = self
        strip.isOpaque = true
        strip.anchorPoint = .zero
        strip.needsDisplayOnBoundsChange = false
        strip.drawsAsynchronously = true
        // Eight bits a channel, as AppKit's own layers: not extended range.
        strip.contentsFormat = .RGBA8Uint
        stripsMade += 1
        return strip
    }

    // MARK: Snapshots

    /// Draws what the strips show of `rect` into `context`, whose user
    /// space is the view's (going down); `shown` is the part of the view
    /// the clip view shows, which the strips' view covers. What they don't
    /// show (outside the clip view) is drawn as the view draws it. For
    /// `cacheDisplay` (tests, screenshots): it draws the strips' backing
    /// stores where the strips are on screen, so a strip that is stale,
    /// blank or misplaced shows as it would on screen.
    func drawSnapshot(of rect: CGRect, shown: CGRect, into context: CGContext) {
        let inView = rect.intersection(shown)
        if inView.isNull || inView != rect {
            context.saveGState()
            // Outside the clip view: drawn directly.
            context.beginPath()
            context.addRect(rect)
            if !inView.isNull { context.addRect(inView) }
            context.clip(using: .evenOdd)
            content.drawDirectly(rect, context: context)
            context.restoreGState()
        }
        guard !inView.isNull, !inView.isEmpty else { return }
        context.saveGState()
        context.clip(to: inView)
        // The root layer's coordinates go up from the bottom of the strips'
        // view, which covers the clip view: its top is at `shown.minY`.
        context.translateBy(x: shown.minX, y: shown.minY + view.bounds.height)
        context.scaleBy(x: 1, y: -1)
        // Drawing a strip can ask for part of another to be drawn again
        // (ink spilling into it from a row seen for the first time:
        // `GridView.spills`), which on screen is drawn in the next frame:
        // the snapshot shows that frame.
        for _ in 0..<3 {
            let before = invalidations
            flushDeferred()
            view.root.render(in: context)
            guard invalidations != before else { break }
        }
        context.restoreGState()
    }

    // MARK: CALayerDelegate

    nonisolated func draw(_ layer: CALayer, in context: CGContext) {
        // Core Animation asks for a strip's drawing on the main thread, in
        // its commit, or in `render(in:)` (`drawsAsynchronously` only moves
        // the rasterising of what is drawn here): the layer and context
        // never leave it.
        nonisolated(unsafe) let layer = layer
        nonisolated(unsafe) let context = context
        MainActor.assumeIsolated {
            guard let strip = layer as? StripLayer else { return }
            stripsDrawn += 1
            let rect = rect(ofStrip: strip.index)
            context.saveGState()
            // The view draws in its own coordinates, which go down.
            context.translateBy(x: 0, y: strip.bounds.height)
            context.scaleBy(x: 1, y: -1)
            context.translateBy(x: -rect.minX, y: -rect.minY)
            let dirty = context.boundingBoxOfClipPath.intersection(CGRect(origin: rect.origin, size: strip.bounds.size))
            if !dirty.isNull, !dirty.isEmpty {
                drawing = strip.index
                content.drawDirectly(dirty, context: context)
                drawing = nil
            }
            context.restoreGState()
        }
    }

    /// No implicit animations: strips move and change at once.
    nonisolated func action(for layer: CALayer, forKey event: String) -> (any CAAction)? {
        NSNull()
    }
}

/// One strip, which knows which rows it shows.
final class StripLayer: CALayer {
    var index = 0

    override init() {
        super.init()
    }

    override init(layer: Any) {
        super.init(layer: layer)
        if let other = layer as? StripLayer { index = other.index }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }
}

/// No implicit animations for the layer that cuts the strips.
private final class StripActions: NSObject, CALayerDelegate, Sendable {
    nonisolated static let shared = StripActions()

    nonisolated func action(for layer: CALayer, forKey event: String) -> (any CAAction)? {
        NSNull()
    }
}

/// The layer-hosting view the strips are in, over the clip view. It takes
/// no clicks: they go to the grid view under it.
@MainActor
final class GridStripView: NSView {
    let root = CALayer()
    /// The strips' layer: it cuts them where the view they draw ends.
    let clip = CALayer()
    weak var strips: GridStrips?
    /// For tests: the scale of the display the window is on, in place of
    /// the window's (a test can't move its window between displays).
    var backingScaleForTesting: CGFloat?

    override init(frame: NSRect) {
        super.init(frame: frame)
        // Layer-hosting: the layer is set before `wantsLayer`.
        root.masksToBounds = true
        clip.masksToBounds = true
        clip.anchorPoint = .zero
        clip.delegate = StripActions.shared
        root.addSublayer(clip)
        layer = root
        wantsLayer = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    /// The scale strips are drawn at: the window's display's.
    var backingScale: CGFloat {
        backingScaleForTesting ?? window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 2
    }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    /// Moving the window to a display of another scale.
    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        strips?.backingChanged()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        strips?.backingChanged()
    }
}

/// A view whose drawing strips show on screen (`GridStrips`): the grid's
/// cells and the gutter. With strips it draws nothing on screen itself (its
/// layer stays empty, so AppKit records and replays nothing), and passes on
/// to its strips what needs redrawing: all of it (`needsDisplay`), or what
/// its own code says changed (`invalidate`).
///
/// AppKit's own requests to redraw part of it (`setNeedsDisplay(_:)`, the
/// strip a scroll exposes) are ignored: the strips already show it. So the
/// app's code says what changed with `invalidate(_:)`, never
/// `setNeedsDisplay(_:)`, which does nothing while there are strips.
///
/// Without strips (`strips == nil`: a grid too wide for them, or the scroll
/// benchmark's comparison) it is an ordinary view, drawn by AppKit.
@MainActor
class StripContentView: NSView {
    weak var strips: GridStrips? {
        didSet {
            guard strips !== oldValue else { return }
            // With strips, a change of size doesn't make AppKit redraw the
            // view: the strips redraw what the new size needs
            // (`GridStrips.update`).
            layerContentsRedrawPolicy = strips == nil ? .duringViewResize : .never
            // AppKit's drawing and the strips' don't share anything: what
            // either drew is stale.
            layer?.contents = nil
            super.needsDisplay = true
            strips?.invalidateAll()
        }
    }

    /// The rows' height, which strips are made of.
    var stripRowHeight: CGFloat { GridMetrics.rowHeight }

    /// Draws `rect`, in this view's coordinates, into `context`: AppKit's
    /// drawing (`draw(_:)`), and each strip's.
    func drawContent(in rect: CGRect, context: CGContext) {}

    /// Draws `rect` with the view's appearance and a flipped AppKit
    /// context: for strips and snapshots, which aren't in AppKit's drawing.
    func drawDirectly(_ rect: CGRect, context: CGContext) {
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: true)
        effectiveAppearance.performAsCurrentDrawingAppearance {
            drawContent(in: rect, context: context)
        }
        NSGraphicsContext.restoreGraphicsState()
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current?.cgContext else { return }
        drawContent(in: dirtyRect, context: context)
    }

    override var wantsUpdateLayer: Bool { strips != nil }

    /// On screen the strips draw; this view's layer stays empty.
    override func updateLayer() {
        layer?.contents = nil
    }

    override var needsDisplay: Bool {
        get { super.needsDisplay }
        set {
            guard let strips else {
                super.needsDisplay = newValue
                return
            }
            if newValue { strips.invalidateAll() }
        }
    }

    override func setNeedsDisplay(_ invalidRect: NSRect) {
        // With strips, this view's layer is empty: nothing AppKit asks to
        // redraw (the strip a scroll exposes) needs drawing.
        guard strips == nil else { return }
        super.setNeedsDisplay(invalidRect)
    }

    /// Redraws `rect`, which changed: in every strip placed (those ahead of
    /// the scroll too), or without strips in view.
    func invalidate(_ rect: NSRect) {
        guard let strips else {
            let part = rect.intersection(visibleRect.insetBy(dx: -2, dy: -2))
            if !part.isNull, !part.isEmpty { super.setNeedsDisplay(part) }
            return
        }
        strips.invalidate(rect)
    }

    /// With strips, the clip view's bounds: what the strips' view, over
    /// the clip view, shows of this view.
    private var shownRect: CGRect {
        (superview as? NSClipView)?.bounds ?? visibleRect
    }

    /// Snapshots (tests, screenshots) draw what the strips show: see
    /// `GridStrips.drawSnapshot`.
    override func cacheDisplay(in rect: NSRect, to bitmapImageRep: NSBitmapImageRep) {
        guard let strips, rect.width > 0, rect.height > 0,
              let graphics = NSGraphicsContext(bitmapImageRep: bitmapImageRep)
        else {
            super.cacheDisplay(in: rect, to: bitmapImageRep)
            return
        }
        let context = graphics.cgContext
        context.saveGState()
        // From the bitmap's pixels, which go up, to this view's points,
        // which go down from `rect`'s top left.
        context.concatenate(context.ctm.inverted())
        context.scaleBy(x: CGFloat(bitmapImageRep.pixelsWide) / rect.width, y: CGFloat(bitmapImageRep.pixelsHigh) / rect.height)
        context.translateBy(x: 0, y: rect.height)
        context.scaleBy(x: 1, y: -1)
        context.translateBy(x: -rect.minX, y: -rect.minY)
        strips.drawSnapshot(of: rect, shown: shownRect, into: context)
        context.restoreGState()
        graphics.flushGraphics()
    }
}

/// Views over the cells, above the strips, that follow the scroll: the
/// place for the in-cell editor (task 2.5) and the invalid-bytes callout
/// (ADR-0011). A view is shown at a rectangle in the grid's coordinates
/// (`GridLayout.cellRect`) and kept there as the grid scrolls. Like the
/// strips, it is placed relative to the visible rectangle, so it stays
/// exact in a grid of 880M points; and like them it is in the scroll view
/// over the clip view, under the scrollers. Clicks outside the views shown
/// go through to the grid.
///
/// A view shown here is an ordinary view in the window (it can be first
/// responder, and an editor's marked text and candidate window are placed
/// from its own frame), and is drawn by AppKit, not into the strips.
@MainActor
final class GridOverlayView: NSView {
    /// Each view shown, by identity, with its rectangle in the grid.
    private var rects: [ObjectIdentifier: CGRect] = [:]
    /// The grid's visible rectangle, as last followed.
    private(set) var visible: CGRect = .zero

    override init(frame: NSRect) {
        super.init(frame: frame)
        isHidden = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    override var isFlipped: Bool { true }

    /// Shows `view` over the grid's `rect`, or moves it there.
    func show(_ view: NSView, at rect: CGRect) {
        rects[ObjectIdentifier(view)] = rect
        if view.superview !== self { addSubview(view) }
        view.frame = rect.offsetBy(dx: -visible.minX, dy: -visible.minY)
        isHidden = false
    }

    /// The rectangle `view` is shown at, in the grid's coordinates.
    func rect(of view: NSView) -> CGRect? {
        rects[ObjectIdentifier(view)]
    }

    /// The grid scrolled, or its visible area changed size.
    func follow(visible: CGRect) {
        self.visible = visible
        for view in subviews {
            guard let rect = rects[ObjectIdentifier(view)] else { continue }
            let frame = rect.offsetBy(dx: -visible.minX, dy: -visible.minY)
            if view.frame != frame { view.frame = frame }
        }
    }

    override func willRemoveSubview(_ subview: NSView) {
        super.willRemoveSubview(subview)
        rects[ObjectIdentifier(subview)] = nil
        if subviews.count <= 1 { isHidden = true }
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        let hit = super.hitTest(point)
        return hit === self ? nil : hit
    }
}
