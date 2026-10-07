//! Local upload history (SQLite). Openable from a data-dir path for CLI reuse.

use rusqlite::{params, Connection, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// A row in the uploads history table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: i64,
    /// The R2 object key (used for delete / URL construction).
    pub key: String,
    /// User-visible display name (editable in UI).
    pub display_name: String,
    /// Size in bytes.
    pub size: i64,
    pub content_type: String,
    /// Full public URL.
    pub url: String,
    /// Unix timestamp (seconds) when the upload happened.
    pub uploaded_at: i64,
    /// Vault name used for the upload (schema migration adds this).
    #[serde(default = "default_vault_name")]
    pub vault: String,
}

fn default_vault_name() -> String {
    "default".to_string()
}

/// Open (or create) the history DB under `data_dir`, apply pragmas, and init schema.
pub fn open(data_dir: &Path) -> Result<Connection> {
    let _ = std::fs::create_dir_all(data_dir);
    let conn = Connection::open(data_dir.join("r2share.db"))?;
    init(&conn)?;
    Ok(conn)
}

/// Apply WAL + busy timeout, then create/migrate tables.
pub fn init(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS uploads (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            key          TEXT    NOT NULL UNIQUE,
            display_name TEXT    NOT NULL,
            size         INTEGER NOT NULL DEFAULT 0,
            content_type TEXT    NOT NULL DEFAULT 'application/octet-stream',
            url          TEXT    NOT NULL,
            uploaded_at  INTEGER NOT NULL
        );",
    )?;
    migrate_add_vault_column(conn)?;
    Ok(())
}

fn migrate_add_vault_column(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(uploads)")?;
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?;
    if !cols.iter().any(|c| c == "vault") {
        conn.execute(
            "ALTER TABLE uploads ADD COLUMN vault TEXT NOT NULL DEFAULT 'default'",
            [],
        )?;
    }
    Ok(())
}

/// Insert a new upload record; returns the new row id.
pub fn insert(
    conn: &Connection,
    key: &str,
    display_name: &str,
    size: i64,
    content_type: &str,
    url: &str,
    vault: &str,
) -> Result<i64> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let vault = if vault.trim().is_empty() {
        "default"
    } else {
        vault
    };

    conn.execute(
        "INSERT INTO uploads (key, display_name, size, content_type, url, uploaded_at, vault)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![key, display_name, size, content_type, url, ts, vault],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Return uploads, newest first. Optional vault filter and limit.
pub fn list(
    conn: &Connection,
    vault: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<FileRecord>> {
    let mut sql = String::from(
        "SELECT id, key, display_name, size, content_type, url, uploaded_at, vault
         FROM uploads",
    );
    if vault.is_some() {
        sql.push_str(" WHERE vault = ?1");
    }
    sql.push_str(" ORDER BY uploaded_at DESC");
    if limit.is_some() {
        if vault.is_some() {
            sql.push_str(" LIMIT ?2");
        } else {
            sql.push_str(" LIMIT ?1");
        }
    }

    let mut stmt = conn.prepare(&sql)?;
    let map_row = |row: &rusqlite::Row| -> Result<FileRecord> {
        Ok(FileRecord {
            id: row.get(0)?,
            key: row.get(1)?,
            display_name: row.get(2)?,
            size: row.get(3)?,
            content_type: row.get(4)?,
            url: row.get(5)?,
            uploaded_at: row.get(6)?,
            vault: row.get(7)?,
        })
    };

    let records = match (vault, limit) {
        (Some(v), Some(n)) => stmt
            .query_map(params![v, n as i64], map_row)?
            .collect(),
        (Some(v), None) => stmt.query_map(params![v], map_row)?.collect(),
        (None, Some(n)) => stmt.query_map(params![n as i64], map_row)?.collect(),
        (None, None) => stmt.query_map([], map_row)?.collect(),
    };
    records
}

/// Return one upload by R2 key.
pub fn get(conn: &Connection, key: &str) -> Result<FileRecord> {
    conn.query_row(
        "SELECT id, key, display_name, size, content_type, url, uploaded_at, vault
         FROM uploads
         WHERE key = ?1",
        params![key],
        |row| {
            Ok(FileRecord {
                id: row.get(0)?,
                key: row.get(1)?,
                display_name: row.get(2)?,
                size: row.get(3)?,
                content_type: row.get(4)?,
                url: row.get(5)?,
                uploaded_at: row.get(6)?,
                vault: row.get(7)?,
            })
        },
    )
}

/// Update a record after renaming its R2 object.
pub fn rename(
    conn: &Connection,
    old_key: &str,
    new_key: &str,
    display_name: &str,
    url: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE uploads
         SET key = ?1, display_name = ?2, url = ?3
         WHERE key = ?4",
        params![new_key, display_name, url, old_key],
    )?;
    Ok(())
}

/// Remove a record by R2 key.
pub fn delete(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM uploads WHERE key = ?1", params![key])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("r2share-db-test-{nanos}"))
    }

    #[test]
    fn opens_in_wal_mode_with_busy_timeout() {
        let dir = tmp();
        let conn = open(&dir).expect("open db");

        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(mode.to_ascii_lowercase(), "wal");

        let timeout_ms: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("busy_timeout");
        assert_eq!(timeout_ms, 5000);

        insert(&conn, "k1", "a.txt", 1, "text/plain", "https://example/k1", "default")
            .unwrap();
        assert_eq!(list(&conn, None, None).unwrap().len(), 1);

        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_vault_column_and_defaults_existing_rows() {
        let dir = tmp();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("r2share.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE uploads (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    key TEXT NOT NULL UNIQUE,
                    display_name TEXT NOT NULL,
                    size INTEGER NOT NULL DEFAULT 0,
                    content_type TEXT NOT NULL DEFAULT 'application/octet-stream',
                    url TEXT NOT NULL,
                    uploaded_at INTEGER NOT NULL
                );
                INSERT INTO uploads (key, display_name, size, content_type, url, uploaded_at)
                VALUES ('old', 'old.txt', 1, 'text/plain', 'https://x/old', 1);",
            )
            .unwrap();
        }
        let conn = open(&dir).unwrap();
        let rows = list(&conn, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].vault, "default");
        assert_eq!(list(&conn, Some("default"), Some(10)).unwrap().len(), 1);
        assert!(list(&conn, Some("other"), None).unwrap().is_empty());
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
