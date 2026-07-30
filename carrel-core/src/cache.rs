//! Reusable cache machinery shared by the desktop app and Carrel Server.
//!
//! `LruCache` is the in-memory LRU (moved here from the app crate);
//! `ManagedCache` + adapters arrive in later tasks.

use serde::Serialize;
use std::sync::{Arc, Mutex};

use crate::error::CarrelResult;
use crate::storage::Storage;

/// A simple LRU cache that bundles the data map and access order in a single
/// structure, so only one Mutex is needed. This eliminates the risk of lock
/// poisoning or inversion that arises from guarding the map and order with
/// separate Mutexes.
pub struct LruCache<V> {
    entries: std::collections::HashMap<String, V>,
    sizes: std::collections::HashMap<String, usize>,
    order: Vec<String>,
    capacity: usize,
    max_bytes: usize,
    current_bytes: usize,
}

impl<V> LruCache<V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            sizes: std::collections::HashMap::new(),
            order: Vec::new(),
            capacity,
            max_bytes: 0, // 0 = no memory limit
            current_bytes: 0,
        }
    }

    /// Set maximum total byte size for the cache (#52).
    pub fn set_max_bytes(&mut self, bytes: usize) {
        self.max_bytes = bytes;
    }

    /// Current total tracked byte size.
    pub fn total_bytes(&self) -> usize {
        self.current_bytes
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Move an existing key to the most-recently-used position.
    pub fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push(key.to_string());
    }

    /// Evict least-recently-used entries until both count and memory limits are satisfied.
    fn evict_if_needed(&mut self) {
        while self.entries.len() >= self.capacity
            || (self.max_bytes > 0 && self.current_bytes > self.max_bytes)
        {
            if let Some(oldest) = self.order.first().cloned() {
                self.entries.remove(&oldest);
                if let Some(size) = self.sizes.remove(&oldest) {
                    self.current_bytes = self.current_bytes.saturating_sub(size);
                }
                self.order.remove(0);
            } else {
                self.entries.clear();
                self.sizes.clear();
                self.current_bytes = 0;
                break;
            }
        }
    }

    /// Insert a key-value pair, evicting the least-recently-used entry when at capacity.
    pub fn insert(&mut self, key: String, value: V) {
        if self.entries.contains_key(&key) {
            self.touch(&key);
            self.entries.insert(key, value);
            return;
        }
        self.evict_if_needed();
        self.entries.insert(key.clone(), value);
        self.order.push(key);
    }

    /// Insert with explicit byte size tracking (#52).
    pub fn insert_with_size(&mut self, key: String, value: V, size_bytes: usize) {
        if self.entries.contains_key(&key) {
            // Update size tracking for existing entry
            if let Some(old_size) = self.sizes.get(&key) {
                self.current_bytes = self.current_bytes.saturating_sub(*old_size);
            }
            self.touch(&key);
            self.entries.insert(key.clone(), value);
            self.sizes.insert(key, size_bytes);
            self.current_bytes += size_bytes;
            return;
        }
        self.current_bytes += size_bytes;
        self.evict_if_needed();
        self.entries.insert(key.clone(), value);
        self.sizes.insert(key.clone(), size_bytes);
        self.order.push(key);
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.entries.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.entries.get_mut(key)
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
        if let Some(size) = self.sizes.remove(key) {
            self.current_bytes = self.current_bytes.saturating_sub(size);
        }
        self.order.retain(|k| k != key);
    }

    /// Drop every entry and reset byte accounting. Capacity and byte
    /// limits are configuration, not contents — they survive a clear.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.sizes.clear();
        self.order.clear();
        self.current_bytes = 0;
    }
}

