//! Caching for Dioxus fullstack apps: on the device, on the server, and at
//! the CDN, with checks against caching the wrong thing in the wrong place.
//!
//! ```ignore
//! use g3_cache::{cache_shared, invalidate_cached, use_cached};
//!
//! // A screen: show the last known answer at once, refetch in the background.
//! let media = use_cached(get_media, (id.clone(),));
//!
//! // After a mutation: refetch what it changed.
//! save_rating(id.clone(), score).await?;
//! invalidate_cached(get_my_rating);
//!
//! // A public read: cached at the CDN and on the server for 5 minutes.
//! #[cache_shared(cdn = 300, server = "5m")]
//! #[get("/api/trending?media_type", db: Db)]
//! pub async fn get_trending(media_type: Option<MediaType>) -> Result<Vec<Media>> { .. }
//! ```
//!
//! Every name says it is about caching, so the crate reads well through a
//! `use` statement.
//!
//! # Choosing a cache
//!
//! A read can be cached in three places. Each answers a different problem,
//! and none replaces another.
//!
//! | | [Client](#client-cache) | [Server](#server-cache) | [CDN](#cdn-cache) |
//! |---|---|---|---|
//! | **For** | Showing the last known data at once, then refreshing it | Not repeating slow or rate-limited work | Answering identical public requests without reaching the server |
//! | **Holds** | One user's data, on one device | Answers shared by every request, in one server process | Whole HTTP responses shared by every visitor |
//! | **Refreshed by** | Screen opens, [`invalidate_cached`], app focus | Expiry only | Expiry only |
//! | **API** | [`use_cached`] | [`cache_shared`]`(server = ..)`, `ServerCache` | [`cache_shared`]`(cdn = ..)`, `cdn_cache_guard` |
//!
//! **The rule:** data that depends on who is asking (their ratings, their
//! lists) is cached on the client only. Data that is the same for everyone
//! (trending titles, a catalog) may also be cached on the server and at the
//! CDN.
//!
//! A read passes through them in order: client memory and store, then the
//! browser's HTTP cache and the CDN, then the server, whose own cache may
//! answer instead of the database or an outside API.
//!
//! # Setup
//!
//! Enable the feature matching each build:
//!
//! ```toml
//! [dependencies]
//! g3-cache = "0.2"
//!
//! [features]
//! web = ["dioxus/web", "g3-cache/web"]          # IndexedDB store
//! mobile = ["dioxus/mobile", "g3-cache/mobile"] # redb file store
//! desktop = ["dioxus/desktop", "g3-cache/desktop"] # redb file store
//! server = ["dioxus/server", "g3-cache/server"] # server + CDN caches; client cache off
//! ```
//!
//! Then, in the app:
//!
//! ```ignore
//! use g3_cache::{CacheConfig, set_cache_owner, use_client_cache};
//!
//! fn App() -> Element {
//!     // 1. Once, first thing in the root component.
//!     use_client_cache(CacheConfig::new("my-app"));
//!
//!     // 2. Whenever the signed-in user is known or changes, and on sign-out.
//!     let user = use_server_future(get_current_user)?;
//!     use_effect(move || {
//!         let owner = user().and_then(|user| user.ok()).flatten().map(|user| user.id);
//!         spawn(set_cache_owner(owner));
//!     });
//!     // ...
//! }
//! ```
//!
//! And on the server router, once, outside the session layer:
//!
//! ```ignore
//! .layer(session_layer)
//! .layer(g3_cache::cdn_cache_guard("/api"))
//! ```
//!
//! # Client cache
//!
//! Use [`use_cached`] in place of `use_resource` for data a screen shows
//! when it opens:
//!
//! ```ignore
//! let media = use_cached(get_media, (id.clone(),));
//! ```
//!
//! The screen shows what the device last saw (from memory, or from disk on
//! a cold start) and refetches in the background; the fresh answer replaces
//! it. After a mutation, [`invalidate_cached`] each read it changed:
//!
//! ```ignore
//! save_rating(id.clone(), score).await?;
//! invalidate_cached(get_my_rating);
//! invalidate_cached(get_my_ratings);
//! ```
//!
//! Don't use it for reads that change with every keystroke (use
//! `use_resource`), or in a handler that must act on current server state
//! (call the server function directly).
//!
//! ## Showing a change at once
//!
//! A tap that waits for a round trip feels broken. [`update_cached`] edits
//! what mounted screens show before the mutation is sent; the invalidation
//! after it replaces the guess with the server's answer, and so also undoes
//! it if the mutation failed:
//!
//! ```ignore
//! update_all_cached(get_feed_page, |page| page.mark_watched(&video_id));
//! let saved = set_watched(video_id.clone(), true).await;
//! invalidate_cached(get_feed_page);
//! saved?;
//! ```
//!
//! [`Cached::pending`] says whether a refetch is in flight, for a
//! pull-to-refresh spinner over a list that is already showing.
//!
//! ## Several devices
//!
//! The server is the only source of truth: mutations go straight to it, and
//! nothing is written offline, so devices never need merging. The question
//! is only how long a device shows what it saw last:
//!
//! - **A screen that opens** shows its old copy and refetches at once.
//! - **A screen that stayed open** refetches when the app comes back into
//!   view ([`CacheConfig::revalidate_on_focus`], on by default).
//! - **A tap on stale data** is still sent. Write mutations so that is
//!   harmless: `set_in_list(list, item, true)` rather than
//!   `toggle_in_list(list, item)`, which undoes the other device's change
//!   when this one shows old state. A full reorder should place items it
//!   was not sent after the ones it was.
//!
//! # Server cache
//!
//! For work worth skipping: a slow query, an outside API with a rate limit
//! or a price. A read by id from your own database rarely is.
//!
//! When the whole answer is the same for every visitor, cache the whole
//! server function:
//!
//! ```ignore
//! #[cache_shared(server = "5m")]
//! #[get("/api/trending?media_type", db: Db)]
//! pub async fn get_trending(media_type: Option<MediaType>) -> Result<Vec<Media>> { .. }
//! ```
//!
//! Otherwise, cache the expensive part in a `ServerCache` static, with the
//! user's id in the key when the answer depends on the user:
//!
//! ```ignore
//! static DESCRIPTIONS: ServerCache<String, Option<Description>> =
//!     ServerCache::new(Duration::from_secs(60 * 60), 10_000);
//! ```
//!
//! Server caches only expire. Anything a user can change and then expects
//! to see changed should not be cached there; see `server`.
//!
//! # CDN cache
//!
//! For public `GET` answers read by many visitors. [`cache_shared`]`(cdn = ..)`
//! marks a function's successful responses `public`; `cdn_cache_guard`
//! keeps everything else private and strips session cookies from what is
//! shared. Only standard `Cache-Control` headers are used, so any CDN works
//! (Cloudflare, CloudFront, Fastly, Vercel, a reverse proxy); most need a
//! rule making API paths eligible. See `cdn` for each provider.
//!
//! # What is checked for you, and what is not
//!
//! | Mistake | Caught |
//! |---|---|
//! | Passing a closure where a server function is expected | Panics on first use, with the fix |
//! | `#[cache_shared]` on a function that binds a session, auth, user or cookie extractor | Compile error |
//! | `#[cache_shared]` on a function whose body reads `FullstackContext` | Compile error |
//! | `#[cache_shared]` on a `POST`, `PUT`, `PATCH`, `DELETE` or `#[server]` | Compile error |
//! | `#[cache_shared]` placed below `#[get]`, or with a malformed duration | Compile error |
//! | `#[cache_shared]` in a server build without `g3-cache/server` | Warning naming the fix; runs uncached |
//! | A shared response carrying a session cookie | Removed at runtime by `cdn_cache_guard` |
//! | An error response being cached at the CDN or on the server | Never cached |
//! | Persisting data before knowing whose it is | Memory only until [`set_cache_owner`] |
//! | A shared function reading the viewer some other way (a global, a header, a query using the session) | **Not caught.** Only share functions whose answer comes from their arguments |
//! | A mutation that forgets to [`invalidate_cached`] a read it changed | **Not caught.** The read stays stale until the screen reopens or the app regains focus |
//! | A per-user answer in a `ServerCache` without the user in the key | **Not caught.** Put the user's id in `K` |
//! | Toggle-style mutations acting on stale state | **Not caught.** Prefer "set to" mutations |
//! | Missing CDN rule | **Not caught.** Nothing is cached at the CDN; harmless |
//!
//! # Features
//!
//! - `web`: the persistent store is IndexedDB.
//! - `mobile`: the persistent store is a redb file in the OS cache directory.
//! - `desktop`: the same redb store, in the user's cache directory.
//! - `server`: `ServerCache`, `cdn_cache_guard` and the `server` and `cdn`
//!   modules. Turns the client cache off: one server process renders for
//!   every visitor, so nothing per-user may be kept there.
//!
//! With none of `web`, `mobile` or `desktop`, the client cache works in
//! memory only.

