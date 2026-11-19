# Wallora implementation status

Updated 2026-11-19. The implementation reviewed for this snapshot ends at `5590726` on `feat/foundation`. [Execution plan](2026-11-18-wallora-plan-v2.md) · [Scope decisions](decisions.md)

## Current milestones

| Milestone | Status |
| --- | --- |
| Task 1 · Foundation | Windows workspace, provisional Iced shell, CI workflow, and MSI packaging are implemented. The API probe and guest website audit are recorded. The signed-in website audit remains open. |
| Task 2 · Provider client | Search and wallpaper requests, pacing, retries, cooldown, cancellation, and API-key origin isolation are implemented. Encoded image downloads have byte and time limits. |
| Task 3 · Sessions and preferences | Implemented and approved after local verification and independent review. Includes sign-in, username verification, Windows Credential Manager, SQLite account state, account switching, revocation, and local proxy preferences. |
| Task 4 · Image pipeline and caches | Ready to start against the local code gates. Implementation is pending. The next work is image decode limits, cache reservations and eviction, and account-scope integration. |

## Account behavior

- Sign-in checks the API key and username separately. The keyed settings and own-collection requests succeeded in the live probe. The supplied username’s public lookup returned 404 twice, so its identity remains inconclusive. The app can show the keyed own-collection list for an unverified username. Private item access and Favorites mapping are Task 6a work. [API evidence](api-capabilities.md#targeted-follow-up-2026-11-19)
- An upstream 401 retires the active session and clears private views. If both SQLite and Credential Manager reject cleanup, account metadata and a key can survive restart. Private settings and collections stay hidden until a fresh response arrives for the current scope.

## Verification

At `5590726`, formatting, both Clippy lanes, dependency policy checks, the release build, and local MSI artifact inspection passed. The default workspace suite passed 43 tests, and the `wallora-core/test-util` suite passed 95. The separately invoked Windows Credential Manager smoke test passed with a synthetic key. A provider timeout test failed intermittently in an earlier revision’s local run and passed on subsequent runs. Hosted results are tracked in [GitHub Actions](https://github.com/ppolkadot/Wallora/actions).

## Open work

- Finish the signed-in Wallhaven walkthrough and record the remaining account evidence. [Parity matrix](parity-matrix.md#outstanding-signed-in-checks)
- Complete the privacy inventory and confirm hosted Windows CI has uploaded an MSI for the branch.
- Complete the hardware check before choosing the Task 5 gallery toolkit. Iced remains provisional. Task 7 owns interactive Windows 10/11 install, upgrade, and uninstall checks.
- Under owner-approved option A, Wallora 1.0 means *complete for supported routes*, with documented gaps and applicable `Open on wallhaven.cc` hand-offs. Phase B starts after the Task 7 release gate passes. [Owner decision](decisions.md#3-parity-scope-checkpoint--task-1e)
