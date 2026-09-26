//! CDN caching: whole responses kept by a shared HTTP cache (Cloudflare,
//! CloudFront, Fastly, a reverse proxy) and served to every visitor without
//! reaching the server. See the [crate docs](crate#cdn-cache) for when to
//! use it.
//!
//! Two layers:
//!
//! - [`cdn_cache_for`] marks one server function's successful responses
//!   `public`. Put it on with [`cache_shared`](crate::cache_shared)`(cdn = 300)`,
//!   which also checks the function is safe to share, rather than with
//!   `#[middleware(..)]` directly.
//! - [`cdn_cache_guard`] goes on the whole router, once. It keeps a session cookie
//!   out of every shared response, and marks every other API response
//!   `private`, so nothing is shared by accident.
//!
//! ```ignore
//! let router = dioxus::server::router(App)
//!     .layer(session_layer)
//!     // Outside the session layer, so it sees the cookie that layer adds.
//!     .layer(g3_cache::cdn_cache_guard("/api"));
//! ```
//!
//! # Providers
//!
//! Only standard `Cache-Control` directives are sent (`public`, `private`,
//! `no-cache`, `max-age`, `s-maxage`), so any CDN or shared cache that
//! follows HTTP caching works. Providers differ in whether they cache API
//! responses at all without being told to:
//!
//! | Provider | Honors `s-maxage` | Also needed |
//! |---|---|---|
//! | Cloudflare | Yes | A Cache Rule making the API paths "Eligible for cache"; by default it caches only static file extensions |
//! | CloudFront | Yes | A cache policy whose TTL range allows it, with the query string in the cache key |
//! | Fastly | Yes | Nothing (`Surrogate-Control` takes precedence when present) |
//! | Vercel, Netlify | Yes | Nothing |
//! | nginx, Varnish | Yes | Caching turned on for the API location |
//!
//! Without that setup nothing is cached at the CDN, which is safe: the
//! responses are still correct, just not shared.
//!
//! Nothing clears a CDN cache when data changes; entries only expire. Each
//! provider has its own purge API, and this crate calls none of them. A
//! client [`invalidate_cached`](crate::invalidate_cached) refetches
//! *through* the CDN and may get the same answer back until it expires.

use axum::http::{
    HeaderMap, HeaderValue, Method, Request, Response, StatusCode,
    header::{CACHE_CONTROL, SET_COOKIE},
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower_layer::Layer;
use tower_service::Service;

/// How long a browser keeps a shared response, unless set with
/// [`CdnCacheFor::browser`]: one minute, so a browser tab rechecks sooner
/// than the CDN does.
const DEFAULT_BROWSER_SECS: u32 = 60;

/// Marks a server function's successful `GET` responses as the same for
/// every visitor: `Cache-Control: public, max-age=60, s-maxage=<cdn_secs>`.
///
/// - Only `2xx` responses are marked, so an error is never cached.
/// - Only `GET` and `HEAD` requests are marked; a CDN never caches others.
/// - The browser keeps its copy for at most a minute
///   ([`browser`](CdnCacheFor::browser) to change).
///
/// Prefer [`cache_shared`](crate::cache_shared)`(cdn = ..)`, which adds this layer and
/// refuses functions that read the session or are not `GET`.
pub fn cdn_cache_for(cdn_secs: u32) -> CdnCacheFor {
    CdnCacheFor {
        cdn_secs,
        browser_secs: DEFAULT_BROWSER_SECS.min(cdn_secs),
    }
}

/// The layer [`cdn_cache_for`] returns.
#[derive(Clone, Copy, Debug)]
pub struct CdnCacheFor {
    cdn_secs: u32,
    browser_secs: u32,
}

impl CdnCacheFor {
    /// How long a browser keeps its copy (`max-age`). Capped at the CDN
    /// lifetime.
    pub fn browser(mut self, secs: u32) -> Self {
        self.browser_secs = secs.min(self.cdn_secs);
        self
    }

    fn header(&self) -> HeaderValue {
        let value = format!(
            "public, max-age={}, s-maxage={}",
            self.browser_secs, self.cdn_secs
        );
        HeaderValue::from_str(&value).expect("digits and ASCII are a valid header value")
    }
}

impl<S> Layer<S> for CdnCacheFor {
    type Service = CdnCacheForService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CdnCacheForService {
            inner,
            policy: *self,
        }
    }
}

/// The service [`CdnCacheFor`] wraps around a route.
#[derive(Clone, Debug)]
pub struct CdnCacheForService<S> {
    inner: S,
    policy: CdnCacheFor,
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl<S, B, ResBody> Service<Request<B>> for CdnCacheForService<S>
where
    S: Service<Request<B>, Response = Response<ResBody>>,
    S::Future: Send + 'static,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let cacheable = matches!(*request.method(), Method::GET | Method::HEAD);
        let policy = self.policy;
        let response = self.inner.call(request);
        Box::pin(async move {
            let mut response = response.await?;
            mark_shared(policy, cacheable, response.status(), response.headers_mut());
            Ok(response)
        })
    }
}

