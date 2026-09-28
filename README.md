# Rauser Browser Browsing Assistant

The Rauser Browser Browsing Assistant (Brauser) is a local-first browser assistant designed to help people keep useful context from the pages they visit. It uses a browser sidebar, Markdown notes, an optional native host,  and local Agent Harness integrations.

## Status

M0 is merged. M1 development code includes a Chrome side panel, opt-in visit capture, and native folder selection and confirmation. The side panel was then reworked (before M2) so page notes are editable and autosaving instead of create-only: the panel follows the active tab, shows its note, and saves the whole note about a second after typing stops, on tab change, and on close. The extension is an unpacked development build; there is no Chrome Web Store listing or installable release yet. M1 is accepted for development on macOS; the Windows interactive check is parked, so it is not ready for general download.

Read the [design specification](DESIGN.md) for the architecture, security model, planned features, and release plan.

## Planned support

- Google Chrome Stable on macOS 15 or later (Intel and Apple silicon)
- Google Chrome Stable on Windows 11 version 25H2 or later supported releases (x64)
- Edge and other browsers are not in the first-release support scope

The release plan calls for the browser extension to be distributed through the Chrome Web Store and a signed native-host installer to be provided for each supported operating system. See [Distribution](DESIGN.md#11-distribution) for details.

## Development

The pinned Rust toolchain is in `rust-toolchain.toml`. Python 3 generates protocol types, and Node.js runs the TypeScript build. From the repository root:

```sh
python3 protocol/generate.py --check
cargo build --locked --workspace --all-targets
cargo test --locked --workspace
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
npm ci --ignore-scripts
npm run typecheck
npm run lint
npm run build:extension
npm run test:extension
```

### Try the M1 development build

1. Build the native host with `cargo build --locked -p brauser` and the extension with `npm ci --ignore-scripts && npm run build:extension`.
2. In Google Chrome, open `chrome://extensions`, enable Developer mode, and load `extension/dist/` as an unpacked extension. Copy the extension ID shown there.
3. Register the development host for that ID. On macOS, run `node scripts/register-dev-host.mjs YOUR_EXTENSION_ID "$(pwd)/target/debug/brauser"` after replacing `YOUR_EXTENSION_ID`. On Windows, run the same script from PowerShell with your extension ID and the absolute path to `target\debug\brauser.exe`.
4. Open the Brauser side panel. Until setup is complete it warns that Brauser isn't set up; click the gear icon or **Open settings** to open the settings page in a tab. There, choose a notes folder, enter a site URL and path prefix, then enable the site. Chrome requests the site permission and the native host confirms the capture policy. The panel updates when settings change; it sends buffered visits and pauses capture. The panel also follows whichever tab is active: on any HTTP(S) page it loads that page's note (if any) and autosaves what you type. The first note on a page the extension has no standing Chrome access to needs the toolbar button, the **Add a note in Brauser** command (`Ctrl+Shift+9`/`Cmd+Shift+9`), or the page's right-click menu, which grant temporary access to that one tab; the panel can then request the page's exact origin so it keeps working there without repeating the trigger.

If **Choose folder** is disabled and the settings page says "Native host unavailable," check that the extension ID in the native-host registration matches the ID shown on `chrome://extensions`, rebuild the host at the registered path, then reload the settings page and reopen the panel. The development host registration is per user; the guided macOS smoke run below sets up its own isolated profile automatically.

For a guided macOS Chrome smoke run, build both components as above, then run:

```sh
npm run smoke:macos
```

The runner opens an isolated Chrome profile and a localhost fixture, loads `extension/dist/` through a local Chrome DevTools pipe, and verifies Chrome can exchange a `hello` with the native host. It then drives the side panel, the settings page, and the fixture tab itself, and stops only for the steps that need a person: the macOS folder picker, Chrome's permission prompt, and Brauser's confirmation dialog. Each prompt is named in the terminal, and the runner continues on its own once it sees the answer. Choose any empty folder when asked. At the end it asks whether a busy cursor stayed on screen after a dialog. The debugging flag and pipe apply only to the disposable profile; no remote debugging port is opened. It checks the host configuration, Chrome grants, visit log, page note autosave (including a simulated conflict from a stale save), and site removal after each step. The native host uses a temporary `HOME`, and its registration lives only in the isolated Chrome profile; the runner does not change the normal Chrome host registration. It preserves its profile and notes under the printed temporary directory for inspection. Run `node scripts/smoke-macos.mjs --help` for an alternate host or Chrome binary path, `--default-port` to serve the fixture on port 80, or `--auto` to answer every prompt through macOS UI scripting with no person present. `--auto` needs Accessibility access for the terminal (System Settings > Privacy & Security > Accessibility) and the Xcode command-line tools' `swiftc`; it takes keyboard focus while dialogs are open.

For routine checks that put nothing on screen, build the test host with `cargo build --locked -p brauser --features scripted-dialogs --target-dir target/scripted-dialogs` and run `npm run smoke:macos:headless`. It runs the same steps in headless Chrome. The host's dialog child takes each answer from a file in the run directory instead of showing the picker or confirmation; the host still mints and checks every folder and consent grant, and the runner checks the confirmation text. Chrome's permission prompt cannot be answered without UI, so the runner grants the access first through Chrome's own mechanisms: the first load of the extension copy lists `webNavigation` as required, and the site is granted from `chrome://extensions`. The run checks that no test process has a window or Dock icon. It does not exercise the real picker, alert, or Chrome prompt, the first-grant extension restart, or the busy-cursor fix, so run `--auto` or the guided run before accepting dialog or permission changes. The feature does not compile in release builds, and the runner refuses to use a scripted host outside `--headless`.

To check that the real macOS confirmation closes by itself, with no click, when the host goes away and when it times out, build with `cargo build --locked -p brauser && cargo build --locked -p brauser-macos-alert --example show_alert` and run `npm run check:macos-alert` in a macOS VM. It shows the real alert twice, closes the dialog child's stdin the way the host does, uses a 1-second timeout in place of the host's 260 seconds, and checks that `UserNotificationCenter` has no alert window left. It refuses to run outside a VM unless given `--on-desktop`.

The development registration in step 3 changes only the current user's Chrome native-host entry. It is for development; the signed installers planned for M5 will own installation and uninstallation. A folder path previously entered directly in an M0 config must be selected again through the native picker before M1 can use it. The host backs up malformed or incompatible config files during a revision-checked repair.

The host binds the chosen folder to its filesystem identity. If that folder is moved or replaced, choose it again before capture resumes. M1 keeps a synced visit-ID intent file in the OS config directory for each saved visit so retries can recover across log dates and interrupted appends. These files remain indefinitely in the development build; compacting the index is part of the public-release work.

Page notes are replaced whole under a version the panel names on every save (`load_note`/`save_note`); a save based on an out-of-date version is refused, and the host returns the note's current content instead of writing anything, so the panel can show it and let the user copy back what it could not save. The host checks the existing file's recorded canonical URL and Brauser ownership before replacing it; a mismatch is left untouched. Visit logging still checks the original URL against its confirmed site rule before normalizing or logging it, but notes work on any HTTP(S) page regardless of that allowlist. See [protocol/README.md](protocol/README.md) for the message contract.

The M1 smoke run has passed on macOS on both a nondefault and the default port, so M1 is accepted for development there. Windows interactive testing is parked until a Windows machine is available; Windows CI still builds and tests every change. Windows power-loss durability for newly created Markdown file names is not yet established. The public package remains a later release milestone.

## License

This repository is licensed under the [Apache License 2.0](LICENSE). The design specifies Apache-2.0 for the planned software as well.
