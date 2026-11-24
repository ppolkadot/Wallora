# Wallora

*An unofficial native Wallhaven client for Windows.*

Wallora is an independent, open-source desktop client written in Rust. It is **not affiliated with, endorsed by, or sponsored by Wallhaven**. "Wallhaven" and `wallhaven.cc` belong to their respective owners. Wallora uses only the public, documented Wallhaven API and follows its published rate limit.

## Status: pre-release (v0.1.x)

Wallora is under active development and is **not usable as a wallpaper browser yet**. The native shell, the Wallhaven API client, and account sign-in are built. Browsing, the image pipeline, downloads and wallpaper control are still to come.

| Area | State |
| --- | --- |
| Windows shell (Iced), navigation, settings, MSI packaging | Built |
| Provider client: search and wallpaper requests, pacing, retries, API-key isolation | Built |
| Accounts: API-key sign-in, Windows Credential Manager, account switching, local proxy setting | Built |
| Image pipeline and scoped thumbnail cache | Implemented on branch `t3/image-cache-pipeline`, in review, not merged |
| Discovery and search UI, wallpaper details, original downloads | Planned |
| Collections (read-only), profiles, tags | Planned |
| Wallpaper control, history, adaptive themes, rotation, tray | Planned, after 1.0 |

Wallora 1.0 means *complete for supported routes*: Wallhaven's public API is read-only, so actions it cannot do (favoriting, collection edits, uploads, comments, forums) are documented gaps and open on `wallhaven.cc` in your browser through a labelled **Open on wallhaven.cc** hand-off. See [the parity matrix](docs/parity-matrix.md) and [scope decisions](docs/decisions.md).

## Requirements

- Windows 10 or 11 (64-bit). Only Windows 11 on one reference machine has been tested so far.
- To build: Rust `1.99.0` (pinned by `rust-toolchain.toml`) with the MSVC toolchain.

## Build and test

```powershell
cargo build --locked --release              # builds target\release\wallora-app.exe
cargo test --locked --workspace --all-targets --features wallora-core/test-util
cargo clippy --locked --workspace --all-targets --features wallora-core/test-util -- -D warnings
cargo deny --locked check
```

CI runs the same checks on `windows-latest` and builds an MSI with `cargo-wix` (artifact `wallora-msi`).

## Downloads and signing

Builds and installers are **unsigned**. Windows SmartScreen may warn when you run an unsigned installer; that is expected for now. There are no published releases yet and no automatic updates.

## Privacy

No telemetry. Your API key lives only in Windows Credential Manager. The full list of hosts contacted and files written is in [docs/privacy.md](docs/privacy.md).

## Documentation

- [Architecture](docs/architecture.md)
- [Implementation plan (v2)](docs/2026-11-18-wallora-plan-v2.md) · [original requirements](docs/2026-11-17-wallhaven-desktop.md)
- [Implementation status](docs/implementation-status.md)
- [Scope decisions](docs/decisions.md) · [API capabilities](docs/api-capabilities.md) · [Parity matrix](docs/parity-matrix.md)
- [Privacy](docs/privacy.md) · [Third-party licences](THIRD_PARTY.md) · [Archive](docs/archive/README.md)

## Licence

Wallora is released under the [MIT License](LICENSE). Third-party components, including the Salted Flutters Polaris font, keep their own licences; see [THIRD_PARTY.md](THIRD_PARTY.md).
