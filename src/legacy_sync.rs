use crate::{
    database::{self, now},
    spotify::{Api, Page},
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::HashSet;

#[derive(Debug, thiserror::Error)]
#[error("A biblioteca mudou durante a paginação. O estado salvo foi preservado; execute sync --resume para refazer esta coleção.")]
struct Unstable;

pub async fn check_cooldown(db: &mut SqliteConnection) -> Result<()> {
    let deadline: Option<String> = sqlx::query_scalar::<_, Option<String>>("SELECT MAX(retry_at) FROM (SELECT retry_at FROM sync_job WHERE status='rate_limited' AND retry_at IS NOT NULL UNION ALL SELECT retry_at FROM api_cooldown)")
        .fetch_one(db).await?;
    if let Some(deadline) = deadline {
        if chrono::DateTime::parse_from_rfc3339(&deadline)? > Utc::now() {
            bail!("Rate limit ativo até {deadline}; nenhuma requisição foi enviada");
        }
    }
    Ok(())
}

pub async fn prepare(db: &mut SqliteConnection, resume: bool, full: bool) -> Result<String> {
    let existing =
        sqlx::query("SELECT id,status,retry_at FROM sync_job WHERE status<>'completed' LIMIT 1")
            .fetch_optional(&mut *db)
            .await?;
    if let Some(row) = existing {
        if !resume {
            bail!("Há uma sincronização pendente. Use sync --resume ou sync --restart");
        }
        if let Some(retry) = row.get::<Option<String>, _>("retry_at") {
            let date = chrono::DateTime::parse_from_rfc3339(&retry)?;
            if date > Utc::now() {
                bail!("Rate limit ativo até {retry}; retome depois desse horário");
            }
        }
        let id: String = row.get("id");
        sqlx::query("UPDATE sync_job SET status='running',retry_at=NULL,last_error=NULL,updated_at=? WHERE id=?")
            .bind(now()).bind(&id).execute(db).await?;
        return Ok(id);
    }
    if resume {
        bail!("Nenhuma sincronização pendente");
    }
    let id = format!(
        "{}-{:016x}",
        Utc::now().timestamp_millis(),
        rand::random::<u64>()
    );
    sqlx::query("INSERT INTO sync_job(id,job_type,status,stage,started_at,updated_at) VALUES(?,?,'running','tracks',?,?)")
        .bind(&id).bind(if full {"full"} else {"normal"}).bind(now()).bind(now()).execute(db).await?;
    Ok(id)
}

pub(crate) async fn stage(db: &mut SqliteConnection, job: &str, next: &str) -> Result<()> {
    sqlx::query("UPDATE sync_job SET stage=?,next_offset=0,total_items=NULL,processed_items=0,updated_at=? WHERE id=?")
        .bind(next).bind(now()).bind(job).execute(db).await?;
    Ok(())
}

pub(crate) fn fingerprint(page: &Page, kind: &str) -> Result<String> {
    let mut hash = Sha256::new();
    for item in &page.items {
        let obj = &item[if kind == "track" { "track" } else { "album" }];
        let id = database::identity(obj)?;
        let date = saved_at(item)?;
        hash.update(serde_json::to_vec(&(id, date))?);
        hash.update(b"\n");
    }
    Ok(hex::encode(hash.finalize()))
}
pub(crate) fn saved_at(item: &Value) -> Result<String> {
    let date = item["added_at"]
        .as_str()
        .context("Item sem added_at; reconciliação bloqueada")?;
    Ok(chrono::DateTime::parse_from_rfc3339(date)?
        .with_timezone(&Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

pub(crate) async fn import_album(
    db: &mut SqliteConnection,
    job: &str,
    obj: &Value,
    full: bool,
    ttl_days: u32,
) -> Result<String> {
    let id = database::album(db, obj).await?;
    let cache = sqlx::query("SELECT tracks_fetched_at,tracks_imported_total FROM album WHERE id=?")
        .bind(&id)
        .fetch_one(&mut *db)
        .await?;
    let fetched: Option<String> = cache.get("tracks_fetched_at");
    let imported_total: Option<i64> = cache.get("tracks_imported_total");
    let fresh = fetched
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .is_some_and(|d| d > Utc::now() - chrono::Duration::days(ttl_days as i64));
    let mode: String = sqlx::query_scalar("SELECT sync_mode FROM sync_job WHERE id=?")
        .bind(job)
        .fetch_one(&mut *db)
        .await?;
    let economy = mode == "incremental" || mode == "reconcile";
    if !full
        && (fresh
            || (economy
                && cache
                    .get::<Option<String>, _>("tracks_fetched_at")
                    .is_some()))
        && imported_total == obj["total_tracks"].as_i64()
    {
        return Ok(id);
    }
    let embedded = obj.get("tracks").filter(|v| v.is_object());
    let mut offset = 0;
    let mut total = None;
    let mut done = false;
    if let Some(tracks) = embedded {
        let page: Page =
            serde_json::from_value(tracks.clone()).context("Página de faixas do álbum inválida")?;
        done = page.validate(0, None)?;
        for track in &page.items {
            database::track(db, track, &id).await?;
        }
        offset = page.items.len() as i64;
        total = Some(page.total);
    }
    sqlx::query("INSERT INTO sync_album_queue(job_id,album_id,status,next_offset,total_items,updated_at) VALUES(?,?,?,?,?,?) ON CONFLICT(job_id,album_id) DO UPDATE SET status=excluded.status,next_offset=excluded.next_offset,total_items=excluded.total_items,updated_at=excluded.updated_at WHERE sync_album_queue.total_items IS NOT excluded.total_items")
        .bind(job).bind(&id).bind(if done {"completed"} else {"pending"}).bind(offset).bind(total).bind(now())
        .execute(&mut *db).await?;
    if done {
        sqlx::query("UPDATE album SET tracks_fetched_at=?,tracks_imported_total=? WHERE id=?")
            .bind(now())
            .bind(total)
            .bind(&id)
            .execute(db)
            .await?;
    }
    Ok(id)
}

async fn collect<A: Api>(
    db: &mut SqliteConnection,
    api: &mut A,
    job: &str,
    kind: &str,
    full: bool,
    ttl: u32,
) -> Result<()> {
    let row = sqlx::query("SELECT next_offset,total_items FROM sync_job WHERE id=?")
        .bind(job)
        .fetch_one(&mut *db)
        .await?;
    let mut offset: i64 = row.get("next_offset");
    let mut total: Option<i64> = row.get("total_items");
    let endpoint = if kind == "track" {
        "/me/tracks"
    } else {
        "/me/albums"
    };
    let bar = ProgressBar::new(total.unwrap_or(0) as u64);
    bar.set_style(ProgressStyle::with_template(
        "{msg} [{bar:30}] {pos}/{len}",
    )?);
    bar.set_message(if kind == "track" {
        "Músicas curtidas"
    } else {
        "Álbuns salvos"
    });
    bar.set_position(offset as u64);
    while total.is_none() || offset < total.unwrap() {
        let page: Page = serde_json::from_value(api.get(endpoint, Some(offset)).await?)?;
        if page.validate(offset, total).is_err() {
            return Err(Unstable.into());
        }
        let fp = fingerprint(&page, kind)?;
        let mut tx = db.begin().await?;
        let mut ids = HashSet::new();
        for item in &page.items {
            let obj = &item[if kind == "track" { "track" } else { "album" }];
            let id = if kind == "track" {
                let album = database::album(&mut tx, &obj["album"]).await?;
                database::track(&mut tx, obj, &album).await?
            } else {
                import_album(&mut tx, job, obj, full, ttl).await?
            };
            if !ids.insert(id.clone()) {
                return Err(Unstable.into());
            }
            let inserted = sqlx::query("INSERT OR IGNORE INTO sync_seen_item VALUES(?,?,?,?)")
                .bind(job)
                .bind(kind)
                .bind(&id)
                .bind(saved_at(item)?)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if inserted != 1 {
                return Err(Unstable.into());
            }
        }
        let next = offset + page.items.len() as i64;
        sqlx::query("INSERT OR REPLACE INTO sync_page VALUES(?,?,?,?,?)")
            .bind(job)
            .bind(kind)
            .bind(offset)
            .bind(fp)
            .bind(page.total)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE sync_job SET next_offset=?,processed_items=?,total_items=?,updated_at=? WHERE id=?")
            .bind(next).bind(next).bind(page.total).bind(now()).bind(job).execute(&mut *tx).await?;
        tx.commit().await?;
        offset = next;
        total = Some(page.total);
        bar.set_length(page.total as u64);
        bar.set_position(next as u64);
    }
    bar.finish_and_clear();
    // Offset pagination has no snapshot token. Recheck EVERY page, not just the first or total.
    let verify = ProgressBar::new(total.unwrap_or(0) as u64);
    verify.set_style(ProgressStyle::with_template(
        "{msg} [{bar:30}] {pos}/{len}",
    )?);
    verify.set_message(format!("Verificando {kind}"));
    let mut offset = 0;
    loop {
        let page: Page = serde_json::from_value(api.get(endpoint, Some(offset)).await?)?;
        if page.validate(offset, total).is_err() {
            return Err(Unstable.into());
        }
        let old: Option<String> = sqlx::query_scalar("SELECT fingerprint FROM sync_page WHERE job_id=? AND item_type=? AND page_offset=? AND total=?")
            .bind(job).bind(kind).bind(offset).bind(page.total).fetch_optional(&mut *db).await?;
        if old.as_deref() != Some(fingerprint(&page, kind)?.as_str()) {
            return Err(Unstable.into());
        }
        offset += page.items.len() as i64;
        verify.set_position(offset as u64);
        if page.next.is_none() {
            break;
        }
    }
    verify.finish_and_clear();
    let seen: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sync_seen_item WHERE job_id=? AND item_type=?")
            .bind(job)
            .bind(kind)
            .fetch_one(&mut *db)
            .await?;
    if seen != total.unwrap_or(0) {
        return Err(Unstable.into());
    }
    Ok(())
}

async fn reset_collection(db: &mut SqliteConnection, job: &str, kind: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM sync_seen_item WHERE job_id=? AND item_type=?")
        .bind(job)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sync_page WHERE job_id=? AND item_type=?")
        .bind(job)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    if kind == "album" {
        sqlx::query("DELETE FROM sync_album_queue WHERE job_id=?")
            .bind(job)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE sync_job SET next_offset=0,processed_items=0,total_items=NULL,updated_at=? WHERE id=?")
        .bind(now()).bind(job).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn album_tracks<A: Api>(db: &mut SqliteConnection, api: &mut A, job: &str) -> Result<()> {
    loop {
        let row = sqlx::query("SELECT album_id,next_offset,total_items FROM sync_album_queue WHERE job_id=? AND status<>'completed' ORDER BY album_id LIMIT 1")
            .bind(job).fetch_optional(&mut *db).await?;
        let Some(row) = row else {
            return Ok(());
        };
        let album: String = row.get("album_id");
        let offset: i64 = row.get("next_offset");
        let total: Option<i64> = row.get("total_items");
        let progress = ProgressBar::new(total.unwrap_or(0) as u64);
        progress.set_style(ProgressStyle::with_template("{msg} {pos}/{len}")?);
        progress.set_message(format!("Faixas do álbum {album}"));
        progress.set_position(offset as u64);
        sqlx::query("UPDATE sync_album_queue SET attempts=attempts+1,updated_at=? WHERE job_id=? AND album_id=?")
            .bind(now()).bind(job).bind(&album).execute(&mut *db).await?;
        let response = api
            .get(&format!("/albums/{album}/tracks"), Some(offset))
            .await;
        let page: Page = match response {
            Ok(value) => serde_json::from_value(value)?,
            Err(error) => {
                sqlx::query("UPDATE sync_album_queue SET last_error=?,updated_at=? WHERE job_id=? AND album_id=?")
                    .bind(error.to_string()).bind(now()).bind(job).bind(&album).execute(&mut *db).await?;
                return Err(error);
            }
        };
        let done = match page.validate(offset, total) {
            Ok(done) => done,
            Err(error) => {
                sqlx::query("UPDATE sync_album_queue SET next_offset=0,total_items=NULL,last_error=?,updated_at=? WHERE job_id=? AND album_id=?")
                    .bind(error.to_string()).bind(now()).bind(job).bind(&album).execute(&mut *db).await?;
                return Err(error);
            }
        };
        let mut tx = db.begin().await?;
        for track in &page.items {
            database::track(&mut tx, track, &album).await?;
        }
        sqlx::query("UPDATE sync_album_queue SET status=?,next_offset=?,total_items=?,last_error=NULL,updated_at=? WHERE job_id=? AND album_id=?")
            .bind(if done {"completed"} else {"running"}).bind(offset+page.items.len() as i64).bind(page.total).bind(now()).bind(job).bind(&album)
            .execute(&mut *tx).await?;
        if done {
            sqlx::query("UPDATE album SET tracks_fetched_at=?,tracks_imported_total=? WHERE id=?")
                .bind(now())
                .bind(page.total)
                .bind(&album)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        progress.finish_and_clear();
    }
}

pub async fn run<A: Api>(
    db: &mut SqliteConnection,
    api: &mut A,
    job: &str,
    ttl: u32,
) -> Result<Value> {
    loop {
        let row = sqlx::query("SELECT stage,job_type FROM sync_job WHERE id=?")
            .bind(job)
            .fetch_one(&mut *db)
            .await?;
        let current: String = row.get("stage");
        let full = row.get::<String, _>("job_type") == "full";
        match current.as_str() {
            "tracks" | "albums" => {
                let kind = if current == "tracks" {
                    "track"
                } else {
                    "album"
                };
                if let Err(error) = collect(db, api, job, kind, full, ttl).await {
                    if error.is::<Unstable>() {
                        reset_collection(db, job, kind).await?;
                    }
                    return Err(error);
                }
                stage(
                    db,
                    job,
                    if current == "tracks" {
                        "albums"
                    } else {
                        "album_tracks"
                    },
                )
                .await?;
            }
            "album_tracks" => {
                album_tracks(db, api, job).await?;
                stage(db, job, "reconcile").await?;
            }
            "reconcile" => return database::finish(db, job).await,
            "done" => return Ok(json!({"status":"completed"})),
            _ => bail!("Etapa desconhecida: {current}"),
        }
    }
}
