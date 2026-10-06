//! Multi-vault configuration (schema v2) with transparent v1 migration.
//!
//! Core load/save/import/export take a data-dir path so a future CLI can share
//! this module without a running Tauri app. Writes are atomic (temp + fsync +
//! rename) and guarded by an advisory lock on `config.lock`.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Flat credentials view used by existing Tauri commands and [`crate::r2::R2Client`].
/// Matches the v1 on-disk shape and the Settings UI payload.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Config {
    #[serde(default)]
    pub account_id: String,

    #[serde(default = "default_bucket")]
    pub bucket: String,

    #[serde(default)]
    pub access_key_id: String,

    #[serde(default)]
    pub secret_access_key: String,

    /// e.g. "https://pub-….r2.dev"
    #[serde(default)]
    pub public_url_base: String,
}

fn default_bucket() -> String {
    "r2share".to_string()
}

impl Config {
    /// True when the minimum fields required for R2 operations are filled.
    pub fn is_configured(&self) -> bool {
        !self.account_id.is_empty()
            && !self.access_key_id.is_empty()
            && !self.secret_access_key.is_empty()
            && !self.public_url_base.is_empty()
    }
}

/// One named set of R2 credentials (a "vault").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Vault {
    pub name: String,

    #[serde(default)]
    pub account_id: String,

    #[serde(default = "default_bucket")]
    pub bucket: String,

    #[serde(default)]
    pub access_key_id: String,

    #[serde(default)]
    pub secret_access_key: String,

    #[serde(default)]
    pub public_url_base: String,
}

impl Vault {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            account_id: String::new(),
            bucket: default_bucket(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            public_url_base: String::new(),
        }
    }

    pub fn to_flat(&self) -> Config {
        Config {
            account_id: self.account_id.clone(),
            bucket: self.bucket.clone(),
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            public_url_base: self.public_url_base.clone(),
        }
    }

    pub fn apply_flat(&mut self, c: &Config) {
        self.account_id = c.account_id.clone();
        self.bucket = c.bucket.clone();
        self.access_key_id = c.access_key_id.clone();
        self.secret_access_key = c.secret_access_key.clone();
        self.public_url_base = c.public_url_base.clone();
    }

    fn blank_secrets(&self) -> Self {
        let mut v = self.clone();
        v.access_key_id.clear();
        v.secret_access_key.clear();
        v
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("vault name must not be empty".into());
        }
        if self.name != self.name.trim() {
            return Err(format!(
                "vault name must not have leading/trailing whitespace: {:?}",
                self.name
            ));
        }
        Ok(())
    }
}

/// Local folder → vault mapping for one-way folder sync (local → R2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderMapping {
    pub path: String,
    pub vault: String,
    /// When true the GUI/CLI watcher skips this mapping.
    #[serde(default)]
    pub paused: bool,
}

impl FolderMapping {
    pub fn new(path: impl Into<String>, vault: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            vault: vault.into(),
            paused: false,
        }
    }
}

/// Suggested default sync folder (created on first enable if missing).
pub const DEFAULT_SYNC_FOLDER: &str = "/workspace/r2share-sync";

/// On-disk config schema version 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub version: u32,
    pub default_vault: String,
    pub vaults: Vec<Vault>,
    #[serde(default)]
    pub folder_mappings: Vec<FolderMapping>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::empty_v2()
    }
}

impl AppConfig {
    pub const VERSION: u32 = 2;

    pub fn empty_v2() -> Self {
        Self {
            version: Self::VERSION,
            default_vault: "default".to_string(),
            vaults: Vec::new(),
            folder_mappings: Vec::new(),
        }
    }

    pub fn from_v1(v1: Config) -> Self {
        Self {
            version: Self::VERSION,
            default_vault: "default".to_string(),
            vaults: vec![Vault {
                name: "default".to_string(),
                account_id: v1.account_id,
                bucket: if v1.bucket.is_empty() {
                    default_bucket()
                } else {
                    v1.bucket
                },
                access_key_id: v1.access_key_id,
                secret_access_key: v1.secret_access_key,
                public_url_base: v1.public_url_base,
            }],
            folder_mappings: Vec::new(),
        }
    }

