use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
    Connection, Row, SqliteConnection,
};
use std::{path::Path, time::Duration};

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub async fn open(path: &Path) -> Result<SqliteConnection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(10));
    let mut db = SqliteConnection::connect_with(&options).await?;
    sqlx::migrate!().run(&mut db).await?;
    Ok(db)
}

pub async fn bind_account(db: &mut SqliteConnection, id: &str) -> Result<()> {
    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM account LIMIT 1")
        .fetch_optional(&mut *db)
        .await?;
    if existing.as_deref().is_some_and(|old| old != id) {
        bail!("Este banco pertence a outra conta Spotify. Use --database com outro arquivo.");
    }
    sqlx::query("INSERT OR IGNORE INTO account VALUES(?,?)")
        .bind(id)
        .bind(now())
        .execute(db)
        .await?;
    Ok(())
}

pub fn merge(target: &mut Value, incoming: &Value) {
    if let (Some(dst), Some(src)) = (target.as_object_mut(), incoming.as_object()) {
        for (key, value) in src {
            if value.is_null() {
                continue;
            }
            if let Some(old) = dst.get_mut(key) {
                merge(old, value);
            } else {
                dst.insert(key.clone(), value.clone());
            }
        }
    } else if let (Some(old), Some(new)) = (target.as_array(), incoming.as_array()) {
        // Keep full nested objects when a simplified response repeats their IDs.
        let values = new
            .iter()
            .map(|item| {
                let previous = item
                    .get("id")
                    .filter(|id| !id.is_null())
                    .and_then(|id| old.iter().find(|value| value.get("id") == Some(id)));
                let mut merged = previous.cloned().unwrap_or(Value::Null);
                merge(&mut merged, item);
                merged
            })
            .collect();
        *target = Value::Array(values);
    } else if !incoming.is_null() {
        *target = incoming.clone();
    }
}

fn text(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
}
fn int(v: &Value, pointer: &str) -> Option<i64> {
    v.pointer(pointer)
        .and_then(|v| v.as_i64().or_else(|| v.as_bool().map(i64::from)))
}
pub fn identity(v: &Value) -> Result<String> {
    text(v, "/linked_from/id")
        .or_else(|| text(v, "/id"))
        .filter(|v| !v.is_empty())
        .context("Objeto Spotify indisponível ou sem ID; reconciliação cancelada por segurança")
}

async fn catalog(
    db: &mut SqliteConnection,
    table: &str,
    id: &str,
    incoming: &Value,
) -> Result<Value> {
    let old: Option<String> =
        sqlx::query_scalar(&format!("SELECT raw_json FROM {table} WHERE id=?"))
            .bind(id)
            .fetch_optional(&mut *db)
            .await?;
    let mut v = old
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or(json!({}));
    merge(&mut v, incoming);
    let stamp = now();
    let sql = format!("INSERT INTO {table}(id,raw_json,created_at,updated_at{}) VALUES(?,?,?,?{}) ON CONFLICT(id) DO UPDATE SET raw_json=excluded.raw_json,updated_at=excluded.updated_at{}",
        if table == "artista" { "" } else { ",metadata_fetched_at" },
        if table == "artista" { "" } else { ",?" },
        if table == "artista" { "" } else { ",metadata_fetched_at=excluded.metadata_fetched_at" });
    let query = sqlx::query(&sql)
        .bind(id)
        .bind(v.to_string())
        .bind(&stamp)
        .bind(&stamp);
    if table == "artista" {
        query.execute(&mut *db).await?;
    } else {
        query.bind(&stamp).execute(&mut *db).await?;
    }
    let original = incoming.to_string();
    let hash = hex::encode(Sha256::digest(original.as_bytes()));
    sqlx::query("INSERT OR IGNORE INTO catalog_observation VALUES(?,?,?,?,?)")
        .bind(table)
        .bind(id)
        .bind(hash)
        .bind(original)
        .bind(stamp)
        .execute(&mut *db)
        .await?;
    Ok(v)
}

