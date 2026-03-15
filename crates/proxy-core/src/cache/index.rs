use rusqlite::params;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_rusqlite::Connection;

use crate::error::Error;

/// A cache entry row from SQLite.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub fingerprint: String,
    pub url: String,
    pub method: String,
    pub status_code: u16,
    pub content_type: Option<String>,
    pub content_length: Option<i64>,
    pub response_headers: String, // JSON
    pub cache_policy: Vec<u8>,    // serde-serialized
    pub created_at: i64,
    pub last_accessed: i64,
    pub expires_at: Option<i64>,
    pub file_path: String,
    pub file_size: i64,
    pub host: String,
    pub vary_key: Option<String>,
    pub media_type: Option<String>,
    pub status: String,
    pub stale_at: Option<i64>,
}

/// Async wrapper around SQLite for cache operations.
#[derive(Clone)]
pub struct CacheIndex {
    conn: Connection,
}

impl CacheIndex {
    /// Open or create the cache database.
    pub async fn open(db_path: &PathBuf) -> Result<Self, Error> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(db_path)
            .await
            .map_err(|e| Error::Proxy(format!("Failed to open database: {}", e)))?;

        // Initialize schema
        conn.call(|conn| {
            conn.execute_batch("PRAGMA journal_mode=WAL;")?;
            conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            conn.execute_batch(SCHEMA)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Proxy(format!("Failed to initialize schema: {}", e)))?;

        Ok(Self { conn })
    }

    /// Create a CacheIndex from an existing connection (for use by other modules).
    pub fn from_conn(conn: Connection) -> Self {
        Self { conn }
    }

    /// Get a reference to the underlying connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Insert a new cache entry.
    pub async fn insert(&self, entry: &CacheEntry) -> Result<(), Error> {
        let entry = entry.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO cache_entries (
                        fingerprint, url, method, status_code, content_type, content_length,
                        response_headers, cache_policy, created_at, last_accessed, expires_at,
                        file_path, file_size, host, vary_key, media_type, status, stale_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                    params![
                        entry.fingerprint,
                        entry.url,
                        entry.method,
                        entry.status_code as i32,
                        entry.content_type,
                        entry.content_length,
                        entry.response_headers,
                        entry.cache_policy,
                        entry.created_at,
                        entry.last_accessed,
                        entry.expires_at,
                        entry.file_path,
                        entry.file_size,
                        entry.host,
                        entry.vary_key,
                        entry.media_type,
                        entry.status,
                        entry.stale_at,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to insert cache entry: {}", e)))
    }

    /// Lookup an active cache entry by fingerprint.
    pub async fn lookup(&self, fingerprint: &str) -> Result<Option<CacheEntry>, Error> {
        let fp = fingerprint.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM cache_entries WHERE fingerprint = ?1 AND status = 'active'",
                )?;
                let entry = stmt.query_row(params![fp], row_to_entry).optional()?;
                Ok(entry)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to lookup cache entry: {}", e)))
    }

    /// Lookup active cache entries by URL (may have multiple due to Vary).
    pub async fn lookup_by_url(&self, url: &str) -> Result<Vec<CacheEntry>, Error> {
        let url = url.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM cache_entries WHERE url = ?1 AND status = 'active'",
                )?;
                let entries = stmt
                    .query_map(params![url], row_to_entry)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(entries)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to lookup by URL: {}", e)))
    }

    /// Update last_accessed timestamp.
    pub async fn touch(&self, fingerprint: &str) -> Result<(), Error> {
        let fp = fingerprint.to_string();
        let now = now_unix();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE cache_entries SET last_accessed = ?1 WHERE fingerprint = ?2",
                    params![now, fp],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to touch cache entry: {}", e)))
    }

