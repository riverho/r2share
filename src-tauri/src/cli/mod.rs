//! `r2share-cli` — clap-driven interface over config/db/r2 core.

mod args;
mod sync_cmd;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use args::{Cli, Commands, OnConflict, VaultCmd};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};

use crate::config::{
    self, AppConfig, Config, ImportMode, Vault,
};
use crate::db;
use crate::r2::{ProgressFn, R2Client};

/// App data directory shared with the GUI (Linux XDG).
pub fn default_data_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("com.summonai.r2share");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/com.summonai.r2share")
}

pub fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.clone().unwrap_or_else(default_data_dir);
    match cli.command {
        Commands::Upload {
            files,
            vault,
            json,
        } => {
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(cmd_upload(&data_dir, files, vault, json))
        }
        Commands::Ls {
            vault,
            limit,
            json,
        } => cmd_ls(&data_dir, vault, limit, json),
        Commands::Rm { key, vault, yes } => {
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(cmd_rm(&data_dir, key, vault, yes))
        }
        Commands::Url { key } => cmd_url(&data_dir, key),
        Commands::Vault { cmd } => {
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(cmd_vault(&data_dir, cmd))
        }
        Commands::Sync { cmd } => {
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(sync_cmd::cmd_sync(&data_dir, cmd))
        }
    }
}

fn load_cfg(data_dir: &Path) -> AppConfig {
    config::load(data_dir)
}

fn resolve_vault<'a>(cfg: &'a AppConfig, name: Option<&str>) -> Result<&'a Vault, String> {
    let name = name.unwrap_or(cfg.default_vault.as_str());
    cfg.vault_by_name(name)
        .ok_or_else(|| format!("vault not found: {name}"))
}

fn vault_flat(cfg: &AppConfig, name: Option<&str>) -> Result<(String, Config), String> {
    let v = resolve_vault(cfg, name)?;
    Ok((v.name.clone(), v.to_flat()))
}

fn require_configured(config: &Config) -> Result<(), String> {
    if config.is_configured() {
        Ok(())
    } else {
        Err("vault is not fully configured (need account_id, access_key_id, secret_access_key, public_url_base)".into())
    }
}

async fn cmd_upload(
    data_dir: &Path,
    files: Vec<PathBuf>,
    vault: Option<String>,
    json: bool,
) -> Result<(), String> {
    if files.is_empty() {
        return Err("no files specified".into());
    }
    let cfg = load_cfg(data_dir);
    let (vault_name, flat) = vault_flat(&cfg, vault.as_deref())?;
    require_configured(&flat)?;
    let client = R2Client::new(&flat).await?;
    let conn = db::open(data_dir).map_err(|e| e.to_string())?;

    let show_progress = io::stderr().is_terminal() && !json;
    let mut results = Vec::new();

    for path in files {
        if !path.is_file() {
            return Err(format!("not a file: {}", path.display()));
        }
        let desired = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let key = client.allocate_key(desired, None).await?;

        let on_progress: ProgressFn = if show_progress {
            let pb = ProgressBar::new(0);
            pb.set_style(
                ProgressStyle::with_template(
                    "{msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
                )
                .unwrap()
                .progress_chars("=>-"),
            );
            pb.set_message(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("upload")
                    .to_string(),
            );
            Arc::new(move |sent, total| {
                if pb.length() != Some(total) {
                    pb.set_length(total);
                }
                pb.set_position(sent);
                if sent >= total && total > 0 {
                    pb.finish_and_clear();
                }
            })
        } else {
            Arc::new(|_, _| {})
        };

        let result = client
            .upload_path(path.to_str().ok_or("invalid path")?, &key, on_progress)
            .await?;
        db::insert(
            &conn,
            &result.key,
            &result.display_name,
            result.size,
            &result.content_type,
            &result.url,
            &vault_name,
        )
        .map_err(|e| e.to_string())?;
        results.push(result);
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?
        );
    } else {
        for r in &results {
            println!("{}", r.url);
        }
    }
    Ok(())
}

fn cmd_ls(
    data_dir: &Path,
    vault: Option<String>,
    limit: Option<usize>,
    json: bool,
) -> Result<(), String> {
    let conn = db::open(data_dir).map_err(|e| e.to_string())?;
    let rows = db::list(&conn, vault.as_deref(), limit).map_err(|e| e.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
        );
    } else {
        for r in rows {
            println!(
                "{}\t{}\t{}\t{}",
                r.uploaded_at, r.vault, r.key, r.url
            );
        }
    }
    Ok(())
}

