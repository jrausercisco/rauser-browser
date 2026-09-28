# Rauser Browser

Rauser is a local-first browser assistant designed to help people keep useful context from the pages they visit. It uses a browser sidebar, Markdown notes, an optional native host,  and local Agent Harness integrations.

## Status

M0 is merged. M1 development code now includes a Chrome side panel, opt-in visit capture, native folder selection and confirmation, and local Markdown page notes. The extension is an unpacked development build; there is no Chrome Web Store listing or installable release yet. The M1 flow has not had an interactive Chrome check on both operating systems, so it is not ready for general download.

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

1. Build the native host with `cargo build --locked -p rauser` and the extension with `npm ci --ignore-scripts && npm run build:extension`.
2. In Google Chrome, open `chrome://extensions`, enable Developer mode, and load `extension/dist/` as an unpacked extension. Copy the extension ID shown there.
3. Register the development host for that ID. On macOS, run `node scripts/register-dev-host.mjs YOUR_EXTENSION_ID "$(pwd)/target/debug/rauser"` after replacing `YOUR_EXTENSION_ID`. On Windows, run the same script from PowerShell with your extension ID and the absolute path to `target\debug\rauser.exe`.
4. Open the Rauser side panel. Until setup is complete it warns that Rauser isn't set up; click the gear icon or **Open settings** to open the settings page in a tab. There, choose a notes folder, enter a site URL and path prefix, then enable the site. Chrome requests the site permission and the native host confirms the capture policy. The panel updates when settings change; it sends buffered visits, pauses capture, and creates a page note for the current permitted tab.

If **Choose folder** is disabled and the settings page says "Native host unavailable," check that the extension ID in the native-host registration matches the ID shown on `chrome://extensions`, rebuild the host at the registered path, then reload the settings page and reopen the panel. The development host registration is per user; the guided macOS smoke run below sets up its own isolated profile automatically.

For a guided macOS Chrome smoke run, build both components as above, then run:

```sh
npm run smoke:macos
```

The runner opens an isolated Chrome profile and a localhost fixture, loads `extension/dist/` through a local Chrome DevTools pipe, and verifies Chrome can exchange a `hello` with the native host before pausing for Chrome permissions, native dialogs, and panel actions. The debugging flag and pipe apply only to the disposable profile; no remote debugging port is opened. It checks the host configuration, visit log, page notes, review draft, and site removal after each checkpoint. The native host uses a temporary `HOME`, and its registration lives only in the isolated Chrome profile; the runner does not change the normal Chrome host registration. It preserves its profile and notes under the printed temporary directory for inspection. Run `node scripts/smoke-macos.mjs --help` for an alternate host or Chrome binary path.

The development registration in step 3 changes only the current user's Chrome native-host entry. It is for development; the signed installers planned for M5 will own installation and uninstallation. A folder path previously entered directly in an M0 config must be selected again through the native picker before M1 can use it. The host backs up malformed or incompatible config files during a revision-checked repair.

The host binds the chosen folder to its filesystem identity. If that folder is moved or replaced, choose it again before capture resumes. M1 keeps a synced visit-ID intent file in the OS config directory for each saved visit so retries can recover across log dates and interrupted appends. These files remain indefinitely in the development build; compacting the index is part of the public-release work.

Existing page notes are never overwritten. An unchanged create request returns the existing note; a new title or body produces a sibling review draft with a stable proposal ID, so retrying the same request does not create another draft. The panel shows the draft path. The host checks the original URL against its confirmed site rule before normalizing or logging it. See [protocol/README.md](protocol/README.md) for the message contract.

M1 still needs interactive Chrome checks on both macOS and Windows. Windows power-loss durability for newly created Markdown file names is not yet established. The public package remains a later release milestone.

## License

This repository is licensed under the [Apache License 2.0](LICENSE). The design specifies Apache-2.0 for the planned software as well.
