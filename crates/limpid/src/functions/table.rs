//! In-memory key-value table store with TTL support.
//!
//! Replaces the old read-only lookup tables with a mutable store that
//! supports `table_lookup`, `table_upsert`, and `table_delete` from DSL
//! expressions.
//!
//! Tables are defined in a global `table { ... }` block:
//!
//! ```text
//! table {
//!     asset {
//!         load "/etc/limpid/tables/asset.json"
//!     }
//!     seen {
//!         max 100000
//!         ttl 3600
//!     }
//! }
//! ```
//!
//! - `load` — load initial data from a JSON or CSV file (entries have no TTL).
//! - `max`  — max entries; oldest evicted when exceeded. Default: no limit.
//! - `ttl`  — default TTL in seconds for `table_upsert`. Default: no expiry.
//!
//! ## Concurrency
//!
//! Each table has its own `RwLock`. `table_lookup` takes a read lock (fast path)
//! and only upgrades to write if the entry is expired. `table_upsert` / `table_delete`
//! take write locks. Tables are independent — locking one does not block others.
//!
//! ## Eviction
//!
//! When `max` is set and the table is at capacity, the oldest entry (by insertion
//! order) is evicted in O(1) using a `VecDeque` that tracks key insertion order.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Table entry
// ---------------------------------------------------------------------------

struct TableEntry {
    value: Value,
    expires_at: Option<Instant>,
}

// ---------------------------------------------------------------------------
// Single table
// ---------------------------------------------------------------------------

struct Table {
    entries: HashMap<String, TableEntry>,
    /// Insertion-order queue for O(1) oldest eviction.
    insertion_order: VecDeque<String>,
    max: Option<usize>,
    default_ttl: Option<Duration>,
}

impl Table {
    fn new(max: Option<usize>, default_ttl: Option<Duration>) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            max,
            default_ttl,
        }
    }

    /// Read-only lookup. Returns clone of value if found and not expired.
    /// Returns None if expired (caller should evict with write lock).
    fn lookup_read(&self, key: &str) -> LookupResult {
        match self.entries.get(key) {
            Some(entry) => {
                if let Some(exp) = entry.expires_at
                    && Instant::now() >= exp
                {
                    return LookupResult::Expired;
                }
                LookupResult::Found(entry.value.clone())
            }
            None => LookupResult::NotFound,
        }
    }

    /// Remove an expired key (called after lookup_read returns Expired).
    /// Does NOT remove from insertion_order — evict_oldest skips missing keys.
    fn evict_expired(&mut self, key: &str) {
        self.entries.remove(key);
    }

    fn upsert(&mut self, key: String, value: Value, expire: Option<Duration>) {
        let expires_at = expire.map(|d| Instant::now() + d);

        let is_new = !self.entries.contains_key(&key);

        // Evict oldest if at capacity and this is a new key
        if is_new && let Some(max) = self.max {
            while self.entries.len() >= max {
                if self.insertion_order.is_empty() {
                    break; // safety: no more tracked entries to evict
                }
                self.evict_oldest();
            }
        }

        self.entries
            .insert(key.clone(), TableEntry { value, expires_at });

        if is_new {
            self.insertion_order.push_back(key);
        }
    }

    /// Does NOT remove from insertion_order — evict_oldest skips missing keys.
    fn delete(&mut self, key: &str) {
        self.entries.remove(key);
    }

    /// Evict the oldest entry by insertion order. O(1) amortized.
    /// Skips entries that were already removed (e.g. by delete or TTL eviction).
    fn evict_oldest(&mut self) {
        while let Some(key) = self.insertion_order.pop_front() {
            if self.entries.remove(&key).is_some() {
                return;
            }
            // Key was already removed — skip and try next
        }
    }
}

enum LookupResult {
    Found(Value),
    NotFound,
    Expired,
}

// ---------------------------------------------------------------------------
// Global table store (thread-safe)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TableStore {
    /// Each table has its own RwLock for independent concurrency.
    tables: Arc<HashMap<String, RwLock<Table>>>,
}

impl TableStore {
    /// Build a TableStore from table configurations. Called once at startup.
    pub fn from_configs(configs: Vec<TableConfig>) -> Result<Self> {
        let mut tables = HashMap::new();
        for config in configs {
            let mut table = Table::new(config.max, config.default_ttl);

            if let Some(path) = &config.load_path {
                let data = load_file(path)?;
                info!(
                    "table '{}': loaded {} entries from {}",
                    config.name,
                    data.len(),
                    path.display()
                );
                for (key, value) in data {
                    table.entries.insert(
                        key.clone(),
                        TableEntry {
                            value,
                            expires_at: None, // loaded entries have no TTL
                        },
                    );
                    table.insertion_order.push_back(key);
                }
            } else {
                info!(
                    "table '{}': created (max: {:?}, ttl: {:?})",
                    config.name, config.max, config.default_ttl
                );
            }

            if let Some(max) = config.max
                && table.entries.len() > max
            {
                warn!(
                    "table '{}': loaded {} entries but max is {} — excess entries will be evicted on first upsert",
                    config.name,
                    table.entries.len(),
                    max
                );
            }

            tables.insert(config.name, RwLock::new(table));
        }

        Ok(Self {
            tables: Arc::new(tables),
        })
    }

