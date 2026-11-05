use anyhow::Result;
use fs2::FileExt;
use serde_json::{json, Value};
use spotify_library_sync::database;
use sqlx::Connection;
use std::{fs::OpenOptions, path::Path, process::Command};

fn invoke(path: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_spotify-sync"))
        .arg("--database")
        .arg(path)
        .args(args)
        .env_remove("SPOTIFY_CLIENT_ID")
        .env_remove("SPOTIFY_REDIRECT_URI")
        .output()
        .unwrap()
}
#[tokio::test]
async fn offline_filters_dates_artist_and_pagination_work_without_credentials() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("library.db");
    let mut db = database::open(&path).await?;
    let album = json!({"id":"a","name":"Album","release_date":"2026-02","artists":[{"id":"artist","name":"Singer"}]});
    database::album(&mut db, &album).await?;
    for (id, date) in [
        ("one", "2026-02-01T00:00:00.000Z"),
        ("two", "2026-02-28T23:59:59.999Z"),
        ("three", "2026-03-01T00:00:00.000Z"),
    ] {
        database::track(
            &mut db,
            &json!({"id":id,"name":id,"artists":[{"id":"artist","name":"Singer"}]}),
            "a",
        )
        .await?;
        sqlx::query("UPDATE musica SET is_saved=1,saved_at=? WHERE id=?")
            .bind(date)
            .bind(id)
            .execute(&mut db)
            .await?;
    }
    db.close().await?;
    let output = invoke(
        &path,
        &[
            "tracks",
            "--liked",
            "--album-not-saved",
            "--from",
            "2026-02",
            "--to",
            "2026-02",
            "--release",
            "2026",
            "--artist",
            "singer",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(data.as_array().unwrap().len(), 2);
    let output = invoke(&path, &["tracks", "--artist", "' OR 1=1 --"]);
    assert_eq!(serde_json::from_slice::<Value>(&output.stdout)?, json!([]));
    let output = invoke(&path, &["tracks", "--limit", "1", "--offset", "1"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let output = invoke(&path, &["tracks", "--from", "2026-02-30"]);
    assert!(!output.status.success());
    let output = invoke(&path, &["tracks", "--from", "2026-03", "--to", "2026-02"]);
    assert!(!output.status.success());
    let output = invoke(&path, &["stats"]);
    assert!(output.status.success());
    let output = invoke(&path, &["status"]);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?["job"],
        Value::Null
    );
    Ok(())
}
#[test]
fn another_process_cannot_open_locked_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lockfile"))
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    let output = invoke(&path, &["sync"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Outra instância"));
    let read = invoke(&path, &["status"]);
    assert!(
        read.status.success(),
        "Status deve continuar disponível durante a sincronização"
    );
}