    /// Flat credentials for the default vault (empty Config if missing).
    pub fn default_flat(&self) -> Config {
        self.vault_by_name(&self.default_vault)
            .map(Vault::to_flat)
            .unwrap_or_default()
    }

    pub fn vault_by_name(&self, name: &str) -> Option<&Vault> {
        self.vaults.iter().find(|v| v.name == name)
    }

    pub fn vault_by_name_mut(&mut self, name: &str) -> Option<&mut Vault> {
        self.vaults.iter_mut().find(|v| v.name == name)
    }

    /// Create or update the default vault from a flat Settings payload.
    pub fn set_default_flat(&mut self, flat: Config) {
        let name = if self.default_vault.trim().is_empty() {
            "default".to_string()
        } else {
            self.default_vault.clone()
        };
        self.default_vault = name.clone();
        if let Some(v) = self.vault_by_name_mut(&name) {
            v.apply_flat(&flat);
        } else {
            let mut v = Vault::new(name);
            v.apply_flat(&flat);
            self.vaults.push(v);
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != Self::VERSION {
            return Err(format!(
                "unsupported config version {} (expected {})",
                self.version,
                Self::VERSION
            ));
        }
        if self.default_vault.trim().is_empty() {
            return Err("default_vault must not be empty".into());
        }
        let mut seen = std::collections::HashSet::new();
        for v in &self.vaults {
            v.validate()?;
            if !seen.insert(v.name.clone()) {
                return Err(format!("duplicate vault name: {}", v.name));
            }
        }
        if !self.vaults.is_empty() && self.vault_by_name(&self.default_vault).is_none() {
            return Err(format!(
                "default_vault {:?} not found in vaults",
                self.default_vault
            ));
        }
        let mut seen_paths = std::collections::HashSet::new();
        for m in &self.folder_mappings {
            if m.path.trim().is_empty() {
                return Err("folder_mappings.path must not be empty".into());
            }
            if m.vault.trim().is_empty() {
                return Err("folder_mappings.vault must not be empty".into());
            }
            if !self.vaults.is_empty() && self.vault_by_name(&m.vault).is_none() {
                return Err(format!(
                    "folder_mappings.vault {:?} not found in vaults",
                    m.vault
                ));
            }
            if !seen_paths.insert(m.path.clone()) {
                return Err(format!("duplicate folder_mappings.path: {}", m.path));
            }
        }
        Ok(())
    }
}

// ── paths ─────────────────────────────────────────────────────────────────────

pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.json")
}

fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.lock")
}

// ── advisory lock ─────────────────────────────────────────────────────────────

struct ConfigLock {
    _file: File,
}

fn acquire_lock(data_dir: &Path) -> Result<ConfigLock, String> {
    fs::create_dir_all(data_dir).map_err(|e| format!("create data dir: {e}"))?;
    let path = lock_path(data_dir);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("open config.lock: {e}"))?;
    file.lock_exclusive()
        .map_err(|e| format!("lock config.lock: {e}"))?;
    Ok(ConfigLock { _file: file })
}

// ── atomic write ──────────────────────────────────────────────────────────────

/// Write `contents` to `path` via temp file + fsync + rename. Unix mode 0600.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("create dir: {e}"))?;

    let tmp_name = format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("config"),
        std::process::id()
    );
    let tmp_path = dir.join(tmp_name);

    let write_result = (|| -> Result<(), String> {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);

        let mut file = opts
            .open(&tmp_path)
            .map_err(|e| format!("open temp config: {e}"))?;
        file.write_all(contents)
            .map_err(|e| format!("write temp config: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("fsync temp config: {e}"))?;
        drop(file);

        fs::rename(&tmp_path, path).map_err(|e| format!("rename config: {e}"))?;

        #[cfg(unix)]
        {
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(path, perms).map_err(|e| format!("chmod config: {e}"))?;
        }
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_result
}

// ── load / save ───────────────────────────────────────────────────────────────

/// Parse raw JSON into AppConfig, migrating v1 → v2 when needed.
/// Returns `(config, migrated)` where `migrated` means the caller should persist.
pub fn parse_config_json(raw: &str) -> Result<(AppConfig, bool), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok((AppConfig::empty_v2(), false));
    }

    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("invalid config JSON: {e}"))?;

    if is_v2_shape(&value) {
        let cfg: AppConfig = serde_json::from_value(value)
            .map_err(|e| format!("invalid v2 config: {e}"))?;
        cfg.validate()?;
        return Ok((cfg, false));
    }

    if is_v1_shape(&value) {
        let v1: Config =
            serde_json::from_value(value).map_err(|e| format!("invalid v1 config: {e}"))?;
        let cfg = AppConfig::from_v1(v1);
        cfg.validate()?;
        return Ok((cfg, true));
    }

    Err("unrecognised config.json shape (expected v1 flat or v2 vaults)".into())
}

