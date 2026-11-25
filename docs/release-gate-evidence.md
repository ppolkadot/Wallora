# Release gate evidence ledger (Task 1c and Task 7)

Recorded 2026-11-25 against candidate `a6a3301a8d052a85e8191e79ccf31569f6127540` (local `main` at the time of the run, eight commits ahead of `origin/main` at `6462fb1`). This ledger records what was run and what it showed. A gate is marked **Passed** only where evidence exists; everything else is **Open** or **Blocked**, with the owner action needed. Related: [status](implementation-status.md) · [parity matrix](parity-matrix.md) · [plan, Task 7](2026-11-18-wallora-plan-v2.md) · [decisions §5](decisions.md#5-hardware-verification-scope)

## Verdict

**Release label: beta / partial (pre-release).** The label `1.0 — complete for supported routes` is **not** defensible yet. Under owner-approved option A it needs the manual row-by-row parity walkthrough, the reference-machine DPI, keyboard and recovery runs, and the MSI install, upgrade and uninstall checks. None of these have been done on the reference machine. Phase B stays blocked. The v1.0 tag remains gated. A beta can be tagged and published separately.

## Hardware scope

| | Documented reference machine ([decisions §2, §5](decisions.md#2-gui-toolkit-spike-spikesiced-gallery--task-1d)) | Machine used for this run |
| --- | --- | --- |
| CPU | Intel i5-12400F | AMD Ryzen 7 5700X |
| GPU | NVIDIA GeForce GT 340 (software `wgpu` fallback) | NVIDIA GeForce RTX 3060 Ti |
| Display | 1440×900 | 1920×1080 ×2 |
| OS | Windows 11 Pro 10.0.26200 | Windows 11 Pro 10.0.26300 |

**This run was not on the reference machine.** Per decision §5, hardware-dependent results from it are not valid release evidence. It was used only for hardware-independent automated checks and for non-installing smoke checks, labelled below. Windows 10 coverage is **unverified**; no Windows 10 machine is available (decision §5).

## Automated gates

| Gate | Command | Outcome |
| --- | --- | --- |
| Formatting | `cargo fmt --all -- --check` | Pass |
| Clippy, default lane | `cargo clippy --locked --workspace --all-targets -- -D warnings` | Pass |
| Tests, default lane | `cargo test --locked --workspace --all-targets` | Pass: 171 passed, 0 failed, 1 ignored (Credential Manager smoke) |
| Clippy, `test-util` lane | `cargo clippy --locked --workspace --all-targets --features wallora-core/test-util -- -D warnings` | Pass |
| Tests, `test-util` lane | `cargo test --locked --workspace --all-targets --features wallora-core/test-util` | Pass: 281 passed, 0 failed, 1 ignored |
| Release build | `cargo build --locked --release` | Pass |
| Dependency policy | `cargo deny --locked check` (0.21.0, CI's pinned version, installed to a scratch root) | Pass: advisories, bans, licenses and sources ok. First local run; before this it ran only in CI. |
| MSI build | `cargo wix --package wallora-app --no-build --nocapture --bin-path <WiX 3.14.1>` (cargo-wix 0.3.9; WiX zip SHA-256 matched the CI pin) | Pass: `wallora-app-0.1.0-x86_64.msi`, 10,174,464 bytes, SHA-256 `E0CFA2C0C829C2559673A5B48B445E8D979F4136725C0EAEC69F96270451EEC0` |
| MSI static checks (same as CI) | Windows Installer COM queries | Pass: `exe0` present, no `Environment` table (no PATH change). Also observed: `ALLUSERS=1` (per-machine, needs elevation), `MajorUpgrade` rows for upgrade code `{929EBA26-…}`, `ProductVersion` 0.1.0, no `Shortcut` table (no Start Menu entry), both MSI and exe `NotSigned`. |
| MSI content | `msiexec /a` administrative extract to scratch (installs nothing) | Pass: `bin\wallora-app.exe` is byte-identical to `target\release\wallora-app.exe`; `License.rtf` is the project's MIT text. |
| Hosted CI, exact commit | none | **Pending at the time of this run.** See [GitHub Releases](https://github.com/ppolkadot/Wallora/releases) for the pre-release tag and its build run. |

Automated tests for the recovery behaviours exist, but they are not the manual runs the gate asks for: 429 cooldown with and without `Retry-After` and prefetch drop (`tests/provider_contract.rs`), offline stale thumbnails (`tests/image_pipeline.rs::offline_cache_hits_are_reported_stale_without_a_fetch`), offline scope visibility (`tests/cache.rs`), and keyboard grid and collections navigation (`wallora-app::app::tests`).

## Follow-up guest smoke at `a6a3301`

The production client returned SFW results for Latest, Toplist, Random, search, tag and uploader browsing, and public collections. A scratch run decoded a thumbnail and preview, then verified the original for 3l5xev with the Windows publisher. Its 1,176,033 bytes matched an independent CDN download. More like this returned HTTP 403 twice. The GUI Save dialog, signed-in flows and manual parity remain open.

## Manual and smoke checks

| Check | Result | Scope |
| --- | --- | --- |
| Download-byte equivalence | **Passed for one sample.** SFW wallpaper `3l5xev`, guest, no key. The website's `#wallpaper` source URL equals the API `data.path`. The native core `OriginalDownloader` saved 1,176,033 bytes (equal to API `file_size`); an independent `curl` of the same URL returned the same size and an identical SHA-256 (`dd658a72…50ec`). | Core download path with the portable `HardLinkFilePublisher`, not the GUI Save dialog or the `MoveFileExW` publisher. Not reference hardware (not hardware dependent). One sample only. |
| App launch smoke | Window opened (`Wallora — Home (Unofficial Wallhaven Client)`) from the extracted MSI exe, loaded guest thumbnails, no crash over 15 s, about 233 MB working set, one benign `fontdb` malformed-system-font warning. | Not reference machine. Not an install. **Side effect:** an attempt to redirect `APPDATA`/`LOCALAPPDATA` did not take effect (`directories` resolves Known Folders), so the app created `%APPDATA%\Wallora` and `%LOCALAPPDATA%\Wallora`. Neither existed before the run (checked), and every file was created during it. I verified that and deleted both directories. No pre-existing app data, credential, system setting or installation was touched. No sign-in occurred, so Credential Manager was not used. |
| Third-party inventory | `cargo deny list` returned 369 crates for the Windows target. The review found matching licence expressions and one extra macOS-only row in `THIRD_PARTY.md`, `objc2-uniform-type-identifiers` 0.3.2. The inventory now lists the 369 Windows entries. No font is bundled. | Hardware independent. |
| Signing and SmartScreen | No certificate; MSI and exe are `NotSigned`. `README.md` already states "unsigned" and that SmartScreen may warn. | Documentation reviewed; no change needed. |
| Guest row-by-row parity walkthrough | **Open.** Not performed. | |
| Account row-by-row parity walkthrough | **Blocked** (needs the owner's authenticated session). | |
| DPI 100%, 150%, 200% | **Blocked.** Needs the reference machine; changing display scaling is a system-setting change. Manifest declares PerMonitorV2 and `longPathAware` (static check only). | |
| Keyboard-only run | **Open.** Automated keyboard tests exist; manual run not done. | |
| Offline and 429 manual recovery | **Open.** Automated coverage above; manual run not done. | |
| MSI install, upgrade, uninstall | **Blocked.** Needs the reference machine, elevation and authorization, and for upgrade a previous-version MSI (the workspace is still `0.1.0`, so no distinct earlier build exists). | |
| Windows 10 | **Unverified** under decision §5. | |

## Task 1c (signed-in website evidence)

| Item | Result |
| --- | --- |
| Surface | Preview browser, Incognito (guest) profile. The Default profile (the owner's) was not opened. |
| Guest observations, 2026-11-25 | `https://wallhaven.cc/` loads for a guest. `/login` passed a Cloudflare managed challenge automatically and shows `Username or Email`, `Password`, `Login`, `Forgot password?` and `Need an account?`. `/settings/account` redirects a guest to `/login`. Guest top navigation links Latest, Hot, Toplist, Random, Upload and Forums, plus Register and Login. A wallpaper page's original URL equals the API `data.path` (see the byte check). |
| Signed-in walkthrough | **Blocked.** No owner-provided authenticated session exists. The preview tab is not visible to a human in this environment, so a live sign-in handoff was not possible. No credential was entered, retrieved or captured. |
| Not observed | Real username and the username verification flow; owned collections list, the Favorites label and its empty state; AI-art and Hot visibility when signed in; profile, settings, upload and tagging/moderation controls; comment and forum posting permissions. No purity-restricted content was viewed. |
| Favorites mapping | Still **unresolved**. The favorite add/remove and restoration test changes the account and needs explicit owner action. It was not attempted. |
| Matrix | No row disposition changed. Website observations do not authorize new API routes. |

## Owner actions needed

1. **Signed-in session (Task 1c and the account half of Task 7):** an owner-provided signed-in test session in a browser tab a human can drive, because of the Cloudflare check and login. Separately, explicit authorization for the favorite add/remove and restoration, done by the owner, to settle the Favorites mapping.
2. **Reference machine (Task 7):** a session on the documented reference PC (i5-12400F, GT 340) with authorization for MSI install, upgrade and uninstall (per-machine, elevated) and for the DPI changes. Upgrade also needs a version-bumped build and an earlier MSI.
3. **Hosted CI:** record the successful run for the eventual v1.0 release commit.
4. **Tagging:** explicit authorization, only after all gates pass.
5. **Windows 10:** a machine, or acceptance that 1.0 ships with Windows 10 documented as unverified.
6. **Signing:** a certificate, or acceptance of an unsigned release.
