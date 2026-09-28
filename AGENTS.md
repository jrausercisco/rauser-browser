# Rauser Browser Browsing Assistant contributor instructions

## Current state

M0 is merged. M1 development code adds a Chrome side panel and settings page, opt-in site capture, native folder selection and consent, and durable visit IDs. The side panel was then reworked, before M2 (DESIGN.md §12.2 step 4): page notes are no longer create-only with sibling review drafts. The panel follows the active tab, loads that page's note on any HTTP(S) page (not gated by the logging allowlist), and autosaves the whole note about a second after typing stops, on tab change, and on panel close, via new `load_note`/`save_note` protocol messages (protocol version 3) that reuse `get_config`/`update_config`'s revision-check pattern. A save based on a stale version is refused; the host returns the note's current content and the panel shows a copy-out of the text it could not save. The first note on a page the extension has no standing Chrome access to starts from the toolbar action, the `note` keyboard command, or a page context-menu entry, which brings `activeTab` and `contextMenus` forward from M2 (and raises `minimum_chrome_version` to 116). The capture housekeeping controls from M1 collapsed into a one-line, expandable capture strip. The answer card, Summary section, and full omnibar are not built (M2/M3). The agent integration, search index, signed installers, Chrome Web Store listing, and public release do not exist yet. M1 still needs interactive Chrome checks on macOS and Windows; Windows new-file power-loss durability is not established. Use [DESIGN.md](DESIGN.md) for intended behavior and update it when implementation changes a contract.

## Repository map

- `protocol/schema.json` defines native messages. Run `python3 protocol/generate.py` after changing it and commit the generated Rust and TypeScript files together.
- `host/` contains the native host. Keep host policy checks on the privileged side of the messaging boundary.
- `macos-alert/` is the host's only unsafe code: a small macOS-only wrapper around `CFUserNotification`, so the Yes/No confirmation can time out and be canceled when the host goes away. The alert is drawn by `UserNotificationCenter`, so killing the dialog process does not close it. Keep new unsafe code there, with a `SAFETY` comment per block; every other crate stays `#![forbid(unsafe_code)]`.
- `extension/` contains the MV3 Chrome development build. `npm run build:extension` emits `extension/dist/` for unpacked loading.
- Product naming (app name, native host ID, storage key, marker, and temp-file prefixes) lives in `extension/brand.ts` (also used by the build and dev scripts) and `host/src/brand.rs`. Page and manifest text use `{{APP_NAME}}`/`{{FULL_NAME}}`, filled in by `extension/build.mjs`. Crate, package, and binary names in `Cargo.toml` and `package.json` still change separately.
- `scripts/register-dev-host.mjs` registers a per-user development native host for one unpacked extension ID; installers own release registration.
- `.github/workflows/ci.yml` runs build and quality checks on macOS and Windows.

## Local checks

Run the commands in [README.md](README.md#development) before proposing a code change. The Rust version is pinned in `rust-toolchain.toml`.

For end-to-end checks, use `npm run smoke:macos:headless`. It never shows a window or takes focus. Run `--auto` (real dialogs, UI scripting) only in a VM or separate session, never on the user's desktop. The same applies to `npm run check:macos-alert` (the real alert's cancel and timeout paths), which refuses to run outside a VM without `--on-desktop`. The general strategy, including what headless cannot prove, is in the ENDURANCE workspace's `TESTING.md` (`/Users/jrauser/ENDURANCE/TESTING.md`); update it with anything you learn here.

## Security rules

- Treat all extension messages and page-derived content as untrusted.
- Keep vault paths rooted in a user-selected directory; never accept a raw file path from a page or extension request.
- Preserve existing user files. Page notes are the one file kind the host replaces whole, and only after checking the caller's version and the file's recorded canonical URL and Brauser ownership (`host/src/note.rs`); an unrelated or unowned file at that path is a conflict, never adopted or overwritten. Every other content location (log, later) stays create-only.
- Keep capture off until a folder is chosen, Chrome grants the requested site origin, and the native host confirms the policy. An M0 config with a manually set root needs folder reselection through the native picker before M1 can use it.
- Do not claim an installer, browser extension, or agent mode works until it has been built and exercised.
