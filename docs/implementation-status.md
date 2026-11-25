# Wallora implementation status

Updated 2026-11-25. Task 4 landed in `main` at `edb885c`. [Execution plan](2026-11-18-wallora-plan-v2.md) · [Scope decisions](decisions.md) · [Architecture](architecture.md) · [Privacy inventory](privacy.md)

## Current milestones

| Milestone | Status |
| --- | --- |
| Task 1 · Foundation | Done, except the signed-in website audit and the Favorites mapping check (see Open work). Workspace, Iced shell, CI, and MSI packaging are implemented and verified on GitHub Actions. The API probe, guest website audit, spike results and parity-scope decision are recorded. |
| Task 2 · Provider client | Done. Search and wallpaper requests, pacing, retries, cooldown, cancellation, and API-key origin isolation are implemented. Encoded image downloads have byte and time limits. |
| Task 3 · Sessions and preferences | Done and independently reviewed. Includes sign-in, username verification, Windows Credential Manager, SQLite account state, account switching, revocation, and local proxy preferences. |
| Task 4 · Image pipeline and caches | Landed in `main` with the review fixes. `ImageGuard` and `CacheStore` are implemented, and `Cargo.lock` now pins `url` 2.5.9 and `form_urlencoded` 1.2.3, which clears RUSTSEC-2026-0592. The original commits `aecdb8a`, `a9b1654` and `b3f7564` remain as historical references. The app does not use the pipeline yet; wiring it in is Task 5. |

## Decisions since the last update (2026-11-23)

- **Toolkit:** Iced is mandatory. The Slint fallback is dropped. [Decision §4](decisions.md#4-gui-toolkit--iced-is-mandatory)
- **Hardware checks:** run only on the reference machine (GT 340, software `wgpu` fallback); results are valid for that machine only. [Decision §5](decisions.md#5-hardware-verification-scope)
- **Font:** Salted Flutters Polaris is under a custom permissive licence (not MIT) that allows bundling with stated conditions. [Decision §6](decisions.md#6-font-licensing--salted-flutters-polaris)

## Account behavior

- Sign-in checks the API key and username separately. The keyed settings and own-collection requests succeed. The supplied username's lookup returns 404, both public (2026-11-19) and keyed (2026-11-23), so its identity remains inconclusive. The app can show the keyed own-collection list for an unverified username. Private item access and Favorites mapping are Task 6a work. [API evidence](api-capabilities.md#favorites-mapping-follow-up-2026-11-23)
- Favorites mapping: the API lists one private collection labelled `Default` with 2 items, stable across two reads. The website label, empty state and id stability after a favorite change are not yet observed, so the mapping stays `unresolved`.
- An upstream 401 retires the active session and clears private views. If both SQLite and Credential Manager reject cleanup, account metadata and a key can survive restart. Private settings and collections stay hidden until a fresh response arrives for the current scope.

## Verification

- **Hosted CI for Task 4:** Windows CI passed on `main` at `edb885c`, including formatting, both Clippy and test lanes, `cargo deny`, the release build, and MSI verification and upload. [GitHub Actions run 45423820201](https://github.com/ppolkadot/Wallora/actions/runs/45423820201)
- **Hosted CI (historical, before Task 4 landed):** green on all three runs for `ef0466d` and its merge `3b782aa` (run `44552943473` on `main`). Every step passed: fmt, both Clippy lanes, both test lanes, `cargo deny`, release build, MSI build, the read-only MSI check, and artifact upload. The `wallora-msi` artifact (about 8.6 MB) was produced. [GitHub Actions](https://github.com/ppolkadot/Wallora/actions)
- **Local, 2026-11-23 (historical):** `cargo fmt --check` clean. The workspace test run with `--features wallora-core/test-util` passed 95 tests (app 8, core unit 32, `key_isolation` 9, `provider_contract` 13, `search_serialization` 3, `session` 27, windows 3 plus 1 ignored Credential Manager smoke test).
- **Task 4 branch (historical), 2026-11-24:** with `--features wallora-core/test-util`, all tests pass: app 8, core lib 35, `cache` 36, `image_guard` 9, `key_isolation` 9, `provider_contract` 13, `search_serialization` 3, `session` 27, windows 3 plus 1 ignored. These counts include the review fixes.
- At `5590726` the Windows Credential Manager smoke test passed with a synthetic key. A provider timeout test failed intermittently in an earlier revision's local run and passed on later runs; it is still worth watching.

## Open work

- **Signed-in Wallhaven walkthrough** (needs the owner's browser session): record the website's Favorites label and empty state, the real username, AI-art and Hot visibility, profile, settings and upload controls. The favorite add/remove stability test changes the account and needs explicit owner action. [Parity matrix](parity-matrix.md#outstanding-signed-in-checks)
- **Task 5 (gallery on Iced):** wire the cache in at app boot. `SessionManager::restore_with_cache(storage, credentials, cache_sink)` was added and the old `restore` still exists, but the app does not call `restore_with_cache` yet. No UI uses the cache yet, so the cache directory stays empty in released builds until this lands.
- **Task 5 cache shutdown:** the app must await `CacheStore::shutdown()` on window close. Otherwise the `cache.active` marker survives, and every launch discards the account caches.
- Task 7 owns interactive MSI install, upgrade and uninstall checks. They run on the reference machine only; Windows 10 stays unverified and must be documented as such.
- Bundling the font requires copying its `LICENSE.txt` next to it (see `THIRD_PARTY.md`). It is not bundled yet.
- Under owner-approved option A, Wallora 1.0 means *complete for supported routes*, with documented gaps and applicable `Open on wallhaven.cc` hand-offs. Phase B starts after the Task 7 release gate passes. [Owner decision](decisions.md#3-parity-scope-checkpoint--task-1e)
