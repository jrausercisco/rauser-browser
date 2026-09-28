// Reports whether any of the given processes is visible to the user: its
// on-screen windows (which include menu-bar items) and whether it is a Dock
// app. An accessory app with no window, such as Chrome's notification helper
// that even headless Chrome starts, shows nothing. Read-only; it needs no
// Accessibility access and cannot move focus.
// Usage: smoke-macos-windows PID... ; prints {"windows":N,"apps":N,"visible":[...]},
// where visible names each counted app so a failure says which process it was.
import AppKit
import CoreGraphics
import Foundation

let pids = Set(CommandLine.arguments.dropFirst().compactMap { pid_t($0) })
let listed = CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID) as? [[String: Any]] ?? []
let windows = listed.filter { window in
  (window[kCGWindowOwnerPID as String] as? pid_t).map(pids.contains) ?? false
}
let apps = NSWorkspace.shared.runningApplications.filter { app in
  pids.contains(app.processIdentifier) && app.activationPolicy == .regular
}
let visible: [[String: Any]] = apps.map { app in
  ["pid": Int(app.processIdentifier), "bundle": app.bundleIdentifier ?? "",
   "path": app.executableURL?.path ?? ""]
} + windows.map { window in
  ["pid": Int(window[kCGWindowOwnerPID as String] as? pid_t ?? 0),
   "window": window[kCGWindowOwnerName as String] as? String ?? ""]
}
let report: [String: Any] = ["windows": windows.count, "apps": apps.count, "visible": visible]
let data = try JSONSerialization.data(withJSONObject: report)
print(String(decoding: data, as: UTF8.self))
