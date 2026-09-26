# g3-cache

[![CI](https://github.com/g3techhq/g3-cache/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/g3techhq/g3-cache/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/g3-cache.svg)](https://crates.io/crates/g3-cache)
[![docs.rs](https://docs.rs/g3-cache/badge.svg)](https://docs.rs/g3-cache)
[![License](https://img.shields.io/crates/l/g3-cache.svg)](#license)

Caching for Dioxus fullstack apps, part of the g3 stack: on the device, on
the server, and at the CDN, with checks against caching the wrong thing in
the wrong place.

## Setup

```toml
[dependencies]
g3-cache = "0.1"

[features]
web = ["dioxus/web", "g3-cache/web"]          # IndexedDB store
mobile = ["dioxus/mobile", "g3-cache/mobile"] # redb file store
server = ["dioxus/server", "g3-cache/server"] # server + CDN caches; client cache off
```

## Usage

```rust
use g3_cache::{cache_shared, invalidate_cached, use_cached};

// A screen: show the last known answer at once, refetch in the background.
let media = use_cached(get_media, (id.clone(),));

// After a mutation: refetch what it changed.
save_rating(id.clone(), score).await?;
invalidate_cached(get_my_rating);

// A public read: cached at the CDN and on the server for 5 minutes.
#[cache_shared(cdn = 300, server = "5m")]
#[get("/api/trending?media_type", db: Db)]
pub async fn get_trending(media_type: Option<MediaType>) -> Result<Vec<Media>> { .. }
```

## Choosing a cache

| | Client | Server | CDN |
|---|---|---|---|
| **For** | Showing the last known data at once | Not repeating slow or rate-limited work | Answering identical public requests without the server |
| **Holds** | One user's data, on one device | Shared answers, in one process | Whole shared HTTP responses |
| **Refreshed by** | Screen opens, `invalidate_cached`, app focus | Expiry only | Expiry only |
| **API** | `use_cached`, `use_cached_key` | `#[cache_shared(server = ..)]`, `ServerCache` | `#[cache_shared(cdn = ..)]`, `cdn_cache_guard` |

**The rule:** data that depends on who is asking is cached on the client
only. Data that is the same for everyone may also be cached on the server
and at the CDN. `#[cache_shared]` refuses, at compile time, functions that
bind a session, auth, user or cookie extractor, and anything that isn't a
`GET`.

For caching part of a server function, declare a cache where it is used:

```rust
static DESCRIPTIONS: ServerCache<String, Option<Description>> =
    ServerCache::new(Duration::from_secs(60 * 60), 10_000);

DESCRIPTIONS.get_or_fetch(media_id, google_description(&media)).await
```

In the app:

```rust
use g3_cache::{CacheConfig, set_cache_owner, use_client_cache};

fn App() -> Element {
    // Once, first thing in the root component.
    use_client_cache(CacheConfig::new("my-app"));
    // Whenever the signed-in user is known or changes, and on sign-out:
    // spawn(set_cache_owner(user_id));
    // ...
}
```

```rust
// Server router: once, outside the session layer.
.layer(session_layer)
.layer(g3_cache::cdn_cache_guard("/api"))
```

Only standard `Cache-Control` headers are sent (`public`, `s-maxage`,
`private`, `no-cache`), so any CDN or shared cache works. Most won't cache
API responses until told to (in Cloudflare, a Cache Rule making the API
paths eligible); until then nothing is cached at the CDN, which is safe.

What you still have to handle:

- **Invalidate after mutations.** Only the mutation knows which reads it
  changed.
- **Write mutations as "set to", not "toggle".** A device showing stale
  state sends what it saw; a toggle then undoes another device's change.
- **Only share functions whose answer comes from their arguments.** The
  compile-time check catches session-like extractors, not every way a
  function can read the visitor.


## Test bed

`testbed/` is a small Dioxus app using the cache API against real server
functions. `just testbed` builds it for the server and the browser.

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your
option.
