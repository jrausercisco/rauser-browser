# Rauser Browser

Rauser is a local-first browser assistant designed to help people keep useful context from the pages they visit. The design describes an optional native host, a browser sidebar, Markdown notes, and local AI integrations.

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
```

### Try the M1 development build

1. Build the native host with `cargo build --locked -p rauser` and the extension with `npm ci --ignore-scripts && npm run build:extension`.
2. In Google Chrome, open `chrome://extensions`, enable Developer mode, and load `extension/dist/` as an unpacked extension. Copy the extension ID shown there.
3. Register the development host for that ID. On macOS, run `node scripts/register-dev-host.mjs YOUR_EXTENSION_ID "$(pwd)/target/debug/rauser"` after replacing `YOUR_EXTENSION_ID`. On Windows, run the same script from PowerShell with your extension ID and the absolute path to `target\debug\rauser.exe`.
4. Open the Rauser side panel, choose a notes folder, enter a site URL and path prefix, then enable the site. Chrome requests the site permission and the native host confirms the capture policy. The panel can send buffered visits and create a page note for the current permitted tab.

This registration changes only the current user's Chrome native-host entry. It is for development; the signed installers planned for M5 will own installation and uninstallation. A folder path previously entered directly in an M0 config must be selected again through the native picker before M1 can use it. The host backs up malformed or incompatible config files during a revision-checked repair.

Existing page notes are never overwritten. An unchanged create request returns the existing note; a new title or body produces a sibling review draft with a stable proposal ID, so retrying the same request does not create another draft. The panel shows the draft path. The host checks the original URL against its confirmed site rule before normalizing or logging it. See [protocol/README.md](protocol/README.md) for the message contract.

M1 still needs interactive Chrome checks on both macOS and Windows. Windows power-loss durability for newly created Markdown file names is not yet established. The public package remains a later release milestone.

## License

This repository is licensed under the [Apache License 2.0](LICENSE). The design specifies Apache-2.0 for the planned software as well.
