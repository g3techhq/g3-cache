//! The client cache: see the [crate docs](crate#client-cache) for when to
//! use it, and [`use_cached`] for how a read is answered.

mod hook;
mod store;

pub use hook::*;

use crate::{CacheKey, CacheableFn};
use dioxus::prelude::{GlobalSignal, Signal, document, spawn, use_hook};
use serde::Serialize;
use std::{
    any::Any,
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::OnceLock,
    time::Duration,
};

/// The server renders for every visitor from one process, so nothing is
/// cached there: every read fetches, and invalidation does nothing.
pub(crate) const ENABLED: bool = !cfg!(feature = "server");

/// The `fetched_at` of an entry that must be refetched: invalidated, or
/// restored from the persistent store.
pub(crate) const STALE: i64 = i64::MIN;

/// Stored under this key: whose data the persistent store holds.
const OWNER_KEY: &str = "\u{1f}owner";

/// Settings for the client cache, given once to [`use_client_cache`].
///
/// ```
/// use g3_cache::CacheConfig;
/// use std::time::Duration;
///
/// let config = CacheConfig::new("media-mancer").keep_for(Duration::from_secs(3 * 24 * 60 * 60));
/// ```
#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub(crate) name: Cow<'static, str>,
    pub(crate) share_for: Duration,
    pub(crate) keep_for: Duration,
    pub(crate) revalidate_on_focus: bool,
    pub(crate) focus_gap: Duration,
}

impl CacheConfig {
    /// Settings with the defaults below. `name` names the persistent store
    /// (the IndexedDB database, or the redb file on mobile), so it must be
    /// unique to the app on a device or origin.
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: name.into(),
            share_for: Duration::from_secs(2),
            keep_for: Duration::from_secs(7 * 24 * 60 * 60),
            revalidate_on_focus: true,
            focus_gap: Duration::from_secs(30),
        }
    }

    /// The persistent store's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How recent a fetch a screen may reuse instead of repeating it when it
    /// opens. Default 2 s. This only merges the reads of components that
    /// mount together (a screen and its children reading the same thing);
    /// keep it short, since a change made without [`invalidate_cached`] shows only
    /// once something refetches.
    pub fn share_for(mut self, duration: Duration) -> Self {
        self.share_for = duration;
        self
    }

    /// How old a stored value may be and still be shown on a cold start
    /// while its refetch runs. Default 7 days: an older list is more
    /// misleading than a loading state.
    pub fn keep_for(mut self, duration: Duration) -> Self {
        self.keep_for = duration;
        self
    }

    /// Whether returning to the app (the tab or window regaining focus, the
    /// app coming back to the foreground, the network coming back) refetches
    /// every mounted read. Default on. This is what shows changes made on
    /// another device on a screen that stayed open; see the
    /// [crate docs](crate#several-devices).
    pub fn revalidate_on_focus(mut self, on: bool) -> Self {
        self.revalidate_on_focus = on;
        self
    }

    /// The least time between two refetches caused by focus. Default 30 s,
    /// so switching windows back and forth doesn't refetch every time.
    pub fn focus_gap(mut self, duration: Duration) -> Self {
        self.focus_gap = duration;
        self
    }
}

static CONFIG: OnceLock<CacheConfig> = OnceLock::new();

pub(crate) fn config() -> &'static CacheConfig {
    CONFIG.get_or_init(|| CacheConfig::new("g3-cache"))
}

/// Sets up the client cache. Call once, at the top of the app's root
/// component, before any cached read.
///
/// ```ignore
/// fn App() -> Element {
///     g3_cache::use_client_cache(g3_cache::CacheConfig::new("media-mancer"));
///     // ...
/// }
/// ```
///
/// Without it, reads are still cached with default settings, but a store
/// named `g3-cache` is shared with any other g3 app on the same origin, and
/// nothing refetches on focus.
///
/// Persisting reads to disk also needs [`set_cache_owner`]: until the app has
/// said whose data this is, the cache keeps everything in memory only.
pub fn use_client_cache(config: CacheConfig) {
    use_hook(move || {
        // The server renders the root component once per request, and has
        // no client cache to configure.
        if !ENABLED {
            return;
        }
        if CONFIG.set(config).is_err() {
            tracing::warn!(
                "g3-cache: `use_client_cache` ran more than once, or after the first cached \
                 read; the later settings are ignored. Call it once, first thing in the root \
                 component."
            );
        }
        if self::config().revalidate_on_focus {
            spawn(revalidate_on_focus());
        }
    });
}

