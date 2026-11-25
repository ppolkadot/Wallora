# Wallora implementation status

Updated 2026-11-25. Tasks 5–6c landed in `main` at `7a33d3d`, on top of Task 4 at `edb885c`. [Execution plan](2026-11-18-wallora-plan-v2.md) · [Scope decisions](decisions.md) · [Architecture](architecture.md) · [Privacy inventory](privacy.md)

## Current milestones

| Milestone | Status |
| --- | --- |
| Task 1 · Foundation | Done, except the signed-in website audit and the Favorites mapping check (see Open work). Workspace, Iced shell, CI, and MSI packaging are implemented and verified on GitHub Actions. The API probe, guest website audit, spike results and parity-scope decision are recorded. |
| Task 2 · Provider client | Done. Search and wallpaper requests, pacing, retries, cooldown, cancellation, and API-key origin isolation are implemented. Encoded image downloads have byte and time limits. |
| Task 3 · Sessions and preferences | Done and independently reviewed. Includes sign-in, username verification, Windows Credential Manager, SQLite account state, account switching, revocation, and local proxy preferences. |
| Task 4 · Image pipeline and caches | Landed in `main` with the review fixes. `ImageGuard` and `CacheStore` are implemented, and `Cargo.lock` now pins `url` 2.5.9 and `form_urlencoded` 1.2.3, which clears RUSTSEC-2026-0592. The original commits `aecdb8a`, `a9b1654` and `b3f7564` remain as historical references. The app now opens the cache at boot and awaits `CacheStore::shutdown()` on window close. |
| Task 5 · Discovery and search | Implemented in `09edb43` and `2802703`. The typed query builder, discovery controller, image pipeline, Iced virtualized gallery, Latest/Toplist/Random/Search views, filters, keyboard navigation, scroll-away thumbnail cancellation, and the offline stale badge are in place. Hot and the AI-art filter remain documented gaps with browser hand-offs. |
| Task 6 · Wallpaper details and original downloads | Implemented in `24912ad`. Detail metadata, `like:` similar search, tag navigation, the filename sanitizer, streaming original downloads, no-clobber `MoveFileExW` publication, the `rfd` native Save dialog, and copy-link actions are in place. |
| Task 6a · Collections (read) | Implemented in `6679674`. Own and public collections, paginated items, the `verified`-only own-item gate, the unresolved per-`AccountId` Favorites mapping, and disabled write actions with `Open on wallhaven.cc` hand-offs are in place. |
| Task 6b · Profiles and tags | Implemented in `b093b21`. Tag metadata and `id:N` pages, `@user` uploader pages, clickable tags/uploaders from Detail, and a back-navigation stack are in place. |
| Task 6c · Remaining matrix rows | Implemented in `7a33d3d`. Every parity row has a disposition; unsupported rows use labelled `Open on wallhaven.cc` hand-offs, including the Home "More on wallhaven.cc (API gaps)" card. Automated hand-off and search-syntax tests are in place. |
| Task 7 · v1.0 release gate | **Open.** Automated gates pass on `a6a3301` locally (below), but the manual row-by-row parity walkthrough, DPI, keyboard, recovery and MSI install/upgrade/uninstall checks have not been done on the reference machine. Windows 10 is unverified. Defensible label today: **beta / partial**. See the [evidence ledger](release-gate-evidence.md). |
| Task 1c · Signed-in Wallhaven walkthrough | **Open, blocked.** Guest checks re-confirmed 2026-11-25. No owner-provided authenticated session was available; see Open work and the [evidence ledger](release-gate-evidence.md). |

## Decisions since the last update (2026-11-23)

