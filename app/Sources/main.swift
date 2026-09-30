import AppKit

// AppKit app lifecycle, with no storyboard and no SwiftUI `App`: create the
// shared application, give it a delegate and run the event loop. `delegate`
// is a global, so it lives as long as the app (`NSApplication.delegate` is
// a weak reference).
let delegate = AppDelegate()
NSApplication.shared.delegate = delegate
NSApplication.shared.run()