// ---------------------------------------------------------------------------
// ManagedCache — unified lifecycle (stats/clear) over heterogeneous caches.
// Deliberately NOT a get/put abstraction: payload types differ per cache
// (parsed EPUB archive vs parsed MOBI book vs page bytes on disk).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheKind {
    Memory,
    Disk,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheSectionStats {
    pub name: String,
    pub kind: CacheKind,
    pub entry_count: usize,
    /// `None` when the cache does not track byte sizes (epub).
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnifiedCacheStats {
    pub sections: Vec<CacheSectionStats>,
    /// Sum of the known (`Some`) section bytes.
    pub total_bytes: u64,
}

pub trait ManagedCache: Send + Sync {
    /// Stable identifier for this cache section ("epub", "mobi", "pages").
    /// Used as the section key in stats payloads.
    fn name(&self) -> &'static str;

    /// Whether entries live in memory or on disk.
    fn kind(&self) -> CacheKind;

    /// Best-effort snapshot. Must not fail: implementations degrade to an empty
    /// section when state is unreadable (e.g. poisoned lock).
    fn stats(&self) -> CacheSectionStats;

    /// Drop all cached entries. Errors propagate — callers decide whether a
    /// partial clear matters.
    fn clear(&self) -> CarrelResult<()>;
}

/// Lifecycle adapter over an in-memory `LruCache` shared behind
/// `Arc<Mutex<..>>` (the same handle `AppState` holds).
pub struct MemoryCacheAdapter<V: Send + 'static> {
    name: &'static str,
    /// Explicit flag — not inferred from a zero byte count, so an empty
    /// byte-tracked cache still reports `Some(0)` rather than `None`.
    tracks_bytes: bool,
    cache: Arc<Mutex<LruCache<V>>>,
}

impl<V: Send> MemoryCacheAdapter<V> {
    pub fn new(name: &'static str, tracks_bytes: bool, cache: Arc<Mutex<LruCache<V>>>) -> Self {
        Self {
            name,
            tracks_bytes,
            cache,
        }
    }
}

impl<V: Send> ManagedCache for MemoryCacheAdapter<V> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn kind(&self) -> CacheKind {
        CacheKind::Memory
    }

    fn stats(&self) -> CacheSectionStats {
        // A poisoned lock degrades to an empty section rather than failing
        // the whole stats call — stats are informational.
        let (entry_count, bytes) = match self.cache.lock() {
            Ok(c) => (c.len(), c.total_bytes() as u64),
            Err(_) => (0, 0),
        };
        CacheSectionStats {
            name: self.name.to_string(),
            kind: CacheKind::Memory,
            entry_count,
            total_bytes: self.tracks_bytes.then_some(bytes),
        }
    }

    fn clear(&self) -> CarrelResult<()> {
        let mut c = self.cache.lock()?;
        c.clear();
        Ok(())
    }
}

/// Lifecycle adapter over the on-disk page cache (PDF/CBZ/CBR), delegating
/// to the existing `page_cache` functions. Internals stay in `page_cache`.
pub struct DiskPageCacheAdapter {
    storage: Arc<dyn Storage>,
}

impl DiskPageCacheAdapter {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }
}

impl ManagedCache for DiskPageCacheAdapter {
    fn name(&self) -> &'static str {
        "pages"
    }

    fn kind(&self) -> CacheKind {
        CacheKind::Disk
    }

    fn stats(&self) -> CacheSectionStats {
        let s = crate::page_cache::get_cache_stats(self.storage.as_ref());
        CacheSectionStats {
            name: "pages".to_string(),
            kind: CacheKind::Disk,
            entry_count: s.book_count,
            total_bytes: Some(s.total_size_bytes),
        }
    }

    fn clear(&self) -> CarrelResult<()> {
        crate::page_cache::clear_cache(self.storage.as_ref())
    }
}

// ---------------------------------------------------------------------------
// Registry helpers
// ---------------------------------------------------------------------------

pub fn unified_stats(registry: &[Box<dyn ManagedCache>]) -> UnifiedCacheStats {
    let sections: Vec<CacheSectionStats> = registry.iter().map(|c| c.stats()).collect();
    let total_bytes = sections.iter().filter_map(|s| s.total_bytes).sum();
    UnifiedCacheStats {
        sections,
        total_bytes,
    }
}

