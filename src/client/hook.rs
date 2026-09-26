use super::{
    EPOCH, STALE, config, invalidate_cached_key, now_ms, persist, remember, remembered, restore,
};
use crate::{CacheKey, CacheableFn};
use dioxus::prelude::{
    CopyValue, ReadableExt, ReadableRef, Result, Signal, WritableExt, use_hook, use_resource,
    use_signal,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{future::Future, pin::Pin, rc::Rc};

/// Per-read settings for [`use_cached_with`] and [`use_cached_key_with`].
#[derive(Clone, Copy, Debug)]
pub struct CacheOptions {
    persist: bool,
}

impl Default for CacheOptions {
    fn default() -> Self {
        Self { persist: true }
    }
}

impl CacheOptions {
    /// Keeps this read in memory only, never in the persistent store. For
    /// data that should not sit on the device's disk, or that is worthless
    /// on a cold start (a one-time token, a live status).
    pub fn memory_only(mut self) -> Self {
        self.persist = false;
        self
    }
}

type Fetch<T> = Rc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<T>>>>>;

/// A cached read's current answer, returned by [`use_cached`].
/// `Copy`, like a signal.
pub struct Cached<T: 'static> {
    data: Signal<Option<Result<T>>>,
    key: Signal<CacheKey>,
    pending: Signal<bool>,
}

impl<T> Clone for Cached<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Cached<T> {}

impl<T: 'static> Cached<T> {
    /// What to show: `None` until there is something, then a remembered,
    /// stored or fetched value. `Some(Err(_))` only when a fetch failed and
    /// there was nothing to show instead; while offline, the last value
    /// stays.
    pub fn read(&self) -> ReadableRef<'_, Signal<Option<Result<T>>>> {
        self.data.read()
    }

    /// Refetches now, keeping the current value on screen meanwhile. For a
    /// pull-to-refresh or a retry button.
    pub fn refresh(&self) {
        invalidate_cached_key(&self.key.peek());
    }

    /// Whether a fetch is in flight, whether or not there is a value on
    /// screen meanwhile. For a pull-to-refresh spinner, or a "refreshing"
    /// hint over a list that is already showing.
    pub fn pending(&self) -> bool {
        (self.pending)()
    }
}

/// Reads a server function through the client cache. **This is the hook to
/// use for data a screen shows when it opens.**
///
/// ```ignore
/// let media = use_cached(get_media, (id.clone(),));
/// let bookmarks = use_cached(get_bookmarks, ());
/// let reviews = use_cached(get_media_reviews, (id.clone(), 0, 21));
///
/// match &*media.read() {
///     Some(Ok(media)) => rsx! { MediaView { media: media.clone() } },
///     Some(Err(_)) => rsx! { ErrorState {} },
///     None => rsx! { Loading {} },
/// }
/// ```
///
/// Pass the function itself and its arguments as a tuple: the pair is the
/// cache key, so the key always matches what is fetched. When the
/// arguments change between renders (new props), it reads the new key.
///
/// A read is answered, in order, from
///
/// 1. memory, for anything fetched this session, then
/// 2. the persistent store, for anything fetched in an earlier session
///    (see [`set_cache_owner`](crate::set_cache_owner)), so a cold start shows the last
///    known data at once, then
/// 3. the server, every time the screen opens, unless the same read
///    finished within [`CacheConfig::share_for`](crate::CacheConfig::share_for). Its
///    answer replaces what was showing.
///
/// A mounted screen refetches after [`invalidate_cached`](crate::invalidate_cached) and
/// when the app regains focus (see [`CacheConfig`](crate::CacheConfig)).
///
/// # When not to use it
///
/// - **Reads that change with every keystroke** (search boxes, pickers):
///   use `use_resource`. Every value is thrown away a moment later.
/// - **Handlers that must see current server state before acting**: call
///   the server function directly. A cache answers with what it last saw.
/// - **Closures.** `use_cached(|| get_media(id), ())` panics: see
///   [`CacheableFn`]. For a read that is not one server function call, use
///   [`use_cached_key`] with a named key.
///
/// On the server (the `server` feature) nothing is cached: every render
/// fetches.
pub fn use_cached<F, Args>(server_fn: F, args: Args) -> Cached<F::Output>
where
    F: CacheableFn<Args>,
    Args: Serialize + Clone + 'static,
    F::Output: Serialize + DeserializeOwned + Clone + PartialEq + 'static,
{
    use_cached_with(server_fn, args, CacheOptions::default())
}

