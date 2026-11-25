# Wallora Task 3 re-review at 5590726

Reviewed `feat/foundation` at `5590726f163110270f6fd85c8044c080fa2a921f` over `24b4a8f597fc0df780aa111b1b1ae13b23d1b02b..5590726f163110270f6fd85c8044c080fa2a921f`. The packet patch matches the pinned Git diff, and the diff whitespace check passed.

**Both blockers addressed. Task 4 can start against the local Task 3 code gates.** The fix diff produced no demonstrated new important regression.

## Blocker verdicts

1. **P1 addressed.** A current-scope 401 switches to Guest, advances generation, drops the active account from memory, and clears settings and collections even when SQLite rejects the cleanup-intent insert. The fallback independently attempts transactional deactivation and cache purge and idempotent vault deletion. It finalizes removal of the account row and cascaded cache only after successful vault deletion or confirmed absence. On Windows, the vault delete call reports success for a successful deletion or a missing entry. Both one-store failure cases restarted as Guest in the independent fault test. If only deactivation and cache purge succeed, inactive account metadata and the failed-to-delete key can remain. [Revocation and fallback](C:/Users/A3/Wallora/crates/wallora-core/src/services/session.rs:729) [SQLite cleanup](C:/Users/A3/Wallora/crates/wallora-core/src/storage/db.rs:346) [Windows vault contract](C:/Users/A3/Wallora/crates/wallora-windows/src/credentials.rs:92)
   
   When both cleanup paths fail, restart can restore saved account metadata and the key can remain in the vault. Startup and account switch create empty settings and collection views. A failed initial refresh leaves them empty, an old generation is discarded, and a successful current-scope refresh can populate them after persistence succeeds. A synthetic switch test left concrete settings and a named private collection on disk and exercised all three refresh outcomes. The committed restart case also exercises a later 401 completing cleanup after writes recover. [Startup](C:/Users/A3/Wallora/crates/wallora-core/src/services/session.rs:340) [Refresh](C:/Users/A3/Wallora/crates/wallora-core/src/services/session.rs:575) [Switch](C:/Users/A3/Wallora/crates/wallora-core/src/services/session.rs:818) [Dual-store failure test](C:/Users/A3/Wallora/crates/wallora-core/tests/session.rs:1223)

2. **P2 addressed.** Signed-in Settings requires an HTTP 200 and a readable body. The shared pacer retries body timeouts as `Timeout` and interrupted reads as `Network`, with three hits observed for each in loopback tests. Malformed JSON in a fully read body stays a decode error. Candidate sign-in retains the original policy. A 200 establishes key validity even if the body is unreadable or defaults are undecodable, so identity checks can proceed and Guest browse defaults can be used. Settings 401 is invalid and a decodable 201 is retry-later, with no collection lookups in either case. The candidate still makes one Settings request when the server closes a truncated 200 body. [Response handling](C:/Users/A3/Wallora/crates/wallora-core/src/provider/client.rs:517) [Scoped Settings](C:/Users/A3/Wallora/crates/wallora-core/src/provider/client.rs:644) [Candidate Settings](C:/Users/A3/Wallora/crates/wallora-core/src/provider/client.rs:704) [Candidate sign-in reducer](C:/Users/A3/Wallora/crates/wallora-core/src/services/session.rs:104)

## Verification

Four committed tests passed in a fresh targeted run from the pinned archive, including the three new regressions and the existing candidate status test. Three additional synthetic checks in that temporary archive passed. They covered one-sided cleanup failures, visibility across switching and refreshing with private content on disk, and the truncated-body distinction between scoped and candidate Settings. The parent reported red/green results for the three new regressions and green workspace test lanes with 43 and 95 passes, both Clippy lanes, fmt, the Windows Credential Manager synthetic smoke, cargo deny, release build, and the local pinned-WiX MSI workflow check.

## Outstanding gates

- The owner-deferred signed-in Task 1 website audit is open.
- Hosted CI is open.
- Iced remains provisional pending the hardware check before the Task 5 toolkit decision.
- The inherited privacy inventory in `docs/privacy.md` is open.
- Task 7 still owns the interactive Windows 10/11 MSI install, upgrade and uninstall walkthrough.
