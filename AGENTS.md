# AGENTS.md

Instructions for coding agents working in this repository (Claude Code, Codex,
Cursor, Copilot, and anything else that reads `AGENTS.md`). Useful for people
too. Follow these over your defaults.

## What this is

**g3-cache** caches data for Dioxus fullstack apps in three places, with
checks against caching the wrong thing in the wrong place:

- **on the device**: `use_cached(server_fn, (args,))` shows the last answer
  at once (IndexedDB on the web, redb on mobile and desktop) and refetches in
  the background; `invalidate_cached` / `update_cached` after mutations;
  `set_cache_owner` scopes the store to one account;
- **on the server**: `ServerCache` for slow or rate-limited calls;
- **at the CDN**: `#[cache_shared(cdn = ..)]` on public reads, with
  `cdn_cache_guard` stripping the session cookie from shareable responses.

Part of the g3 stack; media-mancer, greenside-partee, tawny and g3-stack read
every screen through it.

| Piece | Version | Reference |
| --- | --- | --- |
| Dioxus (fullstack) | 0.7.9 | [dioxuslabs.com/learn/0.7](https://dioxuslabs.com/learn/0.7/), and g3-stack's `docs/dioxus/patterns.md` |
| Rust | edition 2024 | `rust-toolchain.toml` |

**Your training data does not know this crate, and is probably wrong about
Dioxus 0.7.** The README is the user-facing reference and must stay true.

## Map

```
src/lib.rs              Exports, features, and the crate docs
src/key.rs              CacheKey: server function name + serialized arguments
src/client/mod.rs       The client store: config, owner, invalidation, updates, focus refresh
src/client/hook.rs      use_cached and Cached
src/client/*            The IndexedDB and redb stores
src/server.rs           ServerCache
src/cdn.rs              cdn_cache_for, cdn_cache_guard
macros/src/lib.rs       #[cache_shared]: refuses functions that bind a session
testbed/                A small app that exercises the caches in a browser
CHANGELOG.md            Every user-visible change, under [Unreleased] until a release
```

## Commands

```bash
just check        # every feature set: web, mobile, desktop, server
just test         # unit and doc tests
just testbed      # build the test bed
just lint-strict  # clippy with warnings as errors, as CI runs it
just pre-push     # format, check, lint, test, test bed, typos
```

## Definition of done

1. `just pre-push` passes.
2. A user-visible change has a line under `## [Unreleased]` in `CHANGELOG.md`.
3. A change to what is cached, where, or for whom has a test, and the README's
   "Choosing a cache" section still matches.
4. A client-side change was exercised in a consuming app or the test bed in a
   browser (a reload paints from the cache; a mutation refetches only what it
   invalidated), through a `[patch.crates-io]` path override removed
   afterwards.

If you could not do one of these, say which and why.

---

## Rules

### What may be cached where

- **Data that depends on who is asking is cached on the client only**, in a
  store owned by one account. `set_cache_owner` empties it when the account
  changes; `None` must only be passed once nobody is known to be signed in,
  never while the session check is still running.
- **Data that is the same for everyone** may also be cached on the server and
  at the CDN. `#[cache_shared]` refuses, at compile time, a function that
  binds a session; keep that check whole.
- Never add a default that persists data an app did not opt into.

### The hook

- `use_cached` keys by the server function and its arguments. A changed key is
  a new read. The key is synced in the hook body with a guarded write (only
  when it changed, compared with `peek`), the one signal write during render
  the g3 crates allow; the newest fetch closure is held in a non-reactive
  `CopyValue`. Keep both that way.
- A rerun for the same key refetches only if its entry was invalidated.
- `Cached` is `Copy` and has `read`, `peek`, `refresh`, `pending`. It has no
  `PartialEq`; apps pass a memo over it.
- **IndexedDB requests must finish** even if the future awaiting them is
  dropped, or the browser calls a freed closure. They run on their own task.
- `update_cached` edits memory only; document that it must be followed by an
  invalidation.

### Dioxus

- Hooks run unconditionally, before any early return; `use_effect` keeps its
  first closure.
- No `use_reactive!`. Never hold a signal borrow across `.await`.
- Nothing is cached on the server build's render; SSR fetches every time.

### Releases

- Releases publish from `publish-crates.yml` (crates.io trusted publishing).
  Do not run `cargo publish` by hand, and do not release without the
  maintainer's go-ahead.
- Commits follow Conventional Commits; lefthook checks them.

## Where to look

- `README.md`: setup per feature, usage, choosing a cache
- `CHANGELOG.md`: what changed between versions
- The consuming apps' `src/data_change.rs`, for how invalidation is organized