- **Toolkit:** Iced is mandatory. The Slint fallback is dropped. [Decision §4](decisions.md#4-gui-toolkit--iced-is-mandatory)
- **Hardware checks:** run only on the reference machine (GT 340, software `wgpu` fallback); results are valid for that machine only. [Decision §5](decisions.md#5-hardware-verification-scope)
- **Font:** Salted Flutters Polaris is under a custom permissive licence (not MIT) that allows bundling with stated conditions. [Decision §6](decisions.md#6-font-licensing--salted-flutters-polaris)

## Account behavior

- Sign-in checks the API key and username separately. The keyed settings and own-collection requests succeed. The supplied username's lookup returns 404, both public (2026-11-19) and keyed (2026-11-23), so its identity remains inconclusive. The app can show the keyed own-collection list for an unverified username, but own collection *items* stay gated until the username is `verified`. [API evidence](api-capabilities.md#favorites-mapping-follow-up-2026-11-23)
- Favorites mapping: the API lists one private collection labelled `Default` with 2 items, stable across two reads. The website label, empty state and id stability after a favorite change are not yet observed, so the mapping stays `unresolved` and no collection is labelled "Favorites".
- An upstream 401 retires the active session and clears private views. If both SQLite and Credential Manager reject cleanup, account metadata and a key can survive restart. Private settings and collections stay hidden until a fresh response arrives for the current scope.

## Verification

- **Local, 2026-11-25 at `7a33d3d` (default lane):** `cargo test --locked --workspace --all-targets` passed 171 tests, 0 failed, 1 ignored. Breakdown: app 52; core lib 55; `cache_cap` 6; `collections` 7; `discovery` 14; `image_guard` 9; `image_pipeline` 18; `profiles_tags` 4; windows 6 plus 1 ignored Credential Manager smoke test.
- **Local, 2026-11-25 at `7a33d3d` (`test-util` lane):** `cargo test --locked --workspace --all-targets --features wallora-core/test-util` passed 281 tests, 0 failed, 1 ignored. It adds `cache` 36, `downloads` 16, `image_pipeline_loopback` 2, `key_isolation` 9, `provider_contract` 16, `search_serialization` 4 and `session` 27.
- **Local, 2026-11-25 at `a6a3301`:** `cargo fmt --check`, both Clippy lanes and the release build pass, and both test lanes reproduce the counts above (171 and 281). This ran on a machine that is **not** the reference PC; see the [evidence ledger](release-gate-evidence.md#hardware-scope).
- **Dependency policy, 2026-11-25 at `a6a3301`:** `cargo deny --locked check` (0.21.0) passed locally: advisories, bans, licenses and sources ok.
- **MSI, 2026-11-25 at `a6a3301`:** a local `cargo wix` build produced a 10.2 MB MSI that passes the CI static checks (`exe0` present, no `Environment` table). Its payload exe is byte-identical to the release build. MSI and exe are unsigned. No install, upgrade or uninstall was run.
- **Hosted CI for the pre-release:** the published tag and MSI build run are recorded on [GitHub Releases](https://github.com/ppolkadot/Wallora/releases).
- **Hosted CI for Task 4:** Windows CI passed on `main` at `edb885c`, including formatting, both Clippy and test lanes, `cargo deny`, the release build, and MSI verification and upload. [GitHub Actions run 45423820201](https://github.com/ppolkadot/Wallora/actions/runs/45423820201)
- **Hosted CI (historical, before Task 4 landed):** green on all three runs for `ef0466d` and its merge `3b782aa` (run `44552943473` on `main`). Every step passed: fmt, both Clippy lanes, both test lanes, `cargo deny`, release build, MSI build, the read-only MSI check, and artifact upload. The `wallora-msi` artifact (about 8.6 MB) was produced. [GitHub Actions](https://github.com/ppolkadot/Wallora/actions)
- **Local, 2026-11-23 (historical):** `cargo fmt --check` clean. The workspace test run with `--features wallora-core/test-util` passed 95 tests (app 8, core unit 32, `key_isolation` 9, `provider_contract` 13, `search_serialization` 3, `session` 27, windows 3 plus 1 ignored Credential Manager smoke test).
- **Task 4 branch (historical), 2026-11-24:** with `--features wallora-core/test-util`, all tests pass: app 8, core lib 35, `cache` 36, `image_guard` 9, `key_isolation` 9, `provider_contract` 13, `search_serialization` 3, `session` 27, windows 3 plus 1 ignored. These counts include the review fixes.
- At `5590726` the Windows Credential Manager smoke test passed with a synthetic key. A provider timeout test failed intermittently in an earlier revision's local run and passed on later runs; it is still worth watching.

## Open work

- **Signed-in Wallhaven walkthrough** (needs the owner's browser session; still blocked as of 2026-11-25): record the website's Favorites label and empty state, the real username, AI-art and Hot visibility, profile, settings and upload controls. The favorite add/remove stability test changes the account and needs explicit owner action. [Parity matrix](parity-matrix.md#outstanding-signed-in-checks)
- **Task 7 (v1.0 release gate):** the full row-by-row manual parity walkthrough, guest and account, at DPI 100/150/200% with a keyboard-only run, plus connectivity-loss and rate-limit recovery; interactive MSI install, upgrade and uninstall checks; signing status; and the final release label. These are not complete. Windows 10 stays unverified and must be documented as such. Owner actions needed are listed in the [evidence ledger](release-gate-evidence.md#owner-actions-needed). One byte-equivalence sample (SFW `3l5xev`) passed on 2026-11-25.
- Bundling the font requires copying its `LICENSE.txt` next to it (see `THIRD_PARTY.md`). It is not bundled yet.
- Under owner-approved option A, Wallora 1.0 means *complete for supported routes*, with documented gaps and applicable `Open on wallhaven.cc` hand-offs. Phase B starts after the Task 7 release gate passes. [Owner decision](decisions.md#3-parity-scope-checkpoint--task-1e)
