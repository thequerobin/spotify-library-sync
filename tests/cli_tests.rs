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

fn interactive(path: &Path, input: &str) -> std::process::Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_spotify-sync"))
        .arg("--database")
        .arg(path)
        .env_remove("SPOTIFY_CLIENT_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
#[test]
fn menu_opens_without_arguments_handles_invalid_choice_and_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("menu.db");
    let output = interactive(&path, "invalid\n11\n0\n");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Opção inválida"));
    assert!(text.contains("Criar LSUA"));
    assert!(text.contains("Criar USSA"));
    assert!(text.contains("liked_tracks"));
    assert!(interactive(&path, "").status.success());
}
#[tokio::test]
async fn local_playlist_preview_and_cancelled_menu_creation_need_no_credentials() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("menu.db");
    let mut db = database::open(&path).await?;
    database::album(&mut db, &json!({"id":"a","release_date":"2020"})).await?;
    database::track(
        &mut db,
        &json!({"id":"t","uri":"spotify:track:t","name":"Song"}),
        "a",
    )
    .await?;
    sqlx::query("UPDATE musica SET is_saved=1,saved_at='2026-02-01T00:00:00Z'")
        .execute(&mut db)
        .await?;
    db.close().await?;
    let output = invoke(
        &path,
        &["playlist", "preview", "--kind", "lsua", "--from", "02/2026"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(preview["count"], 1);
    assert_eq!(preview["name"], "LSUA - 02-2026");
    let rejected = invoke(&path, &["playlist", "create", "--kind", "lsua", "--yes"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("--allow-incomplete"));
    let output = interactive(&path, "7\n\n\n\n\n\nn\n0\n");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Criação cancelada"), "{text}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Credenciais"));
    let mut db = database::open(&path).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM playlist_job")
            .fetch_one(&mut db)
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn cooldown_formats_commands_and_menu_and_keeps_previews_offline() -> Result<()> {
    use chrono::{Duration, Local, Utc};
    use spotify_library_sync::{rate_limit::CooldownActive, sync};

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("cooldown.db");
    let mut db = database::open(&path).await?;
    let deadline = Utc::now() + Duration::hours(1);
    sqlx::query("INSERT INTO api_cooldown(id,retry_at,reason) VALUES(1,?,'test')")
        .bind(deadline.to_rfc3339())
        .execute(&mut db)
        .await?;
    let error = sync::check_cooldown(&mut db).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<CooldownActive>().unwrap().retry_at,
        deadline
    );
    db.close().await?;

    let expected = deadline
        .with_timezone(&Local)
        .format("%d/%m/%Y às %H:%M:%S (UTC%:z)")
        .to_string();
    for args in [
        vec!["check-saved", "--uri", "spotify:track:test"],
        vec!["discography", "sync"],
    ] {
        let output = invoke(&path, &args);
        assert!(!output.status.success());
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(text.starts_with("\nLimite de requisições ativo.\n"), "{text}");
        assert!(text.contains(&expected), "{text}");
        assert!(text.contains("Tempo restante:"));
        assert!(text.ends_with("Nenhuma requisição foi enviada.\n\n"));
        assert!(!text.contains('\u{1b}'));
    }
    let output = interactive(&path, "4\n0\n");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains(&expected), "{text}");
    assert!(text.contains("Nenhuma requisição foi enviada."));
    assert!(String::from_utf8_lossy(&output.stdout).contains("0. Sair"));
    let output = invoke(&path, &["playlist", "preview", "--kind", "lsua"]);
    assert!(output.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&output.stdout)?["count"], 0);

    let mut db = database::open(&path).await?;
    let stored: String = sqlx::query_scalar("SELECT retry_at FROM api_cooldown WHERE id=1")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(stored, deadline.to_rfc3339());
    sqlx::query("UPDATE api_cooldown SET retry_at=? WHERE id=1")
        .bind((Utc::now() - Duration::seconds(1)).to_rfc3339())
        .execute(&mut db)
        .await?;
    sync::check_cooldown(&mut db).await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn command_uses_system_timezone_in_child_process() -> Result<()> {
    use chrono::{Duration, FixedOffset, Utc};

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("timezone.db");
    let mut db = database::open(&path).await?;
    let deadline = Utc::now() + Duration::days(1);
    sqlx::query("INSERT INTO api_cooldown(id,retry_at,reason) VALUES(1,?,'test')")
        .bind(deadline.to_rfc3339())
        .execute(&mut db)
        .await?;
    db.close().await?;
    for (zone, offset) in [("UTC", 0), ("Asia/Kathmandu", 20700)] {
        let output = Command::new(env!("CARGO_BIN_EXE_spotify-sync"))
            .arg("--database")
            .arg(&path)
            .args(["check-saved", "--uri", "spotify:track:test"])
            .env("TZ", zone)
            .env_remove("SPOTIFY_CLIENT_ID")
            .output()?;
        assert!(!output.status.success());
        let expected = deadline
            .with_timezone(&FixedOffset::east_opt(offset).unwrap())
            .format("%d/%m/%Y às %H:%M:%S (UTC%:z)")
            .to_string();
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(text.contains(&expected), "{text}");
    }
    Ok(())
}
