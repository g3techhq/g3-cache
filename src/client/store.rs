//! Where cached reads outlive the app: a redb file on mobile and desktop, IndexedDB in
//! the browser, and nowhere on the server. Every backend stores the same
//! thing (a JSON string per key) and fails soft: a store that can't be read
//! or written is just a cache miss.

pub(crate) use backend::{clear, load, save};

#[cfg(all(any(feature = "mobile", feature = "desktop"), not(feature = "server")))]
mod backend {
    use redb::{Database, ReadableDatabase, TableDefinition};
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

    pub(crate) async fn save(key: &str, value: &str) {
        let Some(database) = database() else { return };
        let Ok(write) = database.begin_write() else {
            return;
        };
        if let Ok(mut table) = write.open_table(ENTRIES) {
            let _ = table.insert(key, value);
        }
        let _ = write.commit();
    }

    pub(crate) async fn clear() {
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

    pub(crate) async fn save(key: &str, value: &str) {
        let (key, value) = (key.to_string(), value.to_string());
        detached(async move {
            let Some(database) = open().await else { return };
            let Ok(transaction) = database.transaction(&[ENTRIES], TransactionMode::ReadWrite)
            else {
                return;
            };
            if let Ok(store) = transaction.store(ENTRIES) {
                let _ = store
                    .put(&JsValue::from_str(&value), Some(&JsValue::from_str(&key)))
                    .await;
            }
            let _ = transaction.done().await;
        })
        .await;
    }

    pub(crate) async fn clear() {
        detached(async move {
            let Some(database) = open().await else { return };
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

    pub(crate) async fn save(_key: &str, _value: &str) {}

    pub(crate) async fn clear() {}
}
