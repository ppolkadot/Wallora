# Wallora

*An unofficial native Wallhaven client for Windows.*

Wallora is an independent, open-source desktop client written in Rust. It is **not affiliated with, endorsed by, or sponsored by Wallhaven**. "Wallhaven" and `wallhaven.cc` belong to their respective owners. Wallora uses only the public, documented Wallhaven API and follows its published rate limit.

## Status: pre-release (0.1.0 beta)

Wallora is under active development and is **pre-release**. The native shell, the Wallhaven API client, account sign-in, the guarded image pipeline and scoped cache, the discovery/search gallery, wallpaper details and original downloads, read-only collections, and profiles/tags are built. The v1.0 release gate (manual parity walkthrough, installer checks, signing) and Windows 10 verification are still open.

| Area | State |
| --- | --- |
| Windows shell (Iced), navigation, settings, MSI packaging | Built |
| Provider client: search and wallpaper requests, pacing, retries, API-key isolation | Built |
| Accounts: API-key sign-in, Windows Credential Manager, account switching, local proxy setting | Built |
| Image pipeline and scoped thumbnail cache | Built and opened at app boot, with a persisted cap |
| Discovery and search gallery, wallpaper details, original downloads | Built |
| Collections (read-only), profiles, tags, API-gap browser hand-offs | Built |
| Wallpaper control, history, adaptive themes, rotation, tray | Planned, after 1.0 |

Wallora 1.0 means *complete for supported routes*: Wallhaven's public API is read-only, so actions it cannot do (favoriting, collection edits, uploads, comments, forums) are documented gaps and open on `wallhaven.cc` in your browser through a labelled **Open on wallhaven.cc** hand-off. See [the parity matrix](docs/parity-matrix.md) and [scope decisions](docs/decisions.md).

## Requirements

- Windows 10 or 11 (64-bit) are the target platforms. The current app has been smoke-tested on a Windows 11 development PC. Windows 10 and the reference-PC release checks are pending.
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

Windows pre-release installers are distributed through [GitHub Releases](https://github.com/ppolkadot/Wallora/releases). The installers are **unsigned** and may trigger a Windows SmartScreen warning. Wallora has no automatic updates.

## Privacy

No telemetry. Your API key lives only in Windows Credential Manager. The full list of hosts contacted and files written is in [docs/privacy.md](docs/privacy.md).

## Documentation

- [Architecture](docs/architecture.md)
- [Implementation plan (v2)](docs/2026-11-18-wallora-plan-v2.md) · [original requirements](docs/2026-11-17-wallhaven-desktop.md)
- [Implementation status](docs/implementation-status.md) · [Release gate evidence](docs/release-gate-evidence.md) · [v1.0 readiness plan](docs/superpowers/plans/2026-11-25-wallora-v1-release-readiness.md)
- [Scope decisions](docs/decisions.md) · [API capabilities](docs/api-capabilities.md) · [Parity matrix](docs/parity-matrix.md)
- [Privacy](docs/privacy.md) · [Third-party licences](THIRD_PARTY.md) · [Archive](docs/archive/README.md)

## Licence

Wallora is released under the [MIT License](LICENSE). Third-party components, including the Salted Flutters Polaris font, keep their own licences; see [THIRD_PARTY.md](THIRD_PARTY.md).
