use proc_macro2::{TokenStream as TokenStream2, TokenTree};
use quote::{quote, quote_spanned};
use syn::{
    Attribute, Error, FnArg, GenericArgument, ItemFn, Lit, Meta, Pat, PathArguments, ReturnType,
    Token, Type, parse::Parser, punctuated::Punctuated, spanned::Spanned,
};

/// Names that suggest an extractor carries per-visitor state.
const VIEWER_WORDS: &[&str] = &[
    "session",
    "auth",
    "user",
    "cookie",
    "token",
    "jwt",
    "claims",
    "header",
    "identity",
    "viewer",
    "account",
    "login",
    "principal",
    "bearer",
];

const ROUTE_ATTRS: &[&str] = &["get", "post", "put", "patch", "delete", "server"];

struct Args {
    cdn: Option<u32>,
    server: Option<u64>,
    capacity: u64,
    trust_extractors: bool,
}

fn parse_args(attr: TokenStream2) -> syn::Result<Args> {
    let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr.clone())?;
    let mut args = Args {
        cdn: None,
        server: None,
        capacity: 10_000,
        trust_extractors: false,
    };
    for meta in metas {
        let name = meta
            .path()
            .get_ident()
            .map(ToString::to_string)
            .unwrap_or_default();
        match (name.as_str(), &meta) {
            ("trust_extractors", Meta::Path(_)) => args.trust_extractors = true,
            ("cdn", Meta::NameValue(nv)) => {
                let secs: u32 = int_value(&nv.value, "cdn = <seconds>")?;
                if secs == 0 {
                    return Err(Error::new(
                        nv.value.span(),
                        "`cdn` must be at least 1 second",
                    ));
                }
                args.cdn = Some(secs);
            }
            ("server", Meta::NameValue(nv)) => args.server = Some(duration_value(&nv.value)?),
            ("capacity", Meta::NameValue(nv)) => {
                args.capacity = int_value(&nv.value, "capacity = <entries>")?;
                if args.capacity == 0 {
                    return Err(Error::new(nv.value.span(), "`capacity` must be at least 1"));
                }
            }
            _ => {
                return Err(Error::new(
                    meta.span(),
                    "expected `cdn = <seconds>`, `server = \"<duration>\"`, \
                     `capacity = <entries>` or `trust_extractors`",
                ));
            }
        }
    }
    if args.cdn.is_none() && args.server.is_none() {
        return Err(Error::new(
            attr.span(),
            "say where to cache: `#[cache_shared(cdn = 300)]`, `#[cache_shared(server = \"5m\")]`, or both",
        ));
    }
    Ok(args)
}

fn int_value<N: std::str::FromStr>(expr: &syn::Expr, form: &str) -> syn::Result<N>
where
    N::Err: std::fmt::Display,
{
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: Lit::Int(int), ..
        }) => int.base10_parse(),
        _ => Err(Error::new(expr.span(), format!("expected `{form}`"))),
    }
}

/// Parses `"30s"`, `"5m"`, `"1h"` or `"1d"` into seconds.
fn duration_value(expr: &syn::Expr) -> syn::Result<u64> {
    let syn::Expr::Lit(syn::ExprLit {
        lit: Lit::Str(text),
        ..
    }) = expr
    else {
        return Err(Error::new(
            expr.span(),
            "expected a duration string: `server = \"5m\"`",
        ));
    };
    parse_duration(&text.value()).ok_or_else(|| {
        Error::new(
            text.span(),
            "expected a whole number followed by `s`, `m`, `h` or `d`, at least 1s: \"30s\", \"5m\", \"1h\"",
        )
    })
}

fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    let unit = text.chars().last()?;
    let count: u64 = text[..text.len() - unit.len_utf8()].trim().parse().ok()?;
    let secs = match unit {
        's' => count,
        'm' => count.checked_mul(60)?,
        'h' => count.checked_mul(60 * 60)?,
        'd' => count.checked_mul(24 * 60 * 60)?,
        _ => return None,
    };
    (secs > 0).then_some(secs)
}

fn route_name(attr: &Attribute) -> Option<String> {
    let name = attr.path().segments.last()?.ident.to_string();
    ROUTE_ATTRS.contains(&name.as_str()).then_some(name)
}

/// Every identifier in `tokens`, recursively.
fn idents(tokens: TokenStream2, out: &mut Vec<proc_macro2::Ident>) {
    for tree in tokens {
        match tree {
            TokenTree::Ident(ident) => out.push(ident),
            TokenTree::Group(group) => idents(group.stream(), out),
            _ => {}
        }
    }
}