async fn artists(db: &mut SqliteConnection, table: &str, parent: &str, v: &Value) -> Result<()> {
    let Some(items) = v.get("artists").and_then(Value::as_array) else {
        return Ok(());
    };
    let link = if table == "album" {
        "album_artista"
    } else {
        "musica_artista"
    };
    let column = if table == "album" {
        "album_id"
    } else {
        "musica_id"
    };
    sqlx::query(&format!("DELETE FROM {link} WHERE {column}=?"))
        .bind(parent)
        .execute(&mut *db)
        .await?;
    for (position, item) in items.iter().enumerate() {
        let id = identity(item)?;
        let a = catalog(db, "artista", &id, item).await?;
        sqlx::query("UPDATE artista SET name=?,spotify_uri=?,spotify_url=?,href=? WHERE id=?")
            .bind(text(&a, "/name"))
            .bind(text(&a, "/uri"))
            .bind(text(&a, "/external_urls/spotify"))
            .bind(text(&a, "/href"))
            .bind(&id)
            .execute(&mut *db)
            .await?;
        sqlx::query(&format!("INSERT OR REPLACE INTO {link} VALUES(?,?,?)"))
            .bind(parent)
            .bind(id)
            .bind(position as i64)
            .execute(&mut *db)
            .await?;
    }
    Ok(())
}

pub async fn album(db: &mut SqliteConnection, incoming: &Value) -> Result<String> {
    let id = identity(incoming)?;
    let v = catalog(db, "album", &id, incoming).await?;
    sqlx::query("UPDATE album SET name=?,album_type=?,total_tracks=?,release_date=?,release_date_precision=?,spotify_uri=?,spotify_url=?,href=?,image_url=?,label=?,popularity=?,upc=?,ean=? WHERE id=?")
        .bind(text(&v,"/name")).bind(text(&v,"/album_type")).bind(int(&v,"/total_tracks"))
        .bind(text(&v,"/release_date")).bind(text(&v,"/release_date_precision"))
        .bind(text(&v,"/uri")).bind(text(&v,"/external_urls/spotify")).bind(text(&v,"/href"))
        .bind(text(&v,"/images/0/url")).bind(text(&v,"/label")).bind(int(&v,"/popularity"))
        .bind(text(&v,"/external_ids/upc")).bind(text(&v,"/external_ids/ean")).bind(&id)
        .execute(&mut *db).await?;
    artists(db, "album", &id, &v).await?;
    Ok(id)
}

pub async fn track(db: &mut SqliteConnection, incoming: &Value, album_id: &str) -> Result<String> {
    let id = identity(incoming)?;
    let v = catalog(db, "musica", &id, incoming).await?;
    sqlx::query("UPDATE musica SET album_id=COALESCE(album_id,?),name=?,duration_ms=?,track_number=CASE WHEN album_id IS NULL OR album_id=? THEN ? ELSE track_number END,disc_number=CASE WHEN album_id IS NULL OR album_id=? THEN ? ELSE disc_number END,explicit=?,is_local=?,is_playable=?,spotify_uri=?,spotify_url=?,href=?,isrc=?,popularity=?,preview_url=? WHERE id=?")
        .bind(album_id).bind(text(&v,"/name")).bind(int(&v,"/duration_ms"))
        .bind(album_id).bind(int(&v,"/track_number")).bind(album_id).bind(int(&v,"/disc_number")).bind(int(&v,"/explicit"))
        .bind(int(&v,"/is_local")).bind(int(&v,"/is_playable")).bind(text(&v,"/uri"))
        .bind(text(&v,"/external_urls/spotify")).bind(text(&v,"/href")).bind(text(&v,"/external_ids/isrc"))
        .bind(int(&v,"/popularity")).bind(text(&v,"/preview_url")).bind(&id).execute(&mut *db).await?;
    sqlx::query("INSERT INTO album_musica VALUES(?,?,?,?) ON CONFLICT(album_id,musica_id) DO UPDATE SET disc_number=COALESCE(excluded.disc_number,album_musica.disc_number),track_number=COALESCE(excluded.track_number,album_musica.track_number)")
        .bind(album_id).bind(&id).bind(int(incoming,"/disc_number")).bind(int(incoming,"/track_number"))
        .execute(&mut *db).await?;
    artists(db, "musica", &id, &v).await?;
    Ok(id)
}

