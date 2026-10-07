//! One-shot: load config from a data-dir path (migrates v1→v2 in that dir only).
//! Prints vault count / field names and non-empty flags — never secret values.
fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: migrate_check <data-dir>");
    let cfg = r2share_lib::config::load(std::path::Path::new(&dir));
    println!("version={}", cfg.version);
    println!("default_vault={}", cfg.default_vault);
    println!("vault_count={}", cfg.vaults.len());
    for v in &cfg.vaults {
        println!(
            "vault name={:?} fields: account_id={} access_key_id={} secret_access_key={} bucket={} public_url_base={}",
            v.name,
            !v.account_id.is_empty(),
            !v.access_key_id.is_empty(),
            !v.secret_access_key.is_empty(),
            !v.bucket.is_empty(),
            !v.public_url_base.is_empty(),
        );
    }
    println!("folder_mappings={}", cfg.folder_mappings.len());
}
