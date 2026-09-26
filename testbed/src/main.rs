//! Exercises g3-cache's cache against real Dioxus server functions:
//!
//! - `dx serve --web` to click through the client cache;
//! - `curl -i localhost:8080/api/trending` twice: the header is `public`,
//!   and `calls` stays at 1 because the server cache answered the second;
//! - `curl -i "localhost:8080/api/title?id=a"`: `private, no-cache` from the
//!   guard, and `looked_up` stays at 1 across ids thanks to `ServerCache`.

use dioxus::prelude::*;
use g3_cache::{
    CacheConfig, cache_shared, invalidate_cached, set_cache_owner, use_cached, use_client_cache,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
pub struct Trending {
    pub titles: Vec<String>,
    /// How many times the function body has run: the server cache keeps
    /// this at 1 until its entry expires.
    pub calls: u32,
}

#[cache_shared(cdn = 300, server = "5m")]
#[get("/api/trending?kind")]
pub async fn get_trending(kind: Option<String>) -> Result<Trending> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static CALLS: AtomicU32 = AtomicU32::new(0);
    let calls = CALLS.fetch_add(1, Ordering::SeqCst) + 1;
    let titles = match kind.as_deref() {
        Some("book") => vec!["Dune".to_string()],
        _ => vec!["Arrival".to_string(), "Dune".to_string()],
    };
    Ok(Trending { titles, calls })
}

/// A per-call answer (not shared), with its expensive part cached on the
/// server: the "outside API" below runs once per hour, whatever the id.
#[get("/api/title?id")]
pub async fn get_title(id: String) -> Result<String> {
    use g3_cache::ServerCache;
    use std::{
        sync::atomic::{AtomicU32, Ordering},
        time::Duration,
    };
    static CATALOG_SIZE: ServerCache<(), u32> = ServerCache::new(Duration::from_secs(60 * 60), 1);
    static LOOKUPS: AtomicU32 = AtomicU32::new(0);

    let size = CATALOG_SIZE
        .get_or_insert((), async { LOOKUPS.fetch_add(1, Ordering::SeqCst) + 1000 })
        .await;
    let looked_up = LOOKUPS.load(Ordering::SeqCst);
    Ok(format!("title {id} of {size} (looked_up: {looked_up})"))
}

#[post("/api/rate")]
pub async fn rate(id: String, score: u8) -> Result<()> {
    let _ = (id, score);
    Ok(())
}

fn main() {
    #[cfg(feature = "server")]
    dioxus::serve(|| async move {
        Ok(dioxus::server::router(App).layer(g3_cache::cdn_cache_guard("/api")))
    });

    #[cfg(not(feature = "server"))]
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    use_client_cache(CacheConfig::new("g3-cache-testbed"));
    use_hook(|| spawn(set_cache_owner(Some("testbed-user".to_string()))));

    let trending = use_cached(get_trending, (None,));
    let title = use_cached(get_title, ("a".to_string(),));

    rsx! {
        h1 { "g3-cache testbed" }
        p { "trending: {trending.read():?}" }
        p { "title: {title.read():?}" }
        button {
            onclick: move |_| async move {
                if rate("a".to_string(), 5).await.is_ok() {
                    invalidate_cached(get_title);
                }
            },
            "Rate, then invalidate get_title"
        }
        button { onclick: move |_| trending.refresh(), "Refresh trending" }
    }
}
