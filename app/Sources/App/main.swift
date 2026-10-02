import AppKit

// AppKit app lifecycle, with no storyboard and no SwiftUI `App`: create the
// shared application, give it a delegate and run the event loop. `delegate`
// is a global, so it lives as long as the app (`NSApplication.delegate` is
// a weak reference).
//
// The first `NSDocumentController` made is the shared one: Leal's, which
// opens files off the main thread (task 2.0, ADR-0009). Made first, before
// anything could ask for the shared one.
_ = DocumentController()
let delegate = AppDelegate()
NSApplication.shared.delegate = delegate
NSApplication.shared.run()