/// Refetches every mounted read whenever the app comes back into view.
/// Page visibility and focus events reach the webview on mobile as well as
/// the browser, so one listener covers every client.
async fn revalidate_on_focus() {
    const SCRIPT: &str = r#"
        const send = () => dioxus.send(null);
        document.addEventListener("visibilitychange", () => {
            if (document.visibilityState === "visible") send();
        });
        window.addEventListener("focus", send);
        window.addEventListener("online", send);
        await new Promise(() => {});
    "#;
    let mut events = document::eval(SCRIPT);
    let mut last = now_ms();
    while events.recv::<()>().await.is_ok() {
        let gap = i64::try_from(config().focus_gap.as_millis()).unwrap_or(i64::MAX);
        if now_ms().saturating_sub(last) >= gap {
            last = now_ms();
            invalidate_all_cached();
        }
    }
}

struct MemoryEntry {
    value: Rc<dyn Any>,
    fetched_at: i64,
}

thread_local! {
    static MEMORY: RefCell<BTreeMap<CacheKey, MemoryEntry>> = const { RefCell::new(BTreeMap::new()) };
    /// Whether this run has called `set_cache_owner`, which allows writing to the
    /// persistent store.
    static OWNER_SET: Cell<bool> = const { Cell::new(false) };
    /// Whether the persistent store holds a signed-in owner's data, which
    /// allows reading it. Checked once, on the first restore.
    static STORE_OWNED: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Bumped by every invalidation and by [`set_cache_owner`]; every cached read
/// subscribes to it, so each rechecks its entry (cheaply, unless the entry
/// was invalidated).
pub(crate) static EPOCH: GlobalSignal<u64> = Signal::global(|| 0);

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn remembered<T: Clone + 'static>(key: &CacheKey) -> Option<(T, i64)> {
    if !ENABLED {
        return None;
    }
    MEMORY.with_borrow(|memory| {
        let entry = memory.get(key)?;
        let value = entry.value.downcast_ref::<T>()?.clone();
        Some((value, entry.fetched_at))
    })
}

