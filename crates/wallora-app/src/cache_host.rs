//! Owns the Tokio runtime that hosts the image-cache actor and shuts it down in order.
//!
//! The cache actor must outlive any single Iced task, so it runs on its own small runtime.
//! `CacheStore` is channel based, so Iced tasks on a different runtime may use it freely.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::runtime::{Builder, Runtime};
use wallora_core::services::cache::{CacheConfig, CacheError, CacheStore};

pub struct CacheHost {
    runtime: Runtime,
    store: Mutex<Option<CacheStore>>,
}

impl CacheHost {
    pub fn new() -> std::io::Result<Arc<Self>> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("wallora-cache")
            .enable_time()
            .build()?;
        Ok(Arc::new(Self {
            runtime,
            store: Mutex::new(None),
        }))
    }

    /// Runs `future` to completion on the cache runtime from a helper thread, so this is safe to
    /// call from code that already runs inside another runtime's context (such as Iced boot).
    pub fn block_on<F>(&self, future: F) -> Option<F::Output>
    where
        F: Future + Send,
        F::Output: Send,
    {
        std::thread::scope(|scope| scope.spawn(|| self.runtime.block_on(future)).join().ok())
    }

    /// Opens the cache (taking its exclusive root lock) and remembers the handle for shutdown.
    pub fn open(&self, config: CacheConfig) -> Result<CacheStore, CacheError> {
        let store = self
            .block_on(CacheStore::open(config))
            .ok_or_else(|| CacheError::Io("cache startup thread panicked".to_string()))??;
        if let Ok(mut slot) = self.store.lock() {
            *slot = Some(store.clone());
        }
        Ok(store)
    }

    /// Stops cache admissions, joins every writer, and releases the root lock. Idempotent.
    pub fn shutdown(&self) {
        let store = self.store.lock().ok().and_then(|mut slot| slot.take());
        if let Some(store) = store {
            let _ = self.block_on(store.shutdown());
        }
    }
}
