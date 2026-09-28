# Rauser Browser contributor instructions

## Current state

M0 is merged. M1 development code adds a Chrome side panel and settings page, opt-in site capture, native folder selection and consent, durable visit IDs, and create-only page notes with sibling review drafts. The agent integration, search index, signed installers, Chrome Web Store listing, and public release do not exist yet. M1 still needs interactive Chrome checks on macOS and Windows; Windows new-file power-loss durability is not established. Use [DESIGN.md](DESIGN.md) for intended behavior and update it when implementation changes a contract.

## Repository map

- `protocol/schema.json` defines native messages. Run `python3 protocol/generate.py` after changing it and commit the generated Rust and TypeScript files together.
- `host/` contains the native host. Keep host policy checks on the privileged side of the messaging boundary.
- `extension/` contains the MV3 Chrome development build. `npm run build:extension` emits `extension/dist/` for unpacked loading.
- `scripts/register-dev-host.mjs` registers a per-user development native host for one unpacked extension ID; installers own release registration.
- `.github/workflows/ci.yml` runs build and quality checks on macOS and Windows.

## Local checks

Run the commands in [README.md](README.md#development) before proposing a code change. The Rust version is pinned in `rust-toolchain.toml`.

## Security rules

- Treat all extension messages and page-derived content as untrusted.
- Keep vault paths rooted in a user-selected directory; never accept a raw file path from a page or extension request.
- Preserve existing user files. The M0 vault API creates new files only; later edits require managed-block ownership and conflict detection.
- Keep capture off until a folder is chosen, Chrome grants the requested site origin, and the native host confirms the policy. An M0 config with a manually set root needs folder reselection through the native picker before M1 can use it.
- Do not claim an installer, browser extension, or agent mode works until it has been built and exercised.