#![warn(missing_docs)]

mod client;
mod key;

#[cfg(feature = "server")]
pub mod cdn;
#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "server")]
pub use cdn::cdn_cache_guard;
#[cfg(feature = "server")]
pub use server::ServerCache;

pub use client::{
    CacheConfig, CacheOptions, Cached, invalidate_all_cached, invalidate_cached,
    invalidate_cached_call, invalidate_cached_key, invalidate_cached_name, set_cache_owner,
    update_all_cached, update_cached, update_cached_key, use_cached, use_cached_key,
    use_cached_key_with, use_cached_with, use_client_cache,
};
pub use g3_cache_macros::cache_shared;
pub use key::{CacheKey, CacheableFn};

/// Support for the code [`cache_shared`] generates. Not public API.
#[doc(hidden)]
pub mod __private {
    #[cfg(feature = "server")]
    pub use crate::cdn::cdn_cache_for;
    #[cfg(feature = "server")]
    pub use crate::server::__private::*;

    // Stand-ins for a server build that forgot `g3-cache/server`: the
    // function then runs uncached, with a warning that says how to fix it,
    // instead of failing on a path that doesn't exist.

    #[cfg(not(feature = "server"))]
    #[deprecated(
        note = "`#[cache_shared(cdn = ..)]` is not caching: add `g3-cache/server` to your app's `server` feature"
    )]
    pub fn cdn_cache_for(_cdn_secs: u32) -> tower_layer::Identity {
        tower_layer::Identity::new()
    }

    #[cfg(not(feature = "server"))]
    pub struct SharedCache<T>(std::marker::PhantomData<fn() -> T>);

    #[cfg(not(feature = "server"))]
    impl<T> SharedCache<T> {
        #[deprecated(
            note = "`#[cache_shared(server = ..)]` is not caching: add `g3-cache/server` to your app's `server` feature"
        )]
        pub const fn new(_ttl: std::time::Duration, _capacity: u64) -> Self {
            Self(std::marker::PhantomData)
        }

        pub async fn get_or_fetch<E, Fut>(&self, _key: String, fetch: Fut) -> Result<T, E>
        where
            Fut: std::future::Future<Output = Result<T, E>>,
        {
            fetch.await
        }
    }

    #[cfg(not(feature = "server"))]
    pub fn args_key<A>(_args: &A) -> String {
        String::new()
    }
}