/// Clear every cache, attempting all of them even when one fails — a failed
/// disk clear must not leave the memory caches uncleared, and vice versa.
/// Returns the first error encountered, if any.
pub fn clear_all(registry: &[Box<dyn ManagedCache>]) -> CarrelResult<()> {
    let mut first_err = None;
    for cache in registry {
        if let Err(e) = cache.clear() {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::BookFormat;
    use crate::page_cache::{self, CacheManifest};
    use crate::storage::{LocalStorage, Storage};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    // #52: PDF cache memory limits (moved from src-tauri commands.rs)
    #[test]
    fn lru_cache_memory_tracking() {
        let mut cache = LruCache::<String>::new(100); // high count limit
        cache.set_max_bytes(100); // but low memory limit

        // Each entry is ~10 bytes
        cache.insert_with_size("a".to_string(), "0123456789".to_string(), 10);
        cache.insert_with_size("b".to_string(), "0123456789".to_string(), 10);
        assert_eq!(cache.total_bytes(), 20);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn lru_cache_memory_eviction() {
        let mut cache = LruCache::<String>::new(100);
        cache.set_max_bytes(25); // evict when over 25 bytes

        cache.insert_with_size("a".to_string(), "0123456789".to_string(), 10);
        cache.insert_with_size("b".to_string(), "0123456789".to_string(), 10);
        cache.insert_with_size("c".to_string(), "0123456789".to_string(), 10);

        // "a" should have been evicted to stay under 25 bytes
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some() || cache.get("c").is_some());
        assert!(cache.total_bytes() <= 25);
    }

    #[test]
    fn clear_resets_entries_and_bytes() {
        let mut cache = LruCache::<String>::new(10);
        cache.set_max_bytes(1000);
        cache.insert("plain".to_string(), "v".to_string());
        cache.insert_with_size("sized".to_string(), "v".to_string(), 42);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.total_bytes(), 42);

        cache.clear();

        assert!(cache.is_empty());
        assert_eq!(cache.total_bytes(), 0);
        // Still usable after clear
        cache.insert("again".to_string(), "v".to_string());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn memory_adapter_stats_tracked_bytes() {
        let cache = Arc::new(Mutex::new(LruCache::<String>::new(10)));
        cache
            .lock()
            .unwrap()
            .insert_with_size("k".to_string(), "v".to_string(), 42);

        let adapter = MemoryCacheAdapter::new("mobi", true, cache);
        let stats = adapter.stats();
        assert_eq!(stats.name, "mobi");
        assert!(matches!(stats.kind, CacheKind::Memory));
        assert_eq!(stats.entry_count, 1);
        assert_eq!(stats.total_bytes, Some(42));
    }

    #[test]
    fn memory_adapter_stats_untracked_bytes_is_none() {
        let cache = Arc::new(Mutex::new(LruCache::<String>::new(10)));
        cache
            .lock()
            .unwrap()
            .insert("k".to_string(), "v".to_string());

        let adapter = MemoryCacheAdapter::new("epub", false, cache);
        let stats = adapter.stats();
        assert_eq!(stats.entry_count, 1);
        assert_eq!(stats.total_bytes, None);
    }

    #[test]
    fn memory_adapter_tracked_but_empty_reports_some_zero() {
        let cache = Arc::new(Mutex::new(LruCache::<String>::new(10)));
        let adapter = MemoryCacheAdapter::new("mobi", true, cache);
        assert_eq!(adapter.stats().total_bytes, Some(0));
    }

    #[test]
    fn memory_adapter_clear_empties_underlying_cache() {
        let cache = Arc::new(Mutex::new(LruCache::<String>::new(10)));
        cache
            .lock()
            .unwrap()
            .insert_with_size("k".to_string(), "v".to_string(), 42);

        let adapter = MemoryCacheAdapter::new("mobi", true, Arc::clone(&cache));
        adapter.clear().unwrap();

        assert!(cache.lock().unwrap().is_empty());
        assert_eq!(cache.lock().unwrap().total_bytes(), 0);
    }

    /// Seed one fake cached book (manifest + 2 pages of 50 bytes each)
    /// under page_cache's key layout, mirroring page_cache's own test
    /// helper.
    fn seed_disk_cache(storage: &dyn Storage) {
        for i in 0..2u32 {
            storage
                .put(&format!("page-cache/hash_a/{i:03}.jpg"), &[0u8; 50])
                .unwrap();
        }
        let manifest = CacheManifest {
            book_id: "a".to_string(),
            book_hash: "hash_a".to_string(),
            page_count: 2,
            total_size_bytes: 100,
            extracted_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed: "2026-01-01T00:00:00Z".to_string(),
            pages: vec!["000.jpg".to_string(), "001.jpg".to_string()],
            format: BookFormat::Cbz,
            canonical_width: None,
        };
        page_cache::write_manifest(storage, "hash_a", &manifest).unwrap();
    }

    #[test]
    fn disk_adapter_stats_match_page_cache() {
        let dir = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = Arc::new(LocalStorage::new(dir.path()).unwrap());
        seed_disk_cache(storage.as_ref());

        let adapter = DiskPageCacheAdapter::new(Arc::clone(&storage));
        let stats = adapter.stats();
        assert_eq!(stats.name, "pages");
        assert!(matches!(stats.kind, CacheKind::Disk));
        assert_eq!(stats.entry_count, 1); // one book
        assert_eq!(stats.total_bytes, Some(100)); // 2 pages x 50 bytes, manifest excluded
    }

    #[test]
    fn disk_adapter_clear_wipes_all_keys() {
        let dir = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = Arc::new(LocalStorage::new(dir.path()).unwrap());
        seed_disk_cache(storage.as_ref());

        let adapter = DiskPageCacheAdapter::new(Arc::clone(&storage));
        adapter.clear().unwrap();

        assert!(storage.list("page-cache/").unwrap().is_empty());
    }

    #[test]
    fn unified_stats_aggregates_known_bytes_only() {
        let dir = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = Arc::new(LocalStorage::new(dir.path()).unwrap());
        seed_disk_cache(storage.as_ref());

        let epub = Arc::new(Mutex::new(LruCache::<String>::new(10)));
        epub.lock()
            .unwrap()
            .insert("e".to_string(), "v".to_string());
        let mobi = Arc::new(Mutex::new(LruCache::<String>::new(10)));
        mobi.lock()
            .unwrap()
            .insert_with_size("m".to_string(), "v".to_string(), 7);

        let registry: Vec<Box<dyn ManagedCache>> = vec![
            Box::new(MemoryCacheAdapter::new("epub", false, epub)),
            Box::new(MemoryCacheAdapter::new("mobi", true, mobi)),
            Box::new(DiskPageCacheAdapter::new(storage)),
        ];

        let stats = unified_stats(&registry);
        assert_eq!(stats.sections.len(), 3);
        // epub contributes None; total = mobi 7 + disk 100
        assert_eq!(stats.total_bytes, 107);
    }

    #[test]
    fn clear_all_clears_every_cache_in_registry() {
        let dir = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = Arc::new(LocalStorage::new(dir.path()).unwrap());
        seed_disk_cache(storage.as_ref());

        let epub = Arc::new(Mutex::new(LruCache::<String>::new(10)));
        epub.lock()
            .unwrap()
            .insert("e".to_string(), "v".to_string());

        let registry: Vec<Box<dyn ManagedCache>> = vec![
            Box::new(MemoryCacheAdapter::new("epub", false, Arc::clone(&epub))),
            Box::new(DiskPageCacheAdapter::new(Arc::clone(&storage))),
        ];

        clear_all(&registry).unwrap();

        assert!(epub.lock().unwrap().is_empty());
        assert!(storage.list("page-cache/").unwrap().is_empty());
    }
}