    /// Batch update last_accessed timestamps.
    pub async fn touch_batch(&self, updates: Vec<(String, i64)>) -> Result<(), Error> {
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare(
                        "UPDATE cache_entries SET last_accessed = ?1 WHERE fingerprint = ?2",
                    )?;
                    for (fp, ts) in &updates {
                        stmt.execute(params![ts, fp])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to batch touch: {}", e)))
    }

    /// Mark an entry as stale and update its file_path.
    pub async fn mark_stale(
        &self,
        fingerprint: &str,
        new_file_path: &str,
    ) -> Result<(), Error> {
        let fp = fingerprint.to_string();
        let nfp = new_file_path.to_string();
        let now = now_unix();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE cache_entries SET status = 'stale', stale_at = ?1, file_path = ?2 WHERE fingerprint = ?3",
                    params![now, nfp, fp],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to mark stale: {}", e)))
    }

    /// Update the cache policy and last_accessed for a revalidated entry (304).
    pub async fn update_policy(
        &self,
        fingerprint: &str,
        cache_policy: Vec<u8>,
        response_headers: &str,
    ) -> Result<(), Error> {
        let fp = fingerprint.to_string();
        let rh = response_headers.to_string();
        let now = now_unix();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE cache_entries SET cache_policy = ?1, last_accessed = ?2, response_headers = ?3 WHERE fingerprint = ?4",
                    params![cache_policy, now, rh, fp],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to update cache policy: {}", e)))
    }

    /// Mark stale all active GET/HEAD entries matching a URL.
    /// Returns the fingerprints and file_paths that were marked stale.
    pub async fn invalidate_by_url(&self, url: &str) -> Result<Vec<(String, String)>, Error> {
        let url = url.to_string();
        let now = now_unix();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT fingerprint, file_path FROM cache_entries WHERE url = ?1 AND status = 'active'"
                )?;
                let entries: Vec<(String, String)> = stmt
                    .query_map(params![url], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;

                for (fp, _) in &entries {
                    conn.execute(
                        "UPDATE cache_entries SET status = 'stale', stale_at = ?1 WHERE fingerprint = ?2",
                        params![now, fp],
                    )?;
                }
                Ok(entries)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to invalidate by URL: {}", e)))
    }

    /// Permanently delete an entry from the database.
    pub async fn delete(&self, fingerprint: &str) -> Result<Option<String>, Error> {
        let fp = fingerprint.to_string();
        self.conn
            .call(move |conn| {
                let file_path: Option<String> = conn
                    .query_row(
                        "SELECT file_path FROM cache_entries WHERE fingerprint = ?1",
                        params![fp],
                        |row| row.get(0),
                    )
                    .optional()?;
                conn.execute(
                    "DELETE FROM cache_entries WHERE fingerprint = ?1",
                    params![fp],
                )?;
                Ok(file_path)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to delete cache entry: {}", e)))
    }

    /// Get total cache size in bytes (active entries only).
    pub async fn total_size(&self) -> Result<i64, Error> {
        self.conn
            .call(|conn| {
                let size: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(file_size), 0) FROM cache_entries WHERE status = 'active'",
                    [],
                    |row| row.get(0),
                )?;
                Ok(size)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to get total size: {}", e)))
    }

    /// Get total cache size including stale entries.
    pub async fn total_size_all(&self) -> Result<i64, Error> {
        self.conn
            .call(|conn| {
                let size: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(file_size), 0) FROM cache_entries",
                    [],
                    |row| row.get(0),
                )?;
                Ok(size)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to get total size: {}", e)))
    }

    /// Get expired active entries (expires_at < now).
    pub async fn get_expired_active(&self) -> Result<Vec<(String, String)>, Error> {
        let now = now_unix();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT fingerprint, file_path FROM cache_entries WHERE status = 'active' AND expires_at IS NOT NULL AND expires_at < ?1"
                )?;
                let rows = stmt.query_map(params![now], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to get expired entries: {}", e)))
    }

    /// Get stale entries older than the given timestamp, for permanent deletion.
    pub async fn get_stale_older_than(&self, cutoff: i64) -> Result<Vec<(String, String)>, Error> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT fingerprint, file_path FROM cache_entries WHERE status = 'stale' AND stale_at < ?1 ORDER BY stale_at ASC"
                )?;
                let rows = stmt.query_map(params![cutoff], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to get old stale entries: {}", e)))
    }

    /// Get stale entries ordered by stale_at (oldest first) for LRU eviction.
    pub async fn get_stale_for_eviction(&self, limit: usize) -> Result<Vec<(String, String, i64)>, Error> {
        let limit = limit as i64;
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT fingerprint, file_path, file_size FROM cache_entries WHERE status = 'stale' ORDER BY stale_at ASC LIMIT ?1"
                )?;
                let rows = stmt.query_map(params![limit], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to get stale for eviction: {}", e)))
    }

    /// Get active entries ordered by last_accessed (oldest first) for LRU eviction.
    pub async fn get_active_for_eviction(&self, limit: usize) -> Result<Vec<(String, String, i64)>, Error> {
        let limit = limit as i64;
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT fingerprint, file_path, file_size FROM cache_entries WHERE status = 'active' ORDER BY last_accessed ASC LIMIT ?1"
                )?;
                let rows = stmt.query_map(params![limit], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to get active for eviction: {}", e)))
    }

    /// Get count of entries by status.
    pub async fn count_by_status(&self, status: &str) -> Result<i64, Error> {
        let status = status.to_string();
        self.conn
            .call(move |conn| {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM cache_entries WHERE status = ?1",
                    params![status],
                    |row| row.get(0),
                )?;
                Ok(count)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to count entries: {}", e)))
    }

    /// Get cache statistics.
    pub async fn stats(&self) -> Result<CacheStats, Error> {
        self.conn
            .call(|conn| {
                let active_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM cache_entries WHERE status = 'active'",
                    [], |row| row.get(0),
                )?;
                let stale_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM cache_entries WHERE status = 'stale'",
                    [], |row| row.get(0),
                )?;
                let active_size: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(file_size), 0) FROM cache_entries WHERE status = 'active'",
                    [], |row| row.get(0),
                )?;
                let total_size: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(file_size), 0) FROM cache_entries",
                    [], |row| row.get(0),
                )?;
                let image_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM cache_entries WHERE media_type = 'image' AND status = 'active'",
                    [], |row| row.get(0),
                )?;
                let video_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM cache_entries WHERE media_type = 'video' AND status = 'active'",
                    [], |row| row.get(0),
                )?;
                let audio_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM cache_entries WHERE media_type = 'audio' AND status = 'active'",
                    [], |row| row.get(0),
                )?;
                Ok(CacheStats {
                    active_entries: active_count,
                    stale_entries: stale_count,
                    active_size,
                    total_size,
                    image_count,
                    video_count,
                    audio_count,
                })
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to get stats: {}", e)))
    }

    /// Search entries by URL pattern (LIKE query).
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<CacheEntry>, Error> {
        let pattern = format!("%{}%", query);
        let limit = limit as i64;
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM cache_entries WHERE url LIKE ?1 ORDER BY last_accessed DESC LIMIT ?2",
                )?;
                let entries = stmt
                    .query_map(params![pattern, limit], row_to_entry)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(entries)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to search: {}", e)))
    }

    /// Mark all active entries as stale.
    pub async fn mark_all_stale(&self) -> Result<i64, Error> {
        let now = now_unix();
        self.conn
            .call(move |conn| {
                let count = conn.execute(
                    "UPDATE cache_entries SET status = 'stale', stale_at = ?1 WHERE status = 'active'",
                    params![now],
                )?;
                Ok(count as i64)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to mark all stale: {}", e)))
    }

    /// List entries with filtering and pagination.
    pub async fn list_entries(
        &self,
        host: Option<String>,
        media_type: Option<String>,
        status: Option<String>,
        query: Option<String>,
        offset: i64,
        limit: i64,
    ) -> Result<(Vec<CacheEntry>, i64), Error> {
        self.conn
            .call(move |conn| {
                let mut where_clauses = Vec::new();
                let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

                if let Some(ref h) = host {
                    where_clauses.push(format!("host = ?{}", params_vec.len() + 1));
                    params_vec.push(Box::new(h.clone()));
                }
                if let Some(ref mt) = media_type {
                    where_clauses.push(format!("media_type = ?{}", params_vec.len() + 1));
                    params_vec.push(Box::new(mt.clone()));
                }
                if let Some(ref s) = status {
                    where_clauses.push(format!("status = ?{}", params_vec.len() + 1));
                    params_vec.push(Box::new(s.clone()));
                }
                if let Some(ref q) = query {
                    where_clauses.push(format!("url LIKE ?{}", params_vec.len() + 1));
                    params_vec.push(Box::new(format!("%{}%", q)));
                }

                let where_sql = if where_clauses.is_empty() {
                    String::new()
                } else {
                    format!("WHERE {}", where_clauses.join(" AND "))
                };

                // Count total
                let count_sql = format!("SELECT COUNT(*) FROM cache_entries {}", where_sql);
                let total: i64 = conn.query_row(
                    &count_sql,
                    rusqlite::params_from_iter(params_vec.iter().map(|p| p.as_ref())),
                    |row| row.get(0),
                )?;

                // Fetch page
                let select_sql = format!(
                    "SELECT * FROM cache_entries {} ORDER BY last_accessed DESC LIMIT ?{} OFFSET ?{}",
                    where_sql,
                    params_vec.len() + 1,
                    params_vec.len() + 2,
                );
                params_vec.push(Box::new(limit));
                params_vec.push(Box::new(offset));

                let mut stmt = conn.prepare(&select_sql)?;
                let entries = stmt
                    .query_map(
                        rusqlite::params_from_iter(params_vec.iter().map(|p| p.as_ref())),
                        row_to_entry,
                    )?
                    .collect::<Result<Vec<_>, _>>()?;

                Ok((entries, total))
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to list entries: {}", e)))
    }

    /// Restore a stale entry back to active.
    pub async fn restore(&self, fingerprint: &str) -> Result<(), Error> {
        let fp = fingerprint.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE cache_entries SET status = 'active', stale_at = NULL WHERE fingerprint = ?1",
                    params![fp],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to restore entry: {}", e)))
    }

    /// Delete all entries permanently.
    pub async fn delete_all(&self) -> Result<i64, Error> {
        self.conn
            .call(|conn| {
                let count = conn.execute("DELETE FROM cache_entries", [])?;
                Ok(count as i64)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to delete all: {}", e)))
    }

    /// Get a setting value by key.
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>, Error> {
        let k = key.to_string();
        self.conn
            .call(move |conn| {
                let val: Option<String> = conn
                    .query_row(
                        "SELECT value FROM settings WHERE key = ?1",
                        params![k],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(val)
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to get setting: {}", e)))
    }

    /// Set a setting value (upsert).
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<(), Error> {
        let k = key.to_string();
        let v = value.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                    params![k, v],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to set setting: {}", e)))
    }
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub active_entries: i64,
    pub stale_entries: i64,
    pub active_size: i64,
    pub total_size: i64,
    pub image_count: i64,
    pub video_count: i64,
    pub audio_count: i64,
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<CacheEntry> {
    Ok(CacheEntry {
        fingerprint: row.get("fingerprint")?,
        url: row.get("url")?,
        method: row.get("method")?,
        status_code: row.get::<_, i32>("status_code")? as u16,
        content_type: row.get("content_type")?,
        content_length: row.get("content_length")?,
        response_headers: row.get("response_headers")?,
        cache_policy: row.get("cache_policy")?,
        created_at: row.get("created_at")?,
        last_accessed: row.get("last_accessed")?,
        expires_at: row.get("expires_at")?,
        file_path: row.get("file_path")?,
        file_size: row.get("file_size")?,
        host: row.get("host")?,
        vary_key: row.get("vary_key")?,
        media_type: row.get("media_type")?,
        status: row.get("status")?,
        stale_at: row.get("stale_at")?,
    })
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cache_entries (
    fingerprint       TEXT PRIMARY KEY,
    url               TEXT NOT NULL,
    method            TEXT NOT NULL DEFAULT 'GET',
    status_code       INTEGER NOT NULL,
    content_type      TEXT,
    content_length    INTEGER,
    response_headers  TEXT NOT NULL,
    cache_policy      BLOB NOT NULL,
    created_at        INTEGER NOT NULL,
    last_accessed     INTEGER NOT NULL,
    expires_at        INTEGER,
    file_path         TEXT NOT NULL UNIQUE,
    file_size         INTEGER NOT NULL,
    host              TEXT NOT NULL,
    vary_key          TEXT,
    media_type        TEXT,
    status            TEXT NOT NULL DEFAULT 'active',
    stale_at          INTEGER
);

CREATE INDEX IF NOT EXISTS idx_last_accessed ON cache_entries(last_accessed);
CREATE INDEX IF NOT EXISTS idx_expires_at ON cache_entries(expires_at);
CREATE INDEX IF NOT EXISTS idx_host ON cache_entries(host);
CREATE INDEX IF NOT EXISTS idx_url ON cache_entries(url);
CREATE INDEX IF NOT EXISTS idx_media_type ON cache_entries(media_type);
CREATE INDEX IF NOT EXISTS idx_status ON cache_entries(status);
CREATE INDEX IF NOT EXISTS idx_stale_at ON cache_entries(stale_at);

CREATE TABLE IF NOT EXISTS range_resources (
    id                INTEGER PRIMARY KEY,
    url_pattern       TEXT NOT NULL UNIQUE,
    host              TEXT NOT NULL,
    total_size        INTEGER,
    content_type      TEXT,
    dir_path          TEXT NOT NULL,
    is_complete       BOOLEAN NOT NULL DEFAULT FALSE,
    assembled_path    TEXT,
    created_at        INTEGER NOT NULL,
    last_accessed     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS range_slabs (
    resource_id       INTEGER NOT NULL REFERENCES range_resources(id) ON DELETE CASCADE,
    range_start       INTEGER NOT NULL,
    range_end         INTEGER NOT NULL,
    slab_path         TEXT NOT NULL,
    PRIMARY KEY (resource_id, range_start)
);

CREATE TABLE IF NOT EXISTS settings (
    key               TEXT PRIMARY KEY,
    value             TEXT NOT NULL
);
";

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_insert_and_lookup() {
        let index = CacheIndex::open(&PathBuf::from(":memory:")).await.unwrap();

        let entry = CacheEntry {
            fingerprint: "abc123".into(),
            url: "https://example.com/page".into(),
            method: "GET".into(),
            status_code: 200,
            content_type: Some("text/html".into()),
            content_length: Some(1234),
            response_headers: "{}".into(),
            cache_policy: vec![1, 2, 3],
            created_at: 1000,
            last_accessed: 1000,
            expires_at: Some(2000),
            file_path: "example.com/page.html".into(),
            file_size: 1234,
            host: "example.com".into(),
            vary_key: None,
            media_type: None,
            status: "active".into(),
            stale_at: None,
        };

        index.insert(&entry).await.unwrap();

        let found = index.lookup("abc123").await.unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.url, "https://example.com/page");
        assert_eq!(found.status, "active");
    }

    #[tokio::test]
    async fn test_mark_stale_hides_from_lookup() {
        let index = CacheIndex::open(&PathBuf::from(":memory:")).await.unwrap();

        let entry = CacheEntry {
            fingerprint: "def456".into(),
            url: "https://example.com/img.png".into(),
            method: "GET".into(),
            status_code: 200,
            content_type: Some("image/png".into()),
            content_length: None,
            response_headers: "{}".into(),
            cache_policy: vec![],
            created_at: 1000,
            last_accessed: 1000,
            expires_at: None,
            file_path: "example.com/img.png".into(),
            file_size: 5000,
            host: "example.com".into(),
            vary_key: None,
            media_type: Some("image".into()),
            status: "active".into(),
            stale_at: None,
        };

        index.insert(&entry).await.unwrap();
        index
            .mark_stale("def456", "example.com/img~stale~2026.png")
            .await
            .unwrap();

        // Active lookup should return None
        let found = index.lookup("def456").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_total_size() {
        let index = CacheIndex::open(&PathBuf::from(":memory:")).await.unwrap();

        for i in 0..3 {
            let entry = CacheEntry {
                fingerprint: format!("fp{}", i),
                url: format!("https://example.com/{}", i),
                method: "GET".into(),
                status_code: 200,
                content_type: None,
                content_length: None,
                response_headers: "{}".into(),
                cache_policy: vec![],
                created_at: 1000,
                last_accessed: 1000,
                expires_at: None,
                file_path: format!("example.com/{}.bin", i),
                file_size: 1000,
                host: "example.com".into(),
                vary_key: None,
                media_type: None,
                status: "active".into(),
                stale_at: None,
            };
            index.insert(&entry).await.unwrap();
        }

        assert_eq!(index.total_size().await.unwrap(), 3000);
    }
}