fn is_v2_shape(value: &serde_json::Value) -> bool {
    value
        .get("version")
        .and_then(|v| v.as_u64())
        .map(|v| v == 2)
        .unwrap_or(false)
        && value.get("vaults").map(|v| v.is_array()).unwrap_or(false)
}

fn is_v1_shape(value: &serde_json::Value) -> bool {
    value.is_object()
        && value.get("vaults").is_none()
        && (value.get("account_id").is_some()
            || value.get("access_key_id").is_some()
            || value.get("secret_access_key").is_some()
            || value.get("bucket").is_some()
            || value.get("public_url_base").is_some())
}

/// Load config from `data_dir/config.json`. Missing/empty → empty v2.
/// Transparent v1→v2 migration is written back under the advisory lock.
pub fn load(data_dir: &Path) -> AppConfig {
    match load_with_migration(data_dir) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("r2share: config load error ({e}); using empty v2");
            AppConfig::empty_v2()
        }
    }
}

fn load_with_migration(data_dir: &Path) -> Result<AppConfig, String> {
    let _lock = acquire_lock(data_dir)?;
    let path = config_path(data_dir);
    if !path.exists() {
        return Ok(AppConfig::empty_v2());
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read config: {e}"))?;
    let (cfg, migrated) = parse_config_json(&raw)?;
    if migrated {
        save_unlocked(data_dir, &cfg)?;
    }
    Ok(cfg)
}

/// Persist `AppConfig` (must already be valid v2). Takes the advisory lock.
pub fn save(data_dir: &Path, cfg: &AppConfig) -> Result<(), String> {
    cfg.validate()?;
    let _lock = acquire_lock(data_dir)?;
    save_unlocked(data_dir, cfg)
}

fn save_unlocked(data_dir: &Path, cfg: &AppConfig) -> Result<(), String> {
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    atomic_write(&config_path(data_dir), json.as_bytes())
}

// ── import / export ───────────────────────────────────────────────────────────

/// How to resolve vault-name conflicts on import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Replace existing vaults with the same name.
    Overwrite,
    /// Keep existing vaults; skip imported ones with the same name.
    SkipExisting,
}

