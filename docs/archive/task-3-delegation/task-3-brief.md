### Task 3: Sessions and preferences (v0.2)

Account collection item access and Favorites mapping are Task 6a evidence. Task 3 can develop and test sessions against fixtures when a test account's username cannot be verified live. Finish the Task 1 website audit and CI gate before starting Task 3. The owner has deferred the hardware-dependent toolkit decision; retain the provisional Iced shell and finish the spike before committing to the Task 5 gallery toolkit.

- [ ] Add `Provider::user_settings` and `Provider::own_collections` (list only; items come in 6a).
- [ ] Sign-in form takes **API key + username**. Two independent results:
  - **Key validity**: `/settings` with the key → 200 = valid; 401 = invalid (sign-in refused); other = retry later.
  - **Username verification** (only for a valid key): compare `/collections` (keyed, own) with `/collections/{username}` (unkeyed, public view):
    - public ids ⊆ own ids and at least one id in common → **verified**;
    - any public id not in a successfully decoded own list, or a 404 that reliably identifies an unknown username → **mismatch** (sign-in refused with "username does not match this API key");
    - own list contains only private collections (public view empty) → **unverifiable-private**;
    - own list empty → **unverifiable-empty**;
    - either list fails or is malformed, the public list is empty despite public entries in the own list, or a generic 404 has an ambiguous meaning → **inconclusive** (retry later; treated as unverified until resolved). A 404 for the supplied username with an all-private own list was observed and does not by itself prove a mismatch.
  - Unverifiable and inconclusive states are allowed and shown in Account. The authenticated own-collection **list** (names, counts) stays available; own collection **items** and the Favorites label require `verified` (or an authorized account-bound route recorded in the matrix).
- [ ] Tests: each identity outcome (verified, mismatch, unverifiable-private, unverifiable-empty, inconclusive) tested separately, each with a valid key; key-invalid tested separately with no identity check attempted.
- [ ] On success create `AccountId` (UUID), save key via `CredentialStore`, store username + verification state + AccountId in SQLite.
- [ ] `wallora-windows::credentials` implements `CredentialStore` with `keyring` (`windows-native`); manual smoke test against real Credential Manager (write, read, delete).
- [ ] Tests: valid/invalid key; key revoked mid-session (401 → scope generation bump, account caches purged, user told); logout; account switch; guest requesting NSFW (upstream refuses → show upstream's restriction, add none of our own).
- [ ] Apply supported upstream defaults (purity/categories, page size and other documented values with an app equivalent). For upstream settings edits, provide the labelled `Open on wallhaven.cc` hand-off. Keep local proxy preferences separate from upstream settings.
- [ ] Logout / switch: bump generation, clear account-scoped in-memory views and any persisted account cache introduced in Task 3; keep user downloads and applied wallpapers. Integrate the image-cache purge lifecycle when the Task 4 cache exists.
- [ ] Gate: session tests + Credential Manager smoke test. Commit `feat: account sessions`.

## Cross-cutting

- **Before each milestone:** fmt, clippy `-D warnings` (workspace, all targets), tests (workspace, all targets), `cargo deny check`, release build, MSI build.
- **Secrets:** key only in Credential Manager (app) or `.env` (developer probe, git-ignored). Never in SQLite, logs, URLs, crash reports, fixtures, or commit messages.
- **Fixtures:** synthetic or scrubbed; generated PNG/JPEG for byte-exact tests; no catalog images in Git. Live tests behind `--features live-tests` + env var; never in CI.
- **Performance:** measured against the spike's reference machine and method; no redraw while idle.
- **Accessibility:** keyboard-complete UI is required for 1.0; screen-reader coverage documented per toolkit capability.
- **Privacy:** no telemetry; `docs/privacy.md` lists every host contacted and every file written.
- **Updates:** none automatic in v1.x. **i18n:** English only; user-facing strings centralized.

## Review focus

- Session change mid-request: scope + generation on every response and cache op (Tasks 3–5).
- 429 handling: pacer, shared cooldown, retries in budget (Task 2).
- Key isolation: origin check, no redirects with key (Task 2).
- Image safety and cache cap incl. staging (Task 4).
- No-clobber publication and collisions (Tasks 6, 8).
- Wallpaper undo and partial multi-monitor failure (Task 9).


