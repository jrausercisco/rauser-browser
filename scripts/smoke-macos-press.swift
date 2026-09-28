// Presses a button in another app's sheet through the Accessibility API, for
// `smoke-macos.mjs --auto`. System Events cannot see Chrome's permission sheet.
// Usage: smoke-macos-press <pid> <button title>
// Exits 0 once pressed, 2 when no sheet shows that button.
import ApplicationServices

func attribute(_ element: AXUIElement, _ name: String) -> AnyObject? {
  var value: AnyObject?
  return AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success ? value : nil
}

func find(_ element: AXUIElement, _ title: String, inSheet: Bool, depth: Int) -> AXUIElement? {
  if depth > 30 { return nil }
  let role = attribute(element, "AXRole") as? String
  // Skip page content; the prompt is browser UI.
  if role == "AXWebArea" { return nil }
  if inSheet && role == "AXButton" && attribute(element, "AXTitle") as? String == title { return element }
  for child in (attribute(element, "AXChildren") as? [AXUIElement]) ?? [] {
    if let found = find(child, title, inSheet: inSheet || role == "AXSheet", depth: depth + 1) { return found }
  }
  return nil
}

let arguments = CommandLine.arguments
guard arguments.count == 3, let pid = pid_t(arguments[1]) else {
  FileHandle.standardError.write("usage: smoke-macos-press <pid> <button title>\n".data(using: .utf8)!)
  exit(64)
}
let app = AXUIElementCreateApplication(pid)
// Chrome builds its accessibility tree only once a client asks for it.
AXUIElementSetAttributeValue(app, "AXManualAccessibility" as CFString, kCFBooleanTrue)
for window in (attribute(app, "AXWindows") as? [AXUIElement]) ?? [] {
  if let button = find(window, arguments[2], inSheet: false, depth: 0) {
    exit(AXUIElementPerformAction(button, "AXPress" as CFString) == .success ? 0 : 1)
  }
}
exit(2)