pub(crate) fn remember<T: 'static>(key: &CacheKey, value: T, fetched_at: i64) {
    if ENABLED {
        let entry = MemoryEntry {
            value: Rc::new(value),
            fetched_at,
        };
        MEMORY.with_borrow_mut(|memory| memory.insert(key.clone(), entry));
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Stored<T> {
    saved_at: i64,
    value: T,
}

pub(crate) async fn restore<T: serde::de::DeserializeOwned>(key: &CacheKey) -> Option<T> {
    if !ENABLED || !store_owned().await {
        return None;
    }
    let stored: Stored<T> = serde_json::from_str(&store::load(&key.storage_key()).await?).ok()?;
    let keep_for = i64::try_from(config().keep_for.as_millis()).unwrap_or(i64::MAX);
    (now_ms().saturating_sub(stored.saved_at) < keep_for).then_some(stored.value)
}

pub(crate) async fn persist<T: Serialize>(key: &CacheKey, value: &T) {
    if !ENABLED || !OWNER_SET.get() {
        return;
    }
    let stored = Stored {
        saved_at: now_ms(),
        value,
    };
    if let Ok(json) = serde_json::to_string(&stored) {
        store::save(&key.storage_key(), &json).await;
    }
}

async fn store_owned() -> bool {
    if let Some(owned) = STORE_OWNED.get() {
        return owned;
    }
    let owned = store::load(OWNER_KEY)
        .await
        .is_some_and(|owner| !owner.is_empty());
    STORE_OWNED.set(Some(owned));
    owned
}

/// Tells the cache whose data it holds: the signed-in user's id, or `None`
/// when signed out. Call it whenever the signed-in user is known or changes,
/// including on sign-out.
///
/// When the owner differs from the one the persistent store was written
/// for, or is `None`, the whole cache (memory and store) is emptied, so one
/// person never sees another's lists on a shared device.
///
/// Pass `None` only once you know nobody is signed in, never while still
/// finding out: it empties the cache, so an app that says "nobody" before
/// its session check answers starts every launch cold. Resolve the user on
/// the server (`use_server_future`), or wait for the check.
///
/// Until this is first called, the cache **keeps everything in memory
/// only**. An app that never calls it is still safe, just without instant
/// cold starts. A store written in an earlier run is still read on a cold
/// start before this is called: it only ever holds the data of whoever was
/// signed in last, and this empties it if that is no longer the case.
pub async fn set_cache_owner(owner: Option<String>) {
    if !ENABLED {
        return;
    }
    OWNER_SET.set(true);
    let owner = owner.unwrap_or_default();
    if !owner.is_empty() && store::load(OWNER_KEY).await.as_deref() == Some(owner.as_str()) {
        STORE_OWNED.set(Some(true));
        return;
    }
    MEMORY.with_borrow_mut(BTreeMap::clear);
    store::clear().await;
    store::save(OWNER_KEY, &owner).await;
    STORE_OWNED.set(Some(!owner.is_empty()));
    *EPOCH.write() += 1;
}

fn mark_stale(matches: impl Fn(&CacheKey) -> bool) {
    if !ENABLED {
        return;
    }
    MEMORY.with_borrow_mut(|memory| {
        for (key, entry) in memory.iter_mut() {
            if matches(key) {
                entry.fetched_at = STALE;
            }
        }
    });
    *EPOCH.write() += 1;
}

/// Refetches every cached call of `server_fn`, whatever its arguments.
///
/// Call it on the client after a mutation succeeds, once for each read the
/// mutation changes. Mounted screens refetch and keep showing their old
/// value until the new one arrives; unmounted ones refetch when they next
/// open.
///
/// ```ignore
/// save_rating(media_id.clone(), score).await?;
/// g3_cache::invalidate_cached(get_my_rating);
/// g3_cache::invalidate_cached(get_my_ratings);
/// g3_cache::invalidate_cached(get_profile_stats);
/// ```
///
/// Only the mutation knows which reads it affects, so this cannot be
/// automatic. Forgetting a read leaves it stale until the next screen open
/// or focus. When unsure, [`invalidate_all_cached`] is always correct, only
/// slower.
///
/// This does not reach the server's caches or Cloudflare. Reads cached
/// there (see [`cache_shared`](crate::cache_shared)) are refetched through them, and
/// may come back just as old.
pub fn invalidate_cached<F, Args>(server_fn: F)
where
    F: CacheableFn<Args>,
{
    let _ = server_fn;
    let name = crate::key::fn_name::<F>();
    mark_stale(|key| key.name() == name);
}

/// Refetches one cached call: `server_fn` with exactly these `args`.
///
/// ```ignore
/// g3_cache::invalidate_cached_call(get_list_items, (list_id.clone(),));
/// ```
pub fn invalidate_cached_call<F, Args>(server_fn: F, args: Args)
where
    F: CacheableFn<Args>,
    Args: Serialize,
{
    let target = CacheKey::of(&server_fn, &args);
    mark_stale(|key| *key == target);
}

/// Refetches the read cached under exactly `key`.
pub fn invalidate_cached_key(key: &CacheKey) {
    mark_stale(|candidate| candidate == key);
}

/// Refetches every read cached under a [`CacheKey`] named `name`, whatever
/// its arguments.
pub fn invalidate_cached_name(name: &str) {
    mark_stale(|key| key.name() == name);
}

/// Refetches every cached read. Always correct after a mutation, but every
/// mounted screen refetches everything it shows; prefer [`invalidate_cached`] for
/// the reads the mutation changes.
pub fn invalidate_all_cached() {
    mark_stale(|_| true);
}

/// Edits the cached answer of one call, `server_fn` with exactly these
/// `args`, in place, so a change shows at once rather than after the
/// mutation's round trip. Returns whether there was an answer to edit.
///
/// ```ignore
/// g3_cache::update_cached(get_playlist, (id.clone(),), |playlist| {
///     playlist.videos.retain(|video| video.id != removed);
/// });
/// remove_from_playlist(id.clone(), removed).await?;
/// g3_cache::invalidate_cached_call(get_playlist, (id,));
/// ```
///
/// Mounted screens show the edit without refetching. It is memory only: the
/// persistent store keeps the server's last answer, so **follow the
/// mutation with an invalidation**, whether it succeeded or not. The
/// refetch replaces the guess with what the server holds, which also undoes
/// it if the mutation failed.
pub fn update_cached<F, Args>(server_fn: F, args: Args, update: impl FnOnce(&mut F::Output)) -> bool
where
    F: CacheableFn<Args>,
    Args: Serialize,
    F::Output: Clone + 'static,
{
    update_cached_key(&CacheKey::of(&server_fn, &args), update)
}

/// [`update_cached`] for every cached call of `server_fn`, whatever its
/// arguments: a video marked watched in every cached page of a feed, say.
/// Returns how many answers were edited.
pub fn update_all_cached<F, Args>(server_fn: F, mut update: impl FnMut(&mut F::Output)) -> usize
where
    F: CacheableFn<Args>,
    F::Output: Clone + 'static,
{
    let _ = server_fn;
    let name = crate::key::fn_name::<F>();
    edit_entries(|key| key.name() == name, &mut update)
}

/// [`update_cached`] for a read cached under a [`CacheKey`] you named.
pub fn update_cached_key<T: Clone + 'static>(key: &CacheKey, update: impl FnOnce(&mut T)) -> bool {
    let mut update = Some(update);
    edit_entries(|candidate| candidate == key, &mut |value: &mut T| {
        if let Some(update) = update.take() {
            update(value);
        }
    }) > 0
}