impl ImportMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "overwrite" => Ok(Self::Overwrite),
            "skip" | "skip-existing" | "skipexisting" => Ok(Self::SkipExisting),
            other => Err(format!(
                "unknown import mode {other:?} (expected overwrite|skip)"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultBundle {
    pub version: u32,
    pub vaults: Vec<Vault>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportResult {
    pub imported: usize,
    pub skipped: usize,
    pub overwritten: usize,
}

/// Export selected (or all) vaults to `path` as JSON.
/// When `include_secrets` is false, access/secret keys are blanked.
/// On Unix the file is created with mode 0600.
pub fn export_vaults(
    data_dir: &Path,
    path: &Path,
    include_secrets: bool,
    names: Option<Vec<String>>,
) -> Result<(), String> {
    let cfg = {
        let _lock = acquire_lock(data_dir)?;
        let raw_path = config_path(data_dir);
        if !raw_path.exists() {
            AppConfig::empty_v2()
        } else {
            let raw = fs::read_to_string(&raw_path).map_err(|e| format!("read config: {e}"))?;
            parse_config_json(&raw)?.0
        }
    };

    let selected: Vec<Vault> = match names {
        None => cfg.vaults.clone(),
        Some(list) => {
            let mut out = Vec::new();
            for n in &list {
                let v = cfg
                    .vault_by_name(n)
                    .ok_or_else(|| format!("vault not found: {n}"))?;
                out.push(v.clone());
            }
            out
        }
    };

    let vaults = if include_secrets {
        selected
    } else {
        selected.into_iter().map(|v| v.blank_secrets()).collect()
    };

    let bundle = VaultBundle {
        version: AppConfig::VERSION,
        vaults,
    };
    let json = serde_json::to_string_pretty(&bundle).map_err(|e| e.to_string())?;
    atomic_write(path, json.as_bytes())
}

/// Merge vaults from an export file into the on-disk config.
pub fn import_vaults(
    data_dir: &Path,
    path: &Path,
    mode: ImportMode,
) -> Result<ImportResult, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("read import file: {e}"))?;
    let incoming = parse_import_bundle(&raw)?;

    let _lock = acquire_lock(data_dir)?;
    let cfg_path = config_path(data_dir);
    let mut cfg = if cfg_path.exists() {
        let existing = fs::read_to_string(&cfg_path).map_err(|e| format!("read config: {e}"))?;
        parse_config_json(&existing)?.0
    } else {
        AppConfig::empty_v2()
    };

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut overwritten = 0usize;

    for vault in incoming.vaults {
        vault.validate()?;
        match cfg.vault_by_name_mut(&vault.name) {
            Some(existing) => match mode {
                ImportMode::Overwrite => {
                    merge_vault_preserve_secrets(existing, vault);
                    overwritten += 1;
                }
                ImportMode::SkipExisting => {
                    skipped += 1;
                }
            },
            None => {
                cfg.vaults.push(vault);
                imported += 1;
            }
        }
    }

    if cfg.vaults.is_empty() {
        cfg.default_vault = "default".to_string();
    } else if cfg.vault_by_name(&cfg.default_vault).is_none() {
        cfg.default_vault = cfg.vaults[0].name.clone();
    }

    cfg.validate()?;
    save_unlocked(data_dir, &cfg)?;

    Ok(ImportResult {
        imported,
        skipped,
        overwritten,
    })
}


/// Overwrite vault fields, but keep existing access/secret keys when the
/// incoming values are empty (so a `--no-secrets` export cannot wipe keys).
fn merge_vault_preserve_secrets(existing: &mut Vault, incoming: Vault) {
    let keep_access = incoming.access_key_id.is_empty();
    let keep_secret = incoming.secret_access_key.is_empty();
    let preserved_access = existing.access_key_id.clone();
    let preserved_secret = existing.secret_access_key.clone();
    *existing = incoming;
    if keep_access {
        existing.access_key_id = preserved_access;
    }
    if keep_secret {
        existing.secret_access_key = preserved_secret;
    }
}

fn parse_import_bundle(raw: &str) -> Result<VaultBundle, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("import file is empty".into());
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("invalid import JSON: {e}"))?;

    // Accept either {version, vaults:[…]} or a bare vaults array.
    let bundle = if value.is_array() {
        let vaults: Vec<Vault> = serde_json::from_value(value)
            .map_err(|e| format!("invalid vaults array: {e}"))?;
        VaultBundle {
            version: AppConfig::VERSION,
            vaults,
        }
    } else {
        let bundle: VaultBundle = serde_json::from_value(value)
            .map_err(|e| format!("invalid vault bundle: {e}"))?;
        if bundle.version != 0 && bundle.version != AppConfig::VERSION {
            return Err(format!(
                "unsupported import version {} (expected {})",
                bundle.version,
                AppConfig::VERSION
            ));
        }
        bundle
    };

    if bundle.vaults.is_empty() {
        return Err("import file contains no vaults".into());
    }

    let mut seen = std::collections::HashSet::new();
    for v in &bundle.vaults {
        v.validate()?;
        if !seen.insert(v.name.clone()) {
            return Err(format!("duplicate vault name in import: {}", v.name));
        }
    }
    Ok(bundle)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("r2share-test-{label}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_v1() -> Config {
        Config {
            account_id: "acct-fake-111".into(),
            bucket: "bucket-fake".into(),
            access_key_id: "AKIAFAKEEXAMPLE".into(),
            secret_access_key: "secret/fake/example/key".into(),
            public_url_base: "https://pub-fake.example.r2.dev".into(),
        }
    }

    #[test]
    fn v1_migrates_to_v2_with_default_vault() {
        let dir = tmp_dir("migrate");
        let v1 = fake_v1();
        fs::write(
            config_path(&dir),
            serde_json::to_string_pretty(&v1).unwrap(),
        )
        .unwrap();

        let cfg = load(&dir);
        assert_eq!(cfg.version, 2);
        assert_eq!(cfg.default_vault, "default");
        assert_eq!(cfg.vaults.len(), 1);
        assert_eq!(cfg.vaults[0].name, "default");
        assert_eq!(cfg.vaults[0].account_id, v1.account_id);
        assert_eq!(cfg.vaults[0].access_key_id, v1.access_key_id);
        assert_eq!(cfg.vaults[0].secret_access_key, v1.secret_access_key);
        assert_eq!(cfg.vaults[0].bucket, v1.bucket);
        assert_eq!(cfg.vaults[0].public_url_base, v1.public_url_base);
        assert!(cfg.folder_mappings.is_empty());

        // Migration persisted as v2.
        let on_disk = fs::read_to_string(config_path(&dir)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(parsed["version"], 2);
        assert!(parsed["vaults"].as_array().unwrap().len() == 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_config_is_empty_v2() {
        let dir = tmp_dir("missing");
        let cfg = load(&dir);
        assert_eq!(cfg, AppConfig::empty_v2());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_is_empty_v2() {
        let dir = tmp_dir("empty");
        fs::write(config_path(&dir), "   \n").unwrap();
        let cfg = load(&dir);
        assert_eq!(cfg, AppConfig::empty_v2());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_with_and_without_secrets_and_mode() {
        let dir = tmp_dir("export");
        let mut cfg = AppConfig::from_v1(fake_v1());
        cfg.vaults.push({
            let mut v = Vault::new("other");
            v.account_id = "acct-2".into();
            v.access_key_id = "KEY2".into();
            v.secret_access_key = "SECRET2".into();
            v.public_url_base = "https://pub2.example".into();
            v
        });
        save(&dir, &cfg).unwrap();

        let with_path = dir.join("with-secrets.json");
        export_vaults(&dir, &with_path, true, None).unwrap();
        let with: VaultBundle =
            serde_json::from_str(&fs::read_to_string(&with_path).unwrap()).unwrap();
        assert_eq!(with.vaults.len(), 2);
        assert_eq!(with.vaults[0].secret_access_key, "secret/fake/example/key");
        assert_eq!(with.vaults[1].access_key_id, "KEY2");

        let without_path = dir.join("no-secrets.json");
        export_vaults(
            &dir,
            &without_path,
            false,
            Some(vec!["default".into()]),
        )
        .unwrap();
        let without: VaultBundle =
            serde_json::from_str(&fs::read_to_string(&without_path).unwrap()).unwrap();
        assert_eq!(without.vaults.len(), 1);
        assert!(without.vaults[0].access_key_id.is_empty());
        assert!(without.vaults[0].secret_access_key.is_empty());
        assert_eq!(without.vaults[0].account_id, "acct-fake-111");

        #[cfg(unix)]
        {
            let mode = fs::metadata(&with_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "export file mode should be 0600, got {mode:o}");
            let mode2 = fs::metadata(&without_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode2, 0o600);
            let cfg_mode = fs::metadata(config_path(&dir)).unwrap().permissions().mode() & 0o777;
            assert_eq!(cfg_mode, 0o600, "config.json mode should be 0600");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_merge_overwrite_and_skip() {
        let dir = tmp_dir("import");
        save(&dir, &AppConfig::from_v1(fake_v1())).unwrap();

        let bundle = VaultBundle {
            version: 2,
            vaults: vec![
                {
                    let mut v = Vault::new("default");
                    v.account_id = "acct-overwritten".into();
                    v.access_key_id = "NEWKEY".into();
                    v.secret_access_key = "NEWSECRET".into();
                    v.bucket = "new-bucket".into();
                    v.public_url_base = "https://new.example".into();
                    v
                },
                {
                    let mut v = Vault::new("extra");
                    v.account_id = "acct-extra".into();
                    v.access_key_id = "EXTRAKEY".into();
                    v.secret_access_key = "EXTRASECRET".into();
                    v.public_url_base = "https://extra.example".into();
                    v
                },
            ],
        };
        let import_path = dir.join("import.json");
        fs::write(&import_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();

        let skip = import_vaults(&dir, &import_path, ImportMode::SkipExisting).unwrap();
        assert_eq!(skip.skipped, 1);
        assert_eq!(skip.imported, 1);
        assert_eq!(skip.overwritten, 0);
        let after_skip = load(&dir);
        assert_eq!(after_skip.vaults.len(), 2);
        assert_eq!(
            after_skip.vault_by_name("default").unwrap().account_id,
            "acct-fake-111"
        );
        assert!(after_skip.vault_by_name("extra").is_some());

        let over = import_vaults(&dir, &import_path, ImportMode::Overwrite).unwrap();
        assert_eq!(over.overwritten, 2); // default + extra both exist now
        assert_eq!(over.imported, 0);
        let after_over = load(&dir);
        assert_eq!(
            after_over.vault_by_name("default").unwrap().account_id,
            "acct-overwritten"
        );
        assert_eq!(
            after_over.vault_by_name("default").unwrap().access_key_id,
            "NEWKEY"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_invalid_input() {
        let dir = tmp_dir("import-bad");
        save(&dir, &AppConfig::empty_v2()).unwrap();

        let bad_path = dir.join("bad.json");
        fs::write(&bad_path, "{not json").unwrap();
        assert!(import_vaults(&dir, &bad_path, ImportMode::Overwrite).is_err());

        fs::write(&bad_path, r#"{"version":2,"vaults":[]}"#).unwrap();
        assert!(import_vaults(&dir, &bad_path, ImportMode::Overwrite).is_err());

        fs::write(
            &bad_path,
            r#"{"version":2,"vaults":[{"name":"","account_id":"x"}]}"#,
        )
        .unwrap();
        assert!(import_vaults(&dir, &bad_path, ImportMode::Overwrite).is_err());

        fs::write(
            &bad_path,
            r#"{"version":2,"vaults":[{"name":"a"},{"name":"a"}]}"#,
        )
        .unwrap();
        assert!(import_vaults(&dir, &bad_path, ImportMode::Overwrite).is_err());

        fs::write(&bad_path, r#"{"version":99,"vaults":[{"name":"a"}]}"#).unwrap();
        assert!(import_vaults(&dir, &bad_path, ImportMode::Overwrite).is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_replaces_and_sets_mode() {
        let dir = tmp_dir("atomic");
        let path = dir.join("config.json");
        atomic_write(&path, b"{\"ok\":1}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"ok\":1}");
        atomic_write(&path, b"{\"ok\":2}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"ok\":2}");
        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_flat_adapter_roundtrip() {
        let mut cfg = AppConfig::empty_v2();
        assert!(!cfg.default_flat().is_configured());
        cfg.set_default_flat(fake_v1());
        assert_eq!(cfg.default_vault, "default");
        assert_eq!(cfg.vaults.len(), 1);
        let flat = cfg.default_flat();
        assert!(flat.is_configured());
        assert_eq!(flat.account_id, "acct-fake-111");
    }

    #[test]
    fn import_overwrite_preserves_secrets_when_incoming_empty() {
        let dir = tmp_dir("import-preserve");
        save(&dir, &AppConfig::from_v1(fake_v1())).unwrap();

        let bundle = VaultBundle {
            version: 2,
            vaults: vec![{
                let mut v = Vault::new("default");
                v.account_id = "acct-new".into();
                v.bucket = "bucket-new".into();
                v.public_url_base = "https://new.example".into();
                // access_key_id / secret_access_key left empty (no-secrets export)
                v
            }],
        };
        let import_path = dir.join("nosecrets.json");
        fs::write(&import_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();

        let result = import_vaults(&dir, &import_path, ImportMode::Overwrite).unwrap();
        assert_eq!(result.overwritten, 1);

        let after = load(&dir);
        let v = after.vault_by_name("default").unwrap();
        assert_eq!(v.account_id, "acct-new");
        assert_eq!(v.bucket, "bucket-new");
        assert_eq!(v.public_url_base, "https://new.example");
        assert_eq!(v.access_key_id, "AKIAFAKEEXAMPLE");
        assert_eq!(v.secret_access_key, "secret/fake/example/key");

        let _ = fs::remove_dir_all(&dir);
    }

}