async fn cmd_rm(
    data_dir: &Path,
    key: String,
    vault: Option<String>,
    yes: bool,
) -> Result<(), String> {
    if !yes {
        eprint!("Delete remote object and history for key {key}? [y/N] ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let ans = line.trim().to_ascii_lowercase();
        if ans != "y" && ans != "yes" {
            eprintln!("aborted");
            return Ok(());
        }
    }

    let cfg = load_cfg(data_dir);
    let (_name, flat) = vault_flat(&cfg, vault.as_deref())?;
    if flat.is_configured() {
        let client = R2Client::new(&flat).await?;
        client.delete(&key).await?;
    } else {
        eprintln!("warning: vault not configured; removing local history only");
    }

    let conn = db::open(data_dir).map_err(|e| e.to_string())?;
    db::delete(&conn, &key).map_err(|e| e.to_string())?;
    Ok(())
}

fn cmd_url(data_dir: &Path, key: String) -> Result<(), String> {
    let conn = db::open(data_dir).map_err(|e| e.to_string())?;
    if let Ok(rec) = db::get(&conn, &key) {
        println!("{}", rec.url);
        return Ok(());
    }
    let cfg = load_cfg(data_dir);
    let flat = cfg.default_flat();
    if flat.public_url_base.is_empty() {
        return Err(format!("key not in history and no public_url_base: {key}"));
    }
    let base = flat.public_url_base.trim_end_matches('/');
    println!("{base}/{key}");
    Ok(())
}

async fn cmd_vault(data_dir: &Path, cmd: VaultCmd) -> Result<(), String> {
    match cmd {
        VaultCmd::List { json } => {
            let cfg = load_cfg(data_dir);
            if json {
                let rows: Vec<_> = cfg
                    .vaults
                    .iter()
                    .map(|v| {
                        serde_json::json!({
                            "name": v.name,
                            "bucket": v.bucket,
                            "account_id_set": !v.account_id.is_empty(),
                            "public_url_base_set": !v.public_url_base.is_empty(),
                            "default": v.name == cfg.default_vault,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
                );
            } else {
                for v in &cfg.vaults {
                    let marker = if v.name == cfg.default_vault { "*" } else { " " };
                    println!("{marker} {}\t{}", v.name, v.bucket);
                }
            }
            Ok(())
        }
        VaultCmd::Add {
            name,
            account_id,
            access_key_id,
            secret_access_key,
            bucket,
            public_url_base,
        } => {
            let mut cfg = load_cfg(data_dir);
            if cfg.vault_by_name(&name).is_some() {
                return Err(format!("vault already exists: {name}"));
            }
            let mut v = Vault::new(&name);
            v.account_id = account_id.unwrap_or_default();
            v.access_key_id = access_key_id.unwrap_or_default();
            v.secret_access_key = secret_access_key.unwrap_or_default();
            v.bucket = bucket.unwrap_or_else(|| "r2share".into());
            v.public_url_base = public_url_base.unwrap_or_default();
            v.validate()?;
            cfg.vaults.push(v);
            if cfg.vaults.len() == 1 {
                cfg.default_vault = name;
            }
            config::save(data_dir, &cfg)?;
            Ok(())
        }
        VaultCmd::Use { name } => {
            let mut cfg = load_cfg(data_dir);
            if cfg.vault_by_name(&name).is_none() {
                return Err(format!("vault not found: {name}"));
            }
            cfg.default_vault = name;
            config::save(data_dir, &cfg)?;
            Ok(())
        }
        VaultCmd::Rm { name, yes } => {
            let mut cfg = load_cfg(data_dir);
            if cfg.vault_by_name(&name).is_none() {
                return Err(format!("vault not found: {name}"));
            }
            if !yes {
                eprint!("Remove vault {name}? [y/N] ");
                let _ = io::stderr().flush();
                let mut line = String::new();
                io::stdin()
                    .read_line(&mut line)
                    .map_err(|e| e.to_string())?;
                let ans = line.trim().to_ascii_lowercase();
                if ans != "y" && ans != "yes" {
                    eprintln!("aborted");
                    return Ok(());
                }
            }
            cfg.vaults.retain(|v| v.name != name);
            if cfg.default_vault == name {
                cfg.default_vault = cfg
                    .vaults
                    .first()
                    .map(|v| v.name.clone())
                    .unwrap_or_else(|| "default".into());
            }
            config::save(data_dir, &cfg)?;
            Ok(())
        }
        VaultCmd::Export {
            path,
            no_secrets,
            names,
        } => {
            let name_list = names.map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
            });
            config::export_vaults(data_dir, &path, !no_secrets, name_list)
        }
        VaultCmd::Import { path, on_conflict } => {
            let mode = match on_conflict {
                OnConflict::Overwrite => ImportMode::Overwrite,
                OnConflict::Skip => ImportMode::SkipExisting,
            };
            let result = config::import_vaults(data_dir, &path, mode)?;
            eprintln!(
                "imported={} skipped={} overwritten={}",
                result.imported, result.skipped, result.overwritten
            );
            Ok(())
        }
        VaultCmd::Test { name } => {
            let cfg = load_cfg(data_dir);
            let (_n, flat) = vault_flat(&cfg, name.as_deref())?;
            require_configured(&flat)?;
            let client = R2Client::new(&flat).await?;
            client.test().await?;
            eprintln!("ok");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, Config};
    use clap::CommandFactory;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("r2share-cli-{label}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clap_help_parses() {
        Cli::command().debug_assert();
    }

    #[test]
    fn vault_list_add_use_export_import_against_temp_dir() {
        let dir = tmp_dir("vaults");
        // start empty
        assert!(load_cfg(&dir).vaults.is_empty());

        // simulate `vault add`
        let mut cfg = AppConfig::empty_v2();
        let mut v = Vault::new("work");
        v.account_id = "acct-fake".into();
        v.access_key_id = "AKIAFAKE".into();
        v.secret_access_key = "secret-fake".into();
        v.bucket = "b1".into();
        v.public_url_base = "https://pub.example".into();
        cfg.vaults.push(v);
        cfg.default_vault = "work".into();
        config::save(&dir, &cfg).unwrap();

        let listed = load_cfg(&dir);
        assert_eq!(listed.vaults.len(), 1);
        assert_eq!(listed.default_vault, "work");

        // add second + use
        let mut cfg = load_cfg(&dir);
        let mut v2 = Vault::new("home");
        v2.account_id = "acct-2".into();
        v2.access_key_id = "KEY2".into();
        v2.secret_access_key = "SEC2".into();
        v2.bucket = "b2".into();
        v2.public_url_base = "https://pub2.example".into();
        cfg.vaults.push(v2);
        cfg.default_vault = "home".into();
        config::save(&dir, &cfg).unwrap();
        assert_eq!(load_cfg(&dir).default_vault, "home");

        // export without secrets + import overwrite preserves
        let export_path = dir.join("exp.json");
        config::export_vaults(&dir, &export_path, false, None).unwrap();
        let raw = std::fs::read_to_string(&export_path).unwrap();
        assert!(!raw.contains("SEC2"));
        assert!(!raw.contains("secret-fake"));

        // wipe secrets on disk then import no-secrets should restore via preserve
        let mut cfg = load_cfg(&dir);
        for v in &mut cfg.vaults {
            if v.name == "home" {
                // change account but keep keys for preserve test setup:
                // actually: export has empty secrets; overwrite should keep SEC2
                v.account_id = "should-be-overwritten".into();
            }
        }
        config::save(&dir, &cfg).unwrap();
        config::import_vaults(&dir, &export_path, ImportMode::Overwrite).unwrap();
        let after = load_cfg(&dir);
        let home = after.vault_by_name("home").unwrap();
        assert_eq!(home.account_id, "acct-2"); // from export
        assert_eq!(home.secret_access_key, "SEC2"); // preserved
        assert_eq!(home.access_key_id, "KEY2");

        // arg parsing smoke
        let cli = Cli::try_parse_from([
            "r2share-cli",
            "--data-dir",
            dir.to_str().unwrap(),
            "vault",
            "list",
        ])
        .unwrap();
        assert!(matches!(cli.command, Commands::Vault { .. }));

        let cli = Cli::try_parse_from([
            "r2share-cli",
            "upload",
            "a.txt",
            "b.txt",
            "--vault",
            "work",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Upload { files, vault, json } => {
                assert_eq!(files.len(), 2);
                assert_eq!(vault.as_deref(), Some("work"));
                assert!(json);
            }
            _ => panic!("expected upload"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_data_dir_uses_xdg_or_home() {
        let d = default_data_dir();
        assert!(d.ends_with("com.summonai.r2share"));
    }

    #[test]
    fn flat_config_helper() {
        let mut cfg = AppConfig::from_v1(Config {
            account_id: "a".into(),
            bucket: "b".into(),
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
            public_url_base: "https://x".into(),
        });
        assert_eq!(cfg.default_vault, "default");
        let (name, flat) = vault_flat(&cfg, None).unwrap();
        assert_eq!(name, "default");
        assert!(flat.is_configured());
        assert!(vault_flat(&cfg, Some("missing")).is_err());
        cfg.default_vault = "nope".into();
        // invalid default with vaults present — resolve by name still works
        assert!(vault_flat(&cfg, Some("default")).is_ok());
    }
}