pub async fn stats(db: &mut SqliteConnection) -> Result<Value> {
    let r = sqlx::query("SELECT (SELECT COUNT(*) FROM musica) tracks,(SELECT COUNT(*) FROM musica WHERE is_saved=1) liked_tracks,(SELECT COUNT(*) FROM album) albums,(SELECT COUNT(*) FROM album WHERE is_saved=1) saved_albums,(SELECT COUNT(*) FROM artista) artists,(SELECT completed_at FROM sync_job WHERE status='completed' ORDER BY completed_at DESC LIMIT 1) last_success").fetch_one(db).await?;
    Ok(
        json!({"tracks":r.get::<i64,_>("tracks"),"liked_tracks":r.get::<i64,_>("liked_tracks"),"albums":r.get::<i64,_>("albums"),"saved_albums":r.get::<i64,_>("saved_albums"),"artists":r.get::<i64,_>("artists"),"last_success":r.get::<Option<String>,_>("last_success")}),
    )
}

pub async fn finish(db: &mut SqliteConnection, job: &str) -> Result<Value> {
    let mut tx = db.begin().await?;
    let stage: String = sqlx::query_scalar("SELECT CASE WHEN sync_mode='incremental' THEN 'forbidden_incremental' ELSE stage END FROM sync_job WHERE id=?")
        .bind(job)
        .fetch_one(&mut *tx)
        .await?;
    if stage != "reconcile" {
        bail!("Reconciliação recusada: coleções ainda não verificadas");
    }
    let mut summary = json!({});
    for (kind, table) in [("track", "musica"), ("album", "album")] {
        let added: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE is_saved=0 AND id IN(SELECT item_id FROM sync_seen_item WHERE job_id=? AND item_type=?)"))
            .bind(job).bind(kind).fetch_one(&mut *tx).await?;
        let removed = sqlx::query(&format!("UPDATE {table} SET is_saved=0,updated_at=? WHERE is_saved=1 AND id NOT IN(SELECT item_id FROM sync_seen_item WHERE job_id=? AND item_type=?)"))
            .bind(now()).bind(job).bind(kind).execute(&mut *tx).await?.rows_affected();
        sqlx::query(&format!("UPDATE {table} SET is_saved=1,saved_at=(SELECT saved_at FROM sync_seen_item WHERE job_id=? AND item_type=? AND item_id={table}.id),last_seen_saved_sync=?,updated_at=? WHERE id IN(SELECT item_id FROM sync_seen_item WHERE job_id=? AND item_type=?)"))
            .bind(job).bind(kind).bind(job).bind(now()).bind(job).bind(kind).execute(&mut *tx).await?;
        summary[format!("{kind}_added")] = json!(added);
        summary[format!("{kind}_removed")] = json!(removed);
    }
    for kind in ["track", "album"] {
        sqlx::query("UPDATE library_sync_state SET account_id=COALESCE((SELECT id FROM account LIMIT 1),''),initial_import_completed_at=COALESCE(initial_import_completed_at,?),last_full_reconciliation_at=?,last_successful_job_id=?,last_coverage='full_listing',watermark_added_at=(SELECT MAX(saved_at) FROM sync_seen_item WHERE job_id=? AND item_type=?),remote_total_last_seen=(SELECT COUNT(*) FROM sync_seen_item WHERE job_id=? AND item_type=?) WHERE collection=?")
            .bind(now()).bind(now()).bind(job).bind(job).bind(kind).bind(job).bind(kind).bind(kind).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE sync_job SET status='completed',stage='done',coverage='full_listing',completed_at=?,updated_at=?,summary_json=?,last_error=NULL WHERE id=?")
        .bind(now()).bind(now()).bind(summary.to_string()).bind(job).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(summary)
}
