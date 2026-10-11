//! Where cached reads outlive the app: a redb file on mobile and desktop, IndexedDB in
//! the browser, and nowhere on the server. Every backend stores the same
//! thing (a JSON string per key) and fails soft: a store that can't be read
//! or written is just a cache miss.

pub(crate) use backend::{clear, load, prune, save};

/// Oldest saved answers go first; malformed and expired entries are removed
/// even when the store is below its capacity. The owner marker is metadata.
#[cfg(any(
    test,
    all(
        not(feature = "server"),
        any(
            feature = "mobile",
            feature = "desktop",
            all(feature = "web", target_arch = "wasm32")
        )
    )
))]
fn eviction_keys(
    entries: Vec<(String, String)>,
    limits: &super::CacheConfig,
    now: i64,
) -> Vec<String> {
    let mut remove = Vec::new();
    let mut retained = Vec::new();
    let keep_for = i64::try_from(limits.keep_for.as_millis()).unwrap_or(i64::MAX);
    for (key, value) in entries {
        if key == super::OWNER_KEY {
            continue;
        }
        let saved_at = serde_json::from_str::<serde_json::Value>(&value)
            .ok()
            .and_then(|json| json.get("saved_at").and_then(serde_json::Value::as_i64));
        match saved_at {
            Some(saved_at)
                if now.saturating_sub(saved_at) < keep_for && value.len() <= limits.max_bytes =>
            {
                retained.push((saved_at, key, value.len()))
            }
            _ => remove.push(key),
        }
    }
    retained.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let mut bytes = retained.iter().map(|entry| entry.2).sum::<usize>();
    let mut count = retained.len();
    for (_, key, size) in retained {
        if count <= limits.max_entries && bytes <= limits.max_bytes {
            break;
        }
        remove.push(key);
        count -= 1;
        bytes -= size;
    }
    remove
}

#[cfg(all(any(feature = "mobile", feature = "desktop"), not(feature = "server")))]
mod backend {
    use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
    use std::{path::PathBuf, sync::OnceLock};

    const ENTRIES: TableDefinition<&str, &str> = TableDefinition::new("entries");

    fn database() -> Option<&'static Database> {
        static DATABASE: OnceLock<Option<Database>> = OnceLock::new();
        DATABASE
            .get_or_init(|| {
                let dir = cache_dir()?;
                std::fs::create_dir_all(&dir).ok()?;
                let file = format!("{}.redb", super::super::config().name);
                Database::create(dir.join(file)).ok()
            })
            .as_ref()
    }

    /// The OS-managed cache directory, which the OS may clear under storage
    /// pressure. That suits a cache: nothing here is the only copy.
    #[cfg(target_os = "android")]
    fn cache_dir() -> Option<PathBuf> {
        use jni::objects::{JObject, JString};

        let context = ndk_context::android_context();
        // SAFETY: `ndk_context` hands out the process's live JavaVM and
        // Activity pointers, which outlive this call.
        let vm = unsafe { jni::JavaVM::from_raw(context.vm().cast()) }.ok()?;
        let mut env = vm.attach_current_thread().ok()?;
        let activity = unsafe { JObject::from_raw(context.context().cast()) };
        let dir = env
            .call_method(&activity, "getCacheDir", "()Ljava/io/File;", &[])
            .ok()?
            .l()
            .ok()?;
        let path = env
            .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
            .ok()?
            .l()
            .ok()?;
        let path: String = env.get_string(&JString::from(path)).ok()?.into();
        Some(PathBuf::from(path))
    }

    #[cfg(target_os = "ios")]
    fn cache_dir() -> Option<PathBuf> {
        Some(PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches"))
    }

    /// Desktop apps keep it in the user's cache directory. Desktop runs of
    /// a mobile build (the simulator host, `dx serve`) use the temp directory.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn cache_dir() -> Option<PathBuf> {
        #[cfg(feature = "desktop")]
        if let Some(dir) = dirs::cache_dir() {
            return Some(dir.join("g3-cache"));
        }
        Some(std::env::temp_dir().join("g3-cache"))
    }

    pub(crate) async fn load(key: &str) -> Option<String> {
        let read = database()?.begin_read().ok()?;
        let table = read.open_table(ENTRIES).ok()?;
        let value = table.get(key).ok()??;
        Some(value.value().to_string())
    }

    pub(crate) async fn save(key: &str, value: &str, generation: u64) {
        if generation != super::super::owner_generation() {
            return;
        }
        let Some(database) = database() else { return };
        let Ok(write) = database.begin_write() else {
            return;
        };
        if let Ok(mut table) = write.open_table(ENTRIES) {
            let _ = table.insert(key, value);
            trim(&mut table);
        }
        let _ = write.commit();
    }

    fn trim(table: &mut redb::Table<'_, &str, &str>) {
        let Ok(iter) = table.iter() else { return };
        let entries = iter
            .filter_map(Result::ok)
            .map(|(key, value)| (key.value().to_string(), value.value().to_string()))
            .collect();
        for key in super::eviction_keys(entries, super::super::config(), super::super::now_ms()) {
            let _ = table.remove(key.as_str());
        }
    }

    pub(crate) async fn prune(generation: u64) {
        if generation != super::super::owner_generation() {
            return;
        }
        let Some(database) = database() else { return };
        let Ok(write) = database.begin_write() else {
            return;
        };
        if let Ok(mut table) = write.open_table(ENTRIES) {
            trim(&mut table);
        }
        let _ = write.commit();
    }

    pub(crate) async fn clear(generation: u64) {
        if generation != super::super::owner_generation() {
            return;
        }
        let Some(database) = database() else { return };
        let Ok(write) = database.begin_write() else {
            return;
        };
        let _ = write.delete_table(ENTRIES);
        let _ = write.commit();
    }
}

