# Changelog

All notable changes to `g3-cache` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-09-25

### Added

Briefly published as part of the since-yanked `g3-kit`:

- Client cache: `use_cached` (a server function and its arguments) and
  `use_cached_key` (a named key), answered from memory, then a persistent
  store (IndexedDB on web, redb on mobile), then the server.
- `invalidate_cached`, `invalidate_cached_call`, `invalidate_cached_key`,
  `invalidate_cached_name` and `invalidate_all_cached`; refetching on app
  focus; `set_cache_owner`, which empties the cache when the signed-in user
  changes and gates persistence until called.
- `#[cache_shared(cdn = .., server = ..)]` for server functions whose answer
  is the same for every visitor, refusing non-`GET` routes, session-like
  extractors and `FullstackContext` reads at compile time.
- `ServerCache`, a `const`-constructible in-process cache for `static`s, and
  a `moka` re-export.
- `cdn::cdn_cache_for` and `cdn_cache_guard`: standard `Cache-Control`
  headers for any CDN, with session cookies stripped from shared responses
  and `private, no-cache` on every other API response.