fn edit_entries<T: Clone + 'static>(
    matches: impl Fn(&CacheKey) -> bool,
    update: &mut dyn FnMut(&mut T),
) -> usize {
    if !ENABLED {
        return 0;
    }
    let edited = MEMORY.with_borrow_mut(|memory| {
        let mut edited = 0;
        for (key, entry) in memory.iter_mut() {
            if !matches(key) {
                continue;
            }
            let Some(current) = entry.value.downcast_ref::<T>() else {
                continue;
            };
            // Copied rather than edited through the `Rc`: a screen still
            // holding the previous value must see a change arrive, not have
            // its value altered underneath it.
            let mut value = current.clone();
            update(&mut value);
            entry.value = Rc::new(value);
            edited += 1;
        }
        edited
    });
    if edited > 0 {
        // Mounted reads recheck their entry. It keeps its `fetched_at`, so a
        // fresh one shows the edit without refetching and a stale one still
        // refetches.
        *EPOCH.write() += 1;
    }
    edited
}

#[cfg(all(test, not(feature = "server")))]
mod tests {
    use super::*;
    use dioxus::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FETCHES: AtomicUsize = AtomicUsize::new(0);

    fn key() -> CacheKey {
        CacheKey::new("numbers")
    }

    #[derive(Props, Clone, PartialEq)]
    struct Shown {
        shown: Signal<Option<Vec<u32>>>,
        pending: Signal<bool>,
    }

    #[component]
    fn Reader(props: Shown) -> Element {
        let Shown {
            mut shown,
            mut pending,
        } = props;
        let numbers = use_cached_key(key(), || async {
            FETCHES.fetch_add(1, Ordering::SeqCst);
            Ok(vec![1, 2, 3])
        });
        let value = numbers
            .read()
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .cloned();
        if *shown.peek() != value {
            shown.set(value);
        }
        if *pending.peek() != numbers.pending() {
            pending.set(numbers.pending());
        }
        rsx! {}
    }

    async fn settle(dom: &mut VirtualDom) {
        for _ in 0..20 {
            tokio::select! {
                () = dom.wait_for_work() => { dom.render_immediate(&mut dioxus_core::NoOpMutations); }
                () = tokio::time::sleep(std::time::Duration::from_millis(20)) => return,
            }
        }
    }

    /// One test, in order: the steps share the thread-local cache.
    #[tokio::test(flavor = "current_thread")]
    async fn an_update_shows_at_once_and_an_invalidation_reconciles_it() {
        let mut dom = VirtualDom::new(|| {
            let shown = use_signal(|| None);
            let pending = use_signal(|| false);
            use_context_provider(|| (shown, pending));
            rsx! { Reader { shown, pending } }
        });
        dom.rebuild_in_place();
        settle(&mut dom).await;
        let (shown, pending) = dom.in_scope(ScopeId::APP, || {
            consume_context::<(Signal<Option<Vec<u32>>>, Signal<bool>)>()
        });
        assert_eq!(dom.in_runtime(|| shown()), Some(vec![1, 2, 3]));
        assert_eq!(FETCHES.load(Ordering::SeqCst), 1);
        assert!(!dom.in_runtime(|| pending()));

        let edited =
            dom.in_runtime(|| update_cached_key(&key(), |numbers: &mut Vec<u32>| numbers.push(4)));
        assert!(edited);
        settle(&mut dom).await;
        assert_eq!(dom.in_runtime(|| shown()), Some(vec![1, 2, 3, 4]));
        assert_eq!(
            FETCHES.load(Ordering::SeqCst),
            1,
            "an update must not refetch"
        );

        // A key nothing has cached: nothing to edit, and no wasted rerun.
        assert!(
            !dom.in_runtime(|| update_cached_key(&CacheKey::new("absent"), |_: &mut Vec<u32>| {}))
        );

        dom.in_runtime(|| invalidate_cached_key(&key()));
        settle(&mut dom).await;
        assert_eq!(
            dom.in_runtime(|| shown()),
            Some(vec![1, 2, 3]),
            "the refetch replaces the guess"
        );
        assert_eq!(FETCHES.load(Ordering::SeqCst), 2);
        assert!(!dom.in_runtime(|| pending()));
    }
}