/// [`use_cached`] with per-read [`CacheOptions`].
pub fn use_cached_with<F, Args>(
    server_fn: F,
    args: Args,
    options: CacheOptions,
) -> Cached<F::Output>
where
    F: CacheableFn<Args>,
    Args: Serialize + Clone + 'static,
    F::Output: Serialize + DeserializeOwned + Clone + PartialEq + 'static,
{
    let key = CacheKey::of(&server_fn, &args);
    use_cached_key_with(key, options, move || server_fn.call_with(args.clone()))
}

/// Reads through the client cache under a key you name. Use it for a read
/// that is not one server function call: several calls combined, or data
/// from somewhere else. Prefer [`use_cached`] otherwise.
///
/// `key` must name everything `fetch` depends on. A changed key is what
/// makes it refetch, and two reads with the same key share one entry.
///
/// ```ignore
/// let shelves = use_cached_key(CacheKey::new("home_shelves").with(media_type), move || async move {
///     let (trending, new) = futures::join!(get_trending(media_type), get_new_releases(media_type));
///     Ok(Shelves { trending: trending?, new: new? })
/// });
/// ```
///
/// Invalidate it with [`invalidate_cached_name`](crate::invalidate_cached_name) or
/// [`invalidate_cached_key`](crate::invalidate_cached_key).
pub fn use_cached_key<T, F, Fut>(key: CacheKey, fetch: F) -> Cached<T>
where
    T: Serialize + DeserializeOwned + Clone + PartialEq + 'static,
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<T>> + 'static,
{
    use_cached_key_with(key, CacheOptions::default(), fetch)
}

/// [`use_cached_key`] with per-read [`CacheOptions`].
pub fn use_cached_key_with<T, F, Fut>(key: CacheKey, options: CacheOptions, fetch: F) -> Cached<T>
where
    T: Serialize + DeserializeOwned + Clone + PartialEq + 'static,
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<T>> + 'static,
{
    let mut data = use_signal(|| remembered::<T>(&key).map(|(value, _)| Ok(value)));
    let mut current_key = use_signal(|| key.clone());
    let mut pending = use_signal(|| false);
    if *current_key.peek() != key {
        current_key.set(key);
    }

    // The newest closure, since it captures this render's props.
    let mut latest = use_hook(|| CopyValue::new(None::<Fetch<T>>));
    latest.set(Some(Rc::new(move || Box::pin(fetch()))));

    // The key this hook last loaded. A rerun for that same key comes from
    // `EPOCH`, and refetches only if its entry was invalidated.
    let mut loaded = use_hook(|| CopyValue::new(None::<CacheKey>));

    use_resource(move || {
        let key = current_key();
        let _ = EPOCH();
        let fetch = latest.peek().clone();
        async move {
            let Some(fetch) = fetch else { return };
            let opening = loaded.peek().as_ref() != Some(&key);
            match remembered::<T>(&key) {
                Some((value, fetched_at)) => {
                    show(&mut data, value);
                    let current = if opening {
                        let share_for = i64::try_from(config().share_for.as_millis()).unwrap_or(0);
                        now_ms().saturating_sub(fetched_at) < share_for
                    } else {
                        fetched_at != STALE
                    };
                    if current {
                        loaded.set(Some(key));
                        // A rerun can cancel an earlier one mid-fetch.
                        if *pending.peek() {
                            pending.set(false);
                        }
                        return;
                    }
                }
                None => match options.persist.then(|| restore::<T>(&key)) {
                    Some(restoring) => match restoring.await {
                        Some(value) => {
                            remember(&key, value.clone(), STALE);
                            show(&mut data, value);
                        }
                        None => data.set(None),
                    },
                    None => data.set(None),
                },
            }

            // Cleared by whichever run finishes last: a rerun cancels this one
            // mid-fetch, and sets it again itself.
            pending.set(true);
            let result = fetch().await;
            pending.set(false);
            if *current_key.peek() != key {
                // The screen moved on to another key while this was in flight.
                return;
            }
            loaded.set(Some(key.clone()));
            match result {
                Ok(value) => {
                    remember(&key, value.clone(), now_ms());
                    show(&mut data, value.clone());
                    if options.persist {
                        persist(&key, &value).await;
                    }
                }
                // Offline or failing: keep showing what we had, if anything.
                Err(err) if data.peek().as_ref().is_none_or(Result::is_err) => {
                    data.set(Some(Err(err)));
                }
                Err(_) => {}
            }
        }
    });

    Cached {
        data,
        key: current_key,
        pending,
    }
}

/// Sets `data` only when `value` differs, so an unchanged refetch doesn't
/// re-render the screen.
fn show<T: PartialEq + 'static>(data: &mut Signal<Option<Result<T>>>, value: T) {
    let unchanged = matches!(&*data.peek(), Some(Ok(current)) if *current == value);
    if !unchanged {
        data.set(Some(Ok(value)));
    }
}
