//! Naming cached reads: [`CacheKey`], and [`CacheableFn`], which lets a server
//! function stand for its own key.

use serde::Serialize;
use std::{borrow::Cow, fmt, future::Future};

/// Names one cached read: a `name` for what is read, plus the arguments its
/// answer depends on.
///
/// You rarely build one yourself. [`use_cached`](crate::use_cached)
/// and [`invalidate_cached`](crate::invalidate_cached) derive it from the server function
/// and its arguments, so the key cannot miss an argument. Build one by hand
/// only for a read that is not a single server function call, for
/// [`use_cached_key`](crate::use_cached_key):
///
/// ```
/// use g3_cache::CacheKey;
///
/// let key = CacheKey::new("home_shelves").with("movie").with(3);
/// assert_eq!(key.name(), "home_shelves");
/// ```
///
/// [`invalidate_cached_name`](crate::invalidate_cached_name) matches every key with a
/// given name; [`invalidate_cached_key`](crate::invalidate_cached_key) matches one key
/// exactly.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CacheKey {
    name: Cow<'static, str>,
    args: String,
}

impl CacheKey {
    /// A key with no arguments yet. Add each thing the read depends on with
    /// [`with`](Self::with).
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: name.into(),
            args: String::new(),
        }
    }

    /// Adds an argument the read depends on. Any `Serialize` value works;
    /// it is stored as JSON.
    ///
    /// # Panics
    ///
    /// If `part` cannot be serialized to JSON (a map with non-string keys,
    /// for example). That is a bug in the key, not a runtime condition.
    pub fn with(mut self, part: impl Serialize) -> Self {
        let json = serde_json::to_string(&part)
            .unwrap_or_else(|err| panic!("g3-cache: a key argument must serialize to JSON: {err}"));
        if !self.args.is_empty() {
            self.args.push(',');
        }
        self.args.push_str(&json);
        self
    }

    /// The key for calling `server_fn` with `args`: the function's path, and
    /// the arguments as JSON. Arguments are a tuple, as for
    /// [`use_cached`](crate::use_cached): `()`, `(id,)`,
    /// `(id, page)`.
    ///
    /// # Panics
    ///
    /// If `server_fn` is a closure; see [`CacheableFn`].
    pub fn of<F, Args>(server_fn: &F, args: &Args) -> Self
    where
        F: CacheableFn<Args>,
        Args: Serialize,
    {
        let _ = server_fn;
        let args = serde_json::to_string(args).unwrap_or_else(|err| {
            panic!("g3-cache: server function arguments must serialize to JSON: {err}")
        });
        Self {
            name: Cow::Borrowed(fn_name::<F>()),
            args,
        }
    }

    /// What is read: the server function's path, or the name given to
    /// [`new`](Self::new).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The single string the persistent store files this key under.
    pub(crate) fn storage_key(&self) -> String {
        format!("{}\u{1f}{}", self.name, self.args)
    }
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CacheKey({}({}))", self.name, self.args)
    }
}

/// The name of a server function item, used as its cache key's name.
///
/// A function item's type name is its path (`my_app::db::get_media`), which
/// is unique and stable for a build. A closure's is not: every closure in a
/// module shares `{{closure}}` names that say nothing of what it fetches,
/// so two different closures could read each other's cached answers. They
/// are refused, loudly, the first time they are used.
pub(crate) fn fn_name<F>() -> &'static str {
    let name = std::any::type_name::<F>();
    assert!(
        !name.contains("{{closure}}"),
        "g3-cache: `{name}` is a closure. Pass the server function itself, with its \
         arguments as a tuple: `use_cached(get_media, (id,))`, not \
         `use_cached(|| get_media(id), ())`. For a read that is not one \
         server function call, use `use_cached_key` with a `CacheKey`."
    );
    name
}

/// A server function (or any `async fn` returning `dioxus::Result`) that can
/// be passed by name to [`use_cached`](crate::use_cached) and
/// [`invalidate_cached`](crate::invalidate_cached).
///
/// Implemented for functions of up to eight arguments. `Args` is the
/// arguments as a tuple, which Rust infers from the function:
///
/// ```ignore
/// async fn get_bookmarks() -> dioxus::Result<Vec<Media>>;          // Args = ()
/// async fn get_media(id: String) -> dioxus::Result<Media>;          // Args = (String,)
/// async fn get_reviews(id: String, page: u32) -> dioxus::Result<_>; // Args = (String, u32)
/// ```
///
/// Only pass function *items*. A closure compiles, but panics on first use:
/// its type name cannot tell two different reads apart.
pub trait CacheableFn<Args>: 'static {
    /// The `Ok` value.
    type Output;
    /// The future the function returns.
    type Future: Future<Output = dioxus::Result<Self::Output>> + 'static;

    /// Calls the function with its arguments as a tuple.
    fn call_with(&self, args: Args) -> Self::Future;
}

macro_rules! impl_server_fn {
    ($($arg:ident),*) => {
        impl<F, Fut, T, $($arg),*> CacheableFn<($($arg,)*)> for F
        where
            F: Fn($($arg),*) -> Fut + 'static,
            Fut: Future<Output = dioxus::Result<T>> + 'static,
        {
            type Output = T;
            type Future = Fut;

            #[allow(non_snake_case)]
            fn call_with(&self, ($($arg,)*): ($($arg,)*)) -> Fut {
                (self)($($arg),*)
            }
        }
    };
}

impl_server_fn!();
impl_server_fn!(A);
impl_server_fn!(A, B);
impl_server_fn!(A, B, C);
impl_server_fn!(A, B, C, D);
impl_server_fn!(A, B, C, D, E);
impl_server_fn!(A, B, C, D, E, G);
impl_server_fn!(A, B, C, D, E, G, H);
impl_server_fn!(A, B, C, D, E, G, H, I);

#[cfg(test)]
mod tests {
    use super::*;

    async fn get_bookmarks() -> dioxus::Result<Vec<u8>> {
        Ok(vec![])
    }

    async fn get_media(_id: String) -> dioxus::Result<u8> {
        Ok(1)
    }

    async fn get_reviews(_id: String, _offset: u32, _limit: u32) -> dioxus::Result<u8> {
        Ok(1)
    }

    #[test]
    fn server_fns_name_their_own_keys() {
        let key = CacheKey::of(&get_bookmarks, &());
        assert!(key.name().ends_with("tests::get_bookmarks"));

        let a = CacheKey::of(&get_media, &("tt1".to_string(),));
        let b = CacheKey::of(&get_media, &("tt2".to_string(),));
        assert_eq!(a.name(), b.name());
        assert_ne!(a, b);

        let reviews = CacheKey::of(&get_reviews, &("tt1".to_string(), 0, 21));
        assert_eq!(reviews.args, r#"["tt1",0,21]"#);
    }

    #[test]
    fn different_functions_never_share_a_key() {
        let media = CacheKey::of(&get_media, &("tt1".to_string(),));
        let reviews = CacheKey::of(&get_reviews, &("tt1".to_string(), 0, 21));
        assert_ne!(media.name(), reviews.name());
    }

    #[test]
    #[should_panic(expected = "is a closure")]
    fn closures_are_refused() {
        let id = "tt1".to_string();
        let closure = move || get_media(id.clone());
        let _ = CacheKey::of(&closure, &());
    }

    #[test]
    fn hand_built_keys_add_arguments_in_order() {
        let key = CacheKey::new("shelf").with("movie").with(Some(3));
        assert_eq!(key.args, r#""movie",3"#);
        assert_ne!(key, CacheKey::new("shelf").with(Some(3)).with("movie"));
    }
}
