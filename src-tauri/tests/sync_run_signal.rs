//! `sync run` must keep watching when stdin is /dev/null, and exit on SIGTERM.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn tmp() -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("r2share-sigtest-{n}"));
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn sync_run_survives_stdin_eof_and_exits_on_sigterm() {
    let data = tmp();
    let folder = data.join("sync-root");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("a.txt"), b"hi").unwrap();

    let cfg = serde_json::json!({
        "version": 2,
        "default_vault": "work",
        "vaults": [{
            "name": "work",
            "account_id": "a",
            "bucket": "b",
            "access_key_id": "K",
            "secret_access_key": "S",
            "public_url_base": "https://example.invalid"
        }],
        "folder_mappings": [{
            "path": folder.display().to_string(),
            "vault": "work",
            "paused": false
        }]
    });
    fs::write(data.join("config.json"), cfg.to_string()).unwrap();

    let bin = env!("CARGO_BIN_EXE_r2share-cli");
    let mut child = Command::new(bin)
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "sync",
            "run",
            "--dry-run",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sync run");

    // Still alive after ~3s with stdin closed.
    std::thread::sleep(Duration::from_secs(3));
    match child.try_wait().expect("try_wait") {
        None => {} // still running — good
        Some(status) => panic!(
            "sync run exited early with {status:?} (stdin EOF should be ignored)"
        ),
    }

    // SIGTERM → clean exit
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success(), "kill -TERM failed");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(
                status.success() || status.code() == Some(0) || status.code().is_none(),
                "expected clean shutdown, got {status:?}"
            );
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("sync run did not exit within 10s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = fs::remove_dir_all(&data);
}
