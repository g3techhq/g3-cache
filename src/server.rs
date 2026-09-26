//! In-process server caches: answers kept in the server's memory and shared
//! by every request. See the [crate docs](crate#server-cache) for when to
//! use one.
//!
//! Two ways in:
//!
//! - [`cache_shared`](crate::cache_shared)`(server = "5m")` on a server
//!   function whose answer is the same for every visitor caches the whole
//!   function. The attribute checks the function looks visitor-independent.
//! - A [`ServerCache`] static caches anything else: part of a function (an
//!   outside API call behind a database check), or a per-user answer with
//!   the user's id in the key.
//!
//! # What these caches cannot do
//!
//! - **Know about writes.** An entry lives until it expires. A per-user
//!   answer cached here comes back unchanged right after that user's own
//!   edit, even though the client invalidated and refetched. Cache only
//!   what may be stale for its whole TTL, or
//!   [`invalidate`](moka::future::Cache::invalidate) the entry in every
//!   mutation that changes it.
//! - **Span servers.** Each process has its own copy. With several
//!   replicas, an eviction reaches only the one that handled the write.

pub use moka;

use std::{future::Future, hash::Hash, ops::Deref, sync::OnceLock, time::Duration};

/// An in-process cache of up to `capacity` entries, each kept for `ttl`
/// after it was fetched. Declare one as a `static`, where it is needed;
/// it is built on first use:
///
/// ```
/// use g3_cache::ServerCache;
/// use std::time::Duration;
///
/// static DESCRIPTIONS: ServerCache<String, Option<String>> =
///     ServerCache::new(Duration::from_secs(60 * 60), 10_000);
///
/// # async fn google_description(_id: &str) -> Result<Option<String>, String> { Ok(None) }
/// async fn description(media_id: String) -> Result<Option<String>, String> {
///     DESCRIPTIONS
///         .get_or_fetch(media_id.clone(), google_description(&media_id))
///         .await
/// }
/// ```
///
/// Each cache is its own `static` rather than one shared instance: every
/// cache holds its own key and value types, and keeping unrelated answers
/// apart means one can't evict another.
///
/// `K` must include everything the answer depends on. For an answer that
/// depends on the signed-in user, that means the user's id: it arrives
/// through a session extractor, not the function's arguments, so a key
/// built from the arguments alone would give one user's answer to all.
///
/// The least useful entries are dropped first when full. For anything
/// [`get_or_fetch`](Self::get_or_fetch) and
/// [`get_or_insert`](Self::get_or_insert) don't cover, it dereferences to
/// the underlying [moka cache](moka::future::Cache).
pub struct ServerCache<K, V> {
    ttl: Duration,
    capacity: u64,
    cache: OnceLock<moka::future::Cache<K, V>>,
}

impl<K, V> ServerCache<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// A cache keeping up to `capacity` entries for `ttl` each. `const`, so
    /// it can initialize a `static` directly.
    pub const fn new(ttl: Duration, capacity: u64) -> Self {
        Self {
            ttl,
            capacity,
            cache: OnceLock::new(),
        }
    }

    /// The cached answer for `key`, or `fetch`'s. Concurrent misses on one
    /// key share a single `fetch`; an `Err` is returned to every waiter and
    /// not kept, so the next call retries.
    ///
    /// `E` must be `Clone`, as `dioxus::CapturedError` is, since every
    /// waiter gets a copy. For an error type that isn't (`anyhow::Error`),
    /// use [`try_get_with`](moka::future::Cache::try_get_with) through
    /// `Deref`, which returns it in an `Arc`.
    pub async fn get_or_fetch<E, Fut>(&self, key: K, fetch: Fut) -> Result<V, E>
    where
        Fut: Future<Output = Result<V, E>>,
        E: Clone + Send + Sync + 'static,
    {
        self.try_get_with(key, fetch)
            .await
            .map_err(|err| E::clone(&err))
    }

    /// The cached answer for `key`, or `fetch`'s, for a fetch that can't
    /// fail. Concurrent misses on one key share a single `fetch`.
    pub async fn get_or_insert<Fut>(&self, key: K, fetch: Fut) -> V
    where
        Fut: Future<Output = V>,
    {
        self.get_with(key, fetch).await
    }
}

impl<K, V> Deref for ServerCache<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    type Target = moka::future::Cache<K, V>;

    fn deref(&self) -> &Self::Target {
        self.cache.get_or_init(|| {
            moka::future::Cache::builder()
                .max_capacity(self.capacity)
                .time_to_live(self.ttl)
                .build()
        })
    }
}

/// Support for the code [`cache_shared`](crate::cache_shared) generates.
/// Not public API.
#[doc(hidden)]
pub mod __private {
    use serde::Serialize;

    /// The whole-function cache behind `#[cache_shared(server = ..)]`.
    pub type SharedCache<T> = super::ServerCache<String, T>;

    /// The arguments of one call, as the key of its cached answer.
    pub fn args_key<A: Serialize>(args: &A) -> String {
        serde_json::to_string(args).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::ServerCache;
    use std::{
        sync::atomic::{AtomicU32, Ordering},
        time::Duration,
    };

    #[tokio::test]
    async fn answers_are_shared_and_errors_are_not_kept() {
        static CACHE: ServerCache<u32, u32> = ServerCache::new(Duration::from_secs(60), 100);
        static CALLS: AtomicU32 = AtomicU32::new(0);

        let fetch = || async {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(7)
        };
        assert_eq!(CACHE.get_or_fetch(1, fetch()).await, Ok(7));
        assert_eq!(CACHE.get_or_fetch(1, fetch()).await, Ok(7));
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);

        let failing = async { Err::<u32, _>("down".to_string()) };
        assert_eq!(
            CACHE.get_or_fetch(2, failing).await,
            Err("down".to_string())
        );
        assert_eq!(CACHE.get_or_fetch(2, fetch()).await, Ok(7));
    }

    #[tokio::test]
    async fn infallible_fetches_and_moka_calls_share_the_cache() {
        static CACHE: ServerCache<&str, u32> = ServerCache::new(Duration::from_secs(60), 100);
        assert_eq!(CACHE.get_or_insert("a", async { 1 }).await, 1);
        assert_eq!(CACHE.get("a").await, Some(1));
        CACHE.invalidate("a").await;
        assert_eq!(CACHE.get_or_insert("a", async { 2 }).await, 2);
    }
}
