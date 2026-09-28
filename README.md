# Rauser Browser

Rauser is a local-first browser assistant designed to help people keep useful context from the pages they visit. The design describes an optional native host, a browser sidebar, Markdown notes, and local AI integrations.

## Status

This repository currently contains the product design only. Rauser is not implemented yet, and there are no installable packages. Do not download or run software from this repository until a signed release is published.

Read the [design specification](DESIGN.md) for the architecture, security model, planned features, and release plan.

## Planned support

- Google Chrome Stable on macOS 15 or later (Intel and Apple silicon)
- Google Chrome Stable on Windows 11 version 25H2 or later supported releases (x64)
- Edge and other browsers are not in the first-release support scope

The release plan calls for the browser extension to be distributed through the Chrome Web Store and a signed native-host installer to be provided for each supported operating system. See [Distribution](DESIGN.md#11-distribution) for details.

## License

Rauser Browser is planned to be released under the [Apache License 2.0](LICENSE).
