use rusqlite::{params, Connection, Result};
use serde::{Deserialize, Serialize};

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
}

/// Create tables if they don't exist yet.
pub fn init(conn: &Connection) -> Result<()> {
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
    )
}

/// Insert a new upload record; returns the new row id.
pub fn insert(
    conn: &Connection,
    key: &str,
    display_name: &str,
    size: i64,
    content_type: &str,
    url: &str,
) -> Result<i64> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    conn.execute(
        "INSERT INTO uploads (key, display_name, size, content_type, url, uploaded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key, display_name, size, content_type, url, ts],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Return all uploads, newest first.
pub fn list(conn: &Connection) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, key, display_name, size, content_type, url, uploaded_at
         FROM uploads
         ORDER BY uploaded_at DESC",
    )?;
    let records = stmt
        .query_map([], |row| {
            Ok(FileRecord {
                id: row.get(0)?,
                key: row.get(1)?,
                display_name: row.get(2)?,
                size: row.get(3)?,
                content_type: row.get(4)?,
                url: row.get(5)?,
                uploaded_at: row.get(6)?,
            })
        })?
        .collect();
    records
}

/// Return one upload by R2 key.
pub fn get(conn: &Connection, key: &str) -> Result<FileRecord> {
    conn.query_row(
        "SELECT id, key, display_name, size, content_type, url, uploaded_at
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