    pub fn lookup(&self, table_name: &str, key: &str) -> Value {
        let table_lock = match self.tables.get(table_name) {
            Some(t) => t,
            None => {
                warn!("table_lookup: unknown table '{}'", table_name);
                return Value::Null;
            }
        };

        // Fast path: read lock
        {
            let table = table_lock.read().unwrap_or_else(|e| e.into_inner());
            match table.lookup_read(key) {
                LookupResult::Found(v) => return v,
                LookupResult::NotFound => return Value::Null,
                LookupResult::Expired => {} // fall through to write lock
            }
        }

        // Slow path: write lock to evict expired entry.
        // Re-check under write lock (another thread may have upserted or evicted).
        let mut table = table_lock.write().unwrap_or_else(|e| e.into_inner());
        match table.lookup_read(key) {
            LookupResult::Found(v) => v,
            LookupResult::NotFound => Value::Null,
            LookupResult::Expired => {
                table.evict_expired(key);
                Value::Null
            }
        }
    }

    /// Upsert with an explicit TTL override (None = no expiry).
    pub fn upsert(&self, table_name: &str, key: &str, value: Value, expire: Option<Duration>) {
        let table_lock = match self.tables.get(table_name) {
            Some(t) => t,
            None => {
                warn!("table_upsert: unknown table '{}'", table_name);
                return;
            }
        };
        let mut table = table_lock.write().unwrap_or_else(|e| e.into_inner());
        table.upsert(key.to_string(), value, expire);
    }

    /// Upsert using the table's default TTL.
    pub fn upsert_with_default(&self, table_name: &str, key: &str, value: Value) {
        let table_lock = match self.tables.get(table_name) {
            Some(t) => t,
            None => {
                warn!("table_upsert: unknown table '{}'", table_name);
                return;
            }
        };
        let mut table = table_lock.write().unwrap_or_else(|e| e.into_inner());
        let ttl = table.default_ttl;
        table.upsert(key.to_string(), value, ttl);
    }

    pub fn delete(&self, table_name: &str, key: &str) {
        let table_lock = match self.tables.get(table_name) {
            Some(t) => t,
            None => {
                warn!("table_delete: unknown table '{}'", table_name);
                return;
            }
        };
        let mut table = table_lock.write().unwrap_or_else(|e| e.into_inner());
        table.delete(key);
    }
}

/// Configuration for a single table, parsed from the global `table { ... }` block.
pub struct TableConfig {
    pub name: String,
    pub max: Option<usize>,
    pub default_ttl: Option<Duration>,
    pub load_path: Option<std::path::PathBuf>,
}

// ---------------------------------------------------------------------------
// File loading (reused from the old lookup module)
// ---------------------------------------------------------------------------

fn load_file(path: &Path) -> Result<HashMap<String, Value>> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "json" => load_json(path),
        "csv" => load_csv(path),
        _ => load_json(path), // default to JSON
    }
}

fn load_json(path: &Path) -> Result<HashMap<String, Value>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse JSON from {}", path.display()))?;

    match value {
        Value::Object(map) => Ok(map.into_iter().collect()),
        _ => anyhow::bail!("table file must be a JSON object: {}", path.display()),
    }
}

fn load_csv(path: &Path) -> Result<HashMap<String, Value>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let mut table = HashMap::new();
    let mut lines = content.lines();

    let headers: Vec<&str> = match lines.next() {
        Some(line) => line.split(',').map(|s| s.trim()).collect(),
        None => return Ok(table),
    };

    for line in lines {
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if cols.is_empty() {
            continue;
        }
        let key = cols[0].to_string();
        if headers.len() == 2 && cols.len() >= 2 {
            table.insert(key, Value::String(cols[1].to_string()));
        } else {
            let mut obj = serde_json::Map::new();
            for (i, &header) in headers.iter().enumerate().skip(1) {
                let val = cols.get(i).unwrap_or(&"");
                obj.insert(header.to_string(), Value::String(val.to_string()));
            }
            table.insert(key, Value::Object(obj));
        }
    }

    Ok(table)
}
