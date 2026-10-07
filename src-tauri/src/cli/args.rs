use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "r2share-cli",
    about = "Upload to Cloudflare R2 and manage r2share vaults (headless companion to the tray app)",
    version
)]
pub struct Cli {
    /// Override the shared app data directory (config.json + r2share.db).
    #[arg(long, global = true, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Upload one or more files to R2 and record them in history.
    Upload {
        /// Local file paths to upload.
        #[arg(required = true)]
        files: Vec<PathBuf>,

        /// Vault to use (defaults to the configured default vault).
        #[arg(long)]
        vault: Option<String>,

        /// Print results as JSON instead of one URL per line.
        #[arg(long)]
        json: bool,
    },

    /// List upload history.
    Ls {
        /// Filter by vault name.
        #[arg(long)]
        vault: Option<String>,

        /// Max rows to return.
        #[arg(long)]
        limit: Option<usize>,

        #[arg(long)]
        json: bool,
    },

    /// Delete a remote object and its history row.
    Rm {
        /// R2 object key.
        key: String,

        /// Vault whose credentials to use for the remote delete.
        #[arg(long)]
        vault: Option<String>,

        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },

    /// Print the public URL for a key (from history, or constructed from the default vault).
    Url {
        key: String,
    },

    /// Manage named R2 credential vaults.
    Vault {
        #[command(subcommand)]
        cmd: VaultCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum VaultCmd {
    /// List vaults (names, bucket, default marker — never secrets).
    List {
        #[arg(long)]
        json: bool,
    },

    /// Add a vault. Prefer env vars for secrets so they stay out of argv.
    Add {
        name: String,

        #[arg(long, env = "R2_ACCOUNT_ID")]
        account_id: Option<String>,

        #[arg(long, env = "R2_ACCESS_KEY_ID")]
        access_key_id: Option<String>,

        #[arg(long, env = "R2_SECRET_ACCESS_KEY")]
        secret_access_key: Option<String>,

        #[arg(long, env = "R2_BUCKET")]
        bucket: Option<String>,

        #[arg(long, env = "R2_PUBLIC_URL_BASE")]
        public_url_base: Option<String>,
    },

    /// Set the default vault.
    Use {
        name: String,
    },

    /// Remove a vault from config.
    Rm {
        name: String,
        #[arg(long)]
        yes: bool,
    },

    /// Export vaults to a JSON file (0600 on Unix).
    Export {
        path: PathBuf,
        /// Blank access/secret keys in the export.
        #[arg(long)]
        no_secrets: bool,
        /// Comma-separated vault names (default: all).
        #[arg(long, value_name = "a,b")]
        names: Option<String>,
    },

    /// Import vaults from a JSON file.
    Import {
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = OnConflict::Skip)]
        on_conflict: OnConflict,
    },

    /// Test R2 connectivity for a vault (HeadBucket).
    Test {
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum OnConflict {
    Overwrite,
    #[default]
    Skip,
}