#[cfg(all(
    feature = "web",
    target_arch = "wasm32",
    not(feature = "mobile"),
    not(feature = "desktop"),
    not(feature = "server")
))]
mod backend {
    use futures_channel::oneshot;
    use rexie::{ObjectStore, Rexie, TransactionMode};
    use std::future::Future;
    use wasm_bindgen::JsValue;

    const ENTRIES: &str = "entries";

    /// Runs `work` to completion on a task of its own, and waits for it.
    ///
    /// A cached read is a `use_resource`, and a rerun (an invalidation, a new
    /// key) drops its previous future wherever it was. Dropped mid-transaction,
    /// an IndexedDB request loses the handler the browser still calls when the
    /// request completes, which throws "closure invoked recursively or after
    /// being dropped". Detached, the transaction always finishes; the caller
    /// only stops waiting for it.
    async fn detached<T: 'static>(work: impl Future<Output = T> + 'static) -> Option<T> {
        let (answer, answered) = oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = answer.send(work.await);
        });
        answered.await.ok()
    }

    /// Opened per call: IndexedDB handles are cheap, and holding one in a
    /// static would need it to be `Sync`, which browser objects are not.
    async fn open() -> Option<Rexie> {
        Rexie::builder(&super::super::config().name)
            .version(1)
            .add_object_store(ObjectStore::new(ENTRIES))
            .build()
            .await
            .ok()
    }

    pub(crate) async fn load(key: &str) -> Option<String> {
        let key = key.to_string();
        detached(async move {
            let database = open().await?;
            let transaction = database
                .transaction(&[ENTRIES], TransactionMode::ReadOnly)
                .ok()?;
            let value = transaction
                .store(ENTRIES)
                .ok()?
                .get(JsValue::from_str(&key))
                .await
                .ok()?;
            // Finish the transaction before dropping it, for the same reason
            // as `detached`: its completion handler must outlive the request.
            let _ = transaction.done().await;
            value?.as_string()
        })
        .await
        .flatten()
    }

    pub(crate) async fn save(key: &str, value: &str, generation: u64) {
        let (key, value) = (key.to_string(), value.to_string());
        detached(async move {
            let Some(database) = open().await else { return };
            if generation != super::super::owner_generation() {
                return;
            }
            let Ok(transaction) = database.transaction(&[ENTRIES], TransactionMode::ReadWrite)
            else {
                return;
            };
            if let Ok(store) = transaction.store(ENTRIES) {
                let _ = store
                    .put(&JsValue::from_str(&value), Some(&JsValue::from_str(&key)))
                    .await;
                trim(&store).await;
            }
            let _ = transaction.done().await;
        })
        .await;
    }

    async fn trim(store: &rexie::Store) {
        let Ok(entries) = store.scan(None, None, None, None).await else {
            return;
        };
        let entries = entries
            .into_iter()
            .filter_map(|(key, value)| Some((key.as_string()?, value.as_string()?)))
            .collect();
        for key in super::eviction_keys(entries, super::super::config(), super::super::now_ms()) {
            let _ = store.delete(JsValue::from_str(&key)).await;
        }
    }

    pub(crate) async fn prune(generation: u64) {
        detached(async move {
            let Some(database) = open().await else { return };
            if generation != super::super::owner_generation() {
                return;
            }
            let Ok(transaction) = database.transaction(&[ENTRIES], TransactionMode::ReadWrite)
            else {
                return;
            };
            if let Ok(store) = transaction.store(ENTRIES) {
                trim(&store).await;
            }
            let _ = transaction.done().await;
        })
        .await;
    }

    pub(crate) async fn clear(generation: u64) {
        detached(async move {
            let Some(database) = open().await else { return };
            if generation != super::super::owner_generation() {
                return;
            }
            let Ok(transaction) = database.transaction(&[ENTRIES], TransactionMode::ReadWrite)
            else {
                return;
            };
            if let Ok(store) = transaction.store(ENTRIES) {
                let _ = store.clear().await;
            }
            let _ = transaction.done().await;
        })
        .await;
    }
}

/// No persistent store: the server (which must not cache per-user data), a
/// native check of a web build, or a build with none of `web`, `mobile` or
/// `desktop`.
/// The in-memory cache still works; nothing survives a restart.
#[cfg(not(any(
    all(any(feature = "mobile", feature = "desktop"), not(feature = "server")),
    all(
        feature = "web",
        target_arch = "wasm32",
        not(feature = "mobile"),
        not(feature = "desktop"),
        not(feature = "server")
    )
)))]
mod backend {
    pub(crate) async fn load(_key: &str) -> Option<String> {
        None
    }

    pub(crate) async fn save(_key: &str, _value: &str, _generation: u64) {}

    pub(crate) async fn clear(_generation: u64) {}

    pub(crate) async fn prune(_generation: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_eviction_removes_expiry_then_oldest_and_keeps_the_owner() {
        let answer =
            |saved_at| serde_json::json!({"saved_at": saved_at, "value": [1, 2, 3]}).to_string();
        let limits = super::super::CacheConfig::new("test")
            .keep_for(std::time::Duration::from_millis(100))
            .max_entries(1);
        let entries = vec![
            (super::super::OWNER_KEY.into(), "account-a".into()),
            ("expired".into(), answer(10)),
            ("older".into(), answer(950)),
            ("newest".into(), answer(990)),
            ("invalid".into(), "broken".into()),
        ];
        let keys = eviction_keys(entries.clone(), &limits, 1000);
        assert_eq!(keys, ["expired", "invalid", "older"]);
        let keys = eviction_keys(entries, &limits.max_bytes(1), 1000);
        assert!(keys.contains(&"newest".to_string()));
        assert!(!keys.contains(&super::super::OWNER_KEY.to_string()));
    }
}