/// The route attribute's tokens after its path: the extractors it binds.
fn extractor_tokens(route: &Attribute) -> TokenStream2 {
    let Meta::List(list) = &route.meta else {
        return TokenStream2::new();
    };
    list.tokens
        .clone()
        .into_iter()
        .skip_while(|tree| !matches!(tree, TokenTree::Punct(p) if p.as_char() == ','))
        .collect()
}

fn check_extractors(route: &Attribute) -> syn::Result<()> {
    let mut found = Vec::new();
    idents(extractor_tokens(route), &mut found);
    for ident in found {
        let lower = ident.to_string().to_lowercase();
        if let Some(word) = VIEWER_WORDS.iter().find(|word| lower.contains(*word)) {
            return Err(Error::new(
                ident.span(),
                format!(
                    "`{ident}` looks like per-visitor state (it mentions `{word}`), so this \
                     function's answer may depend on who asks. A shared answer is shown to every \
                     visitor. Use the client cache (`use_cached`) for per-user data. If \
                     this extractor does not depend on the visitor, add `trust_extractors`."
                ),
            ));
        }
    }
    Ok(())
}

fn check_body(function: &ItemFn) -> syn::Result<()> {
    let mut found = Vec::new();
    let block = &function.block;
    idents(quote!(#block), &mut found);
    match found.iter().find(|ident| *ident == "FullstackContext") {
        Some(ident) => Err(Error::new(
            ident.span(),
            "a shared function must not read the request through `FullstackContext`: its \
             answer would depend on who asks. Take what it needs as arguments instead.",
        )),
        None => Ok(()),
    }
}

/// `T` in a return type of `Result<T>`, `Result<T, E>` or `SomethingResult<T>`.
fn ok_type(output: &ReturnType) -> Option<Type> {
    let ReturnType::Type(_, ty) = output else {
        return None;
    };
    let Type::Path(path) = &**ty else {
        return None;
    };
    let last = path.path.segments.last()?;
    if !last.ident.to_string().ends_with("Result") {
        return None;
    }
    let PathArguments::AngleBracketed(generics) = &last.arguments else {
        return None;
    };
    generics.args.iter().find_map(|arg| match arg {
        GenericArgument::Type(ty) => Some(ty.clone()),
        _ => None,
    })
}

pub(crate) fn expand(attr: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    let args = parse_args(attr)?;
    let mut function: ItemFn = syn::parse2(item)?;

    let Some((route, method)) = function
        .attrs
        .iter()
        .find_map(|attr| route_name(attr).map(|name| (attr, name)))
    else {
        return Err(Error::new(
            function.sig.ident.span(),
            "`#[cache_shared]` must be placed above the route attribute, e.g. \
             `#[cache_shared(cdn = 300)]` then `#[get(\"/api/...\")]`. Below it, the route \
             attribute has already expanded and the function can no longer be cached.",
        ));
    };
    if method != "get" {
        let why = if method == "server" {
            "`#[server]` functions are POST"
        } else {
            "only GET reads are shared"
        };
        return Err(Error::new(
            route.span(),
            format!(
                "`#[cache_shared]` needs `#[get(..)]`: {why}, and a CDN never caches other methods. \
                 A function that changes data must not be cached."
            ),
        ));
    }
    if !args.trust_extractors {
        check_extractors(route)?;
    }
    check_body(&function)?;
    if function.sig.asyncness.is_none() {
        return Err(Error::new(
            function.sig.fn_token.span(),
            "`#[cache_shared]` functions must be `async`",
        ));
    }
    let Some(ok) = ok_type(&function.sig.output) else {
        return Err(Error::new(
            function.sig.output.span(),
            "`#[cache_shared]` functions must return a `Result<T>`, so errors can be kept out of the cache",
        ));
    };

    if let Some(secs) = args.server {
        let mut arg_names = Vec::new();
        for input in &function.sig.inputs {
            match input {
                FnArg::Typed(typed) => match &*typed.pat {
                    Pat::Ident(pat) => arg_names.push(pat.ident.clone()),
                    other => {
                        return Err(Error::new(
                            other.span(),
                            "`#[cache_shared(server = ..)]` needs plain argument names, to key the cache",
                        ));
                    }
                },
                FnArg::Receiver(receiver) => {
                    return Err(Error::new(
                        receiver.span(),
                        "`#[cache_shared]` cannot cache a method",
                    ));
                }
            }
        }
        let capacity = args.capacity;
        let body = function.block.clone();
        let span = function.block.span();
        *function.block = syn::parse2(quote_spanned! {span=> {
            static __G3_CACHE_SHARED: ::g3_cache::__private::SharedCache<#ok> =
                ::g3_cache::__private::SharedCache::new(::core::time::Duration::from_secs(#secs), #capacity);
            let __g3_cache_key = ::g3_cache::__private::args_key(&(#(&#arg_names,)*));
            __G3_CACHE_SHARED
                .get_or_fetch(__g3_cache_key, async move #body)
                .await
        }})?;
    }

    if let Some(secs) = args.cdn {
        function.attrs.push(syn::parse_quote! {
            #[middleware(::g3_cache::__private::cdn_cache_for(#secs))]
        });
    }

    Ok(quote!(#function))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(attr: TokenStream2, item: TokenStream2) -> String {
        expand(attr, item).unwrap_err().to_string()
    }

    fn trending(extractors: TokenStream2) -> TokenStream2 {
        quote! {
            #[get("/api/trending?media_type", #extractors)]
            pub async fn get_trending(media_type: Option<MediaType>) -> Result<Vec<Media>> {
                Ok(vec![])
            }
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s"), Some(30));
        assert_eq!(parse_duration("5m"), Some(300));
        assert_eq!(parse_duration("1h"), Some(3600));
        assert_eq!(parse_duration("2d"), Some(172_800));
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("5"), None);
        assert_eq!(parse_duration("m"), None);
        assert_eq!(parse_duration("5 minutes"), None);
    }

    #[test]
    fn cdn_adds_the_middleware_for_get_to_read() {
        let out = expand(quote!(cdn = 300), trending(quote!(db: Db)))
            .unwrap()
            .to_string();
        assert!(out.contains("middleware"));
        assert!(out.contains("cdn_cache_for (300u32)"));
        assert!(out.find("get").unwrap() < out.find("middleware").unwrap());
    }

    #[test]
    fn server_wraps_the_body_keyed_by_arguments() {
        let out = expand(quote!(server = "5m"), trending(quote!(db: Db)))
            .unwrap()
            .to_string();
        assert!(out.contains("SharedCache < Vec < Media > >"));
        assert!(out.contains("from_secs (300u64) , 10000u64"));
        assert!(out.contains("args_key (& (& media_type ,))"));
        assert!(!out.contains("middleware"));
    }

    #[test]
    fn refuses_session_extractors() {
        let item =
            trending(quote!(crate::StateExtractor { db, session_user, .. }: crate::StateExtractor));
        assert!(
            error(quote!(cdn = 300), item).contains("`session_user` looks like per-visitor state")
        );

        let item = trending(quote!(auth: AuthSession));
        assert!(error(quote!(server = "1h"), item).contains("per-visitor"));
    }

    #[test]
    fn neutral_extractors_pass_and_trust_overrides() {
        let item = trending(quote!(crate::StateExtractor { db, .. }: crate::StateExtractor));
        assert!(expand(quote!(cdn = 300), item).is_ok());

        let item = trending(quote!(user_agent: UserAgent));
        assert!(expand(quote!(cdn = 300, trust_extractors), item).is_ok());
    }

    #[test]
    fn refuses_writes_and_server_fns() {
        let post = quote! {
            #[post("/api/rate")]
            pub async fn rate(id: String) -> Result<()> { Ok(()) }
        };
        assert!(error(quote!(cdn = 300), post).contains("needs `#[get(..)]`"));

        let server = quote! {
            #[server]
            pub async fn rate(id: String) -> Result<()> { Ok(()) }
        };
        assert!(error(quote!(server = "5m"), server).contains("are POST"));
    }

    #[test]
    fn refuses_being_placed_below_the_route() {
        let bare = quote! {
            pub async fn get_trending() -> Result<Vec<Media>> { Ok(vec![]) }
        };
        assert!(error(quote!(cdn = 300), bare).contains("must be placed above"));
    }

    #[test]
    fn refuses_reading_the_request_in_the_body() {
        let item = quote! {
            #[get("/api/x")]
            pub async fn x() -> Result<u8> {
                let session = FullstackContext::extract::<Session, _>().await?;
                Ok(1)
            }
        };
        assert!(error(quote!(cdn = 60), item).contains("FullstackContext"));
    }

    #[test]
    fn refuses_bad_arguments() {
        let item = || trending(quote!(db: Db));
        assert!(error(quote!(), item()).contains("say where to cache"));
        assert!(error(quote!(cdn = 0), item()).contains("at least 1 second"));
        assert!(error(quote!(server = "soon"), item()).contains("whole number"));
        assert!(error(quote!(server = 300), item()).contains("duration string"));
        assert!(error(quote!(browser = 5), item()).contains("expected `cdn"));
    }

    #[test]
    fn refuses_non_results() {
        let item = quote! {
            #[get("/api/x")]
            pub async fn x() -> Vec<u8> { vec![] }
        };
        assert!(error(quote!(cdn = 60), item).contains("must return a `Result<T>`"));
    }
}
