// Reports whether any of the given processes is visible to the user: its
// on-screen windows and whether it is a Dock or menu-bar app. Read-only; it
// needs no Accessibility access and cannot move focus.
// Usage: smoke-macos-windows PID... ; prints {"windows":N,"apps":N}.
import AppKit
import CoreGraphics

let pids = Set(CommandLine.arguments.dropFirst().compactMap { pid_t($0) })
let listed = CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID) as? [[String: Any]] ?? []
let windows = listed.filter { window in
  (window[kCGWindowOwnerPID as String] as? pid_t).map(pids.contains) ?? false
}.count
let apps = NSWorkspace.shared.runningApplications.filter { app in
  pids.contains(app.processIdentifier) && app.activationPolicy != .prohibited
}.count
print("{\"windows\":\(windows),\"apps\":\(apps)}")