fn mark_shared(policy: CdnCacheFor, cacheable: bool, status: StatusCode, headers: &mut HeaderMap) {
    if cacheable && status.is_success() {
        headers.insert(CACHE_CONTROL, policy.header());
    }
}

/// The router-wide safety layer for CDN caching. Add it once, outside the
/// session layer, whether or not any function is shared yet:
///
/// - A response marked `public` loses any `Set-Cookie`. The session layer
///   can attach a cookie to any response, and a cached copy carrying one
///   would hand that visitor's session to everyone who gets the copy.
/// - A response to a path under `api_prefix` with no `Cache-Control` gets
///   `private, no-cache`, so no browser or CDN reuses per-user data, and a
///   client refetch after [`invalidate_cached`](crate::invalidate_cached) always reaches
///   the server. Pages and static assets, outside the prefix, keep
///   whatever headers they had.
pub fn cdn_cache_guard(api_prefix: &'static str) -> CdnCacheGuard {
    CdnCacheGuard { api_prefix }
}

/// The layer [`cdn_cache_guard`] returns.
#[derive(Clone, Copy, Debug)]
pub struct CdnCacheGuard {
    api_prefix: &'static str,
}

impl<S> Layer<S> for CdnCacheGuard {
    type Service = CdnCacheGuardService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CdnCacheGuardService {
            inner,
            api_prefix: self.api_prefix,
        }
    }
}

/// The service [`CdnCacheGuard`] wraps around a router.
#[derive(Clone, Debug)]
pub struct CdnCacheGuardService<S> {
    inner: S,
    api_prefix: &'static str,
}

impl<S, B, ResBody> Service<Request<B>> for CdnCacheGuardService<S>
where
    S: Service<Request<B>, Response = Response<ResBody>>,
    S::Future: Send + 'static,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let api = under_prefix(request.uri().path(), self.api_prefix);
        let response = self.inner.call(request);
        Box::pin(async move {
            let mut response = response.await?;
            apply_guard(api, response.headers_mut());
            Ok(response)
        })
    }
}

fn under_prefix(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix.trim_end_matches('/'))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn is_shared(headers: &HeaderMap) -> bool {
    headers
        .get_all(CACHE_CONTROL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|part| part.trim().eq_ignore_ascii_case("public"))
}

fn apply_guard(api: bool, headers: &mut HeaderMap) {
    if is_shared(headers) {
        headers.remove(SET_COOKIE);
    } else if api && !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_cookie(cache_control: Option<&'static str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(SET_COOKIE, HeaderValue::from_static("session=abc"));
        if let Some(value) = cache_control {
            headers.insert(CACHE_CONTROL, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn shared_responses_never_carry_a_cookie() {
        let mut headers = with_cookie(Some("public, max-age=60, s-maxage=300"));
        apply_guard(true, &mut headers);
        assert!(headers.get(SET_COOKIE).is_none());
    }

    #[test]
    fn other_api_responses_are_private_and_keep_their_cookie() {
        let mut headers = with_cookie(None);
        apply_guard(true, &mut headers);
        assert!(headers.get(SET_COOKIE).is_some());
        assert_eq!(headers[CACHE_CONTROL], "private, no-cache");

        let mut explicit = with_cookie(Some("no-store"));
        apply_guard(true, &mut explicit);
        assert_eq!(explicit[CACHE_CONTROL], "no-store");
    }

    #[test]
    fn pages_and_assets_keep_their_headers() {
        let mut headers = with_cookie(None);
        apply_guard(false, &mut headers);
        assert!(headers.get(CACHE_CONTROL).is_none());
    }

    #[test]
    fn prefixes_match_whole_segments() {
        assert!(under_prefix("/api/v1/trending", "/api"));
        assert!(under_prefix("/api", "/api/"));
        assert!(!under_prefix("/apiary", "/api"));
        assert!(!under_prefix("/assets/app.js", "/api"));
    }

    #[test]
    fn only_successful_gets_are_marked() {
        let policy = cdn_cache_for(300);
        let mut ok = HeaderMap::new();
        mark_shared(policy, true, StatusCode::OK, &mut ok);
        assert_eq!(ok[CACHE_CONTROL], "public, max-age=60, s-maxage=300");

        let mut error = HeaderMap::new();
        mark_shared(policy, true, StatusCode::INTERNAL_SERVER_ERROR, &mut error);
        assert!(error.get(CACHE_CONTROL).is_none());

        let mut post = HeaderMap::new();
        mark_shared(policy, false, StatusCode::OK, &mut post);
        assert!(post.get(CACHE_CONTROL).is_none());
    }

    #[test]
    fn the_browser_never_keeps_a_copy_longer_than_the_cdn() {
        assert_eq!(
            cdn_cache_for(30).header(),
            "public, max-age=30, s-maxage=30"
        );
        assert_eq!(
            cdn_cache_for(300).browser(600).header(),
            "public, max-age=300, s-maxage=300"
        );
    }
}
