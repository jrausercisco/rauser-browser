# Rauser Browser

Rauser is a local-first browser assistant designed to help people keep useful context from the pages they visit. The design describes an optional native host, a browser sidebar, Markdown notes, and local AI integrations.

## Status

The M0 foundation is implemented: the repository contains a versioned native-messaging protocol, a Rust host foundation, and CI for macOS and Windows. There is no browser extension or installable release yet.

Read the [design specification](DESIGN.md) for the architecture, security model, planned features, and release plan.

## Planned support

- Google Chrome Stable on macOS 15 or later (Intel and Apple silicon)
- Google Chrome Stable on Windows 11 version 25H2 or later supported releases (x64)
- Edge and other browsers are not in the first-release support scope

The release plan calls for the browser extension to be distributed through the Chrome Web Store and a signed native-host installer to be provided for each supported operating system. See [Distribution](DESIGN.md#11-distribution) for details.

## Development

The pinned Rust toolchain is in `rust-toolchain.toml`. Python 3 generates protocol types, and Node.js runs the TypeScript checks. From the repository root:

```sh
python3 protocol/generate.py --check
cargo build --locked --workspace --all-targets
cargo test --locked --workspace
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
npm ci --ignore-scripts
npm run typecheck
npm run lint
```

The current host protocol supports discovery and configuration messages. Capture stays disabled until the site permission and allowlist rules are implemented. The vault module currently creates new page files only and refuses to replace an existing file. See [protocol/README.md](protocol/README.md) for the message contract.

## License

This repository is licensed under the [Apache License 2.0](LICENSE). The design specifies Apache-2.0 for the planned software as well.
