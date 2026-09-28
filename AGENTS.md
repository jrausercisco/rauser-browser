# Rauser Browser contributor instructions

## Current state

M0 provides the shared protocol, Rust native-host foundation, configuration validation, confined create-only vault writes, and CI. The extension, agent integration, search index, installers, and public release do not exist yet. Use [DESIGN.md](DESIGN.md) for the intended product behavior and update it when implementation changes a contract.

## Repository map

- `protocol/schema.json` defines native messages. Run `python3 protocol/generate.py` after changing it and commit the generated Rust and TypeScript files together.
- `host/` contains the native host. Keep host policy checks on the privileged side of the messaging boundary.
- `.github/workflows/ci.yml` runs build and quality checks on macOS and Windows.

## Local checks

Run the commands in [README.md](README.md#development) before proposing a code change. The Rust version is pinned in `rust-toolchain.toml`.

## Security rules

- Treat all extension messages and page-derived content as untrusted.
- Keep vault paths rooted in a user-selected directory; never accept a raw file path from a page or extension request.
- Preserve existing user files. The M0 vault API creates new files only; later edits require managed-block ownership and conflict detection.
- Keep capture disabled until site allowlisting and permission enforcement are implemented and verified.
- Do not claim an installer, browser extension, or agent mode works until it has been built and exercised.
