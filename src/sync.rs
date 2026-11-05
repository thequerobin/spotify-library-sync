//! Incremental orchestration. Legacy full scans remain available for v0.1.0 jobs.
use crate::{
    database::{self, now},
    legacy_sync,
    spotify::{Api, BudgetExhausted, Page, Paused},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::HashSet;

pub use legacy_sync::{check_cooldown, prepare};

#[derive(Default, Clone, Debug)]
pub struct Options {
    pub full: bool,
    pub reconcile: bool,
    pub collection_budget: Option<u32>,
    pub album_budget: Option<u32>,
    pub known_pages: Option<u32>,
}
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct WorkPaused {
    pub reason: &'static str,
    pub message: String,
}
fn window_changed() -> anyhow::Error {
    WorkPaused {reason:"window_changed", message:"A janela recente mudou. Progresso de catálogo preservado; retome com sync --resume. Nenhuma varredura completa foi iniciada.".into()}.into()
}

pub async fn prepare_sync(
    db: &mut SqliteConnection,
    resume: bool,
    options: &Options,
) -> Result<String> {
    check_cooldown(db).await?;
    if options.full && options.reconcile {
        bail!("Escolha apenas --full ou --reconcile");
    }
    if options.collection_budget.is_some_and(|n| n < 5)
        || options.album_budget == Some(0)
        || options.known_pages.is_some_and(|n| !(1..=10).contains(&n))
    {
        bail!("Orçamento de coleção deve ser >=5, orçamento de faixas >=1 e páginas conhecidas entre 1 e 10");
    }
    if let Some(job) =
        sqlx::query_scalar::<_, String>("SELECT id FROM sync_job WHERE status<>'completed'")
            .fetch_optional(&mut *db)
            .await?
    {
        if !resume {
            bail!("Há uma sincronização pendente. Use sync --resume ou sync --restart");
        }
        sqlx::query("UPDATE sync_job SET status='running',last_error=NULL,pause_reason=NULL,retry_at=NULL,collection_budget=COALESCE(?,collection_budget),album_budget=COALESCE(?,album_budget),known_pages=COALESCE(?,known_pages),updated_at=? WHERE id=?")
            .bind(options.collection_budget).bind(options.album_budget).bind(options.known_pages).bind(now()).bind(&job).execute(&mut *db).await?;
        return Ok(job);
    }
    if resume {
        bail!("Nenhuma sincronização pendente");
    }
    let complete: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM library_sync_state WHERE initial_import_completed_at IS NOT NULL",
    )
    .fetch_one(&mut *db)
    .await?;
    let mode = if options.full {
        "full"
    } else if options.reconcile {
        "reconcile"
    } else if complete == 2 {
        "incremental"
    } else {
        "initial"
    };
    // Existing confirmed saved states without provenance are not an invitation to a silent full scan.
    if mode == "initial" {
        let existing:i64=sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM musica WHERE is_saved=1)+(SELECT COUNT(*) FROM album WHERE is_saved=1)").fetch_one(&mut *db).await?;
        if existing > 0 {
            bail!("Há estados salvos sem comprovação de importação inicial concluída. Use --reconcile explicitamente; o banco foi preservado");
        }
    }
    let id = format!(
        "{}-{:016x}",
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u64>()
    );
    let mut tx = db.begin().await?;
    sqlx::query("INSERT INTO sync_job(id,job_type,sync_mode,status,stage,coverage,collection_budget,album_budget,known_pages,started_at,updated_at) VALUES(?,?,?,'running','tracks',?,?,?,?,?,?)")
        .bind(&id).bind(mode).bind(mode).bind(if mode=="incremental" {"recent_window"} else {"full_listing"})
        .bind(options.collection_budget.unwrap_or(20)).bind(options.album_budget.unwrap_or(20)).bind(options.known_pages.unwrap_or(2)).bind(now()).bind(now()).execute(&mut *tx).await?;
    // Preserve unfinished album work across --restart, including previously published scopes.
    sqlx::query("INSERT INTO sync_album_queue(job_id,album_id,status,next_offset,total_items,attempts,last_error,updated_at) SELECT ?,album_id,'pending',next_offset,total_items,attempts,last_error,updated_at FROM (SELECT *,ROW_NUMBER() OVER(PARTITION BY album_id ORDER BY updated_at DESC,job_id DESC) n FROM sync_album_queue WHERE status IN('pending','running')) WHERE n=1")
        .bind(&id).execute(&mut *tx).await?;
    sqlx::query("UPDATE sync_album_queue SET status='superseded' WHERE job_id<>? AND status IN('pending','running')").bind(&id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn restart(db: &mut SqliteConnection) -> Result<()> {
    check_cooldown(db).await?;
    let mut tx = db.begin().await?;
    // Preserve queue FK/provenance. An abandoned job never establishes a full baseline.
    sqlx::query("UPDATE sync_job SET status='completed',stage='abandoned',pause_reason='restarted',updated_at=? WHERE status<>'completed'").bind(now()).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM sync_baseline_item WHERE job_id IN(SELECT id FROM sync_job WHERE stage='abandoned')").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn remember_cooldown(db: &mut SqliteConnection, error: &anyhow::Error) -> Result<()> {
    if let Some(pause) = error.downcast_ref::<Paused>() {
        let deadline = pause
            .retry_at
            .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::seconds(60))
            .to_rfc3339();
        sqlx::query("INSERT INTO api_cooldown(id,retry_at,reason) VALUES(1,?,?) ON CONFLICT(id) DO UPDATE SET retry_at=MAX(api_cooldown.retry_at,excluded.retry_at),reason=excluded.reason")
            .bind(deadline).bind(error.to_string()).execute(db).await?;
    }
    Ok(())
}

pub async fn record_error(
    db: &mut SqliteConnection,
    job: &str,
    error: &anyhow::Error,
) -> Result<()> {
    remember_cooldown(db, error).await?;
    let quota = error.downcast_ref::<Paused>();
    let work = error.downcast_ref::<WorkPaused>();
    let budget = error.is::<BudgetExhausted>();
    let status = if quota.is_some() {
        "rate_limited"
    } else if work.is_some() || budget {
        "paused"
    } else {
        "failed"
    };
    let reason = if budget {
        Some("request_budget_exhausted")
    } else {
        work.map(|p| p.reason)
    };
    sqlx::query("UPDATE sync_job SET status=?,pause_reason=?,last_error=?,error_count=error_count+1,retry_at=?,updated_at=?,coverage=CASE WHEN ? IN('window_changed','ordering_inconclusive') THEN 'inconclusive' ELSE coverage END WHERE id=?")
        .bind(status).bind(reason).bind(error.to_string()).bind(quota.and_then(|p|p.retry_at.map(|t|t.to_rfc3339()))).bind(now()).bind(reason).bind(job).execute(db).await?;
    Ok(())
}

struct Budget {
    limit: u32,
    used: u32,
}
impl Budget {
    async fn new(db: &mut SqliteConnection, job: &str, kind: &str, limit: u32) -> Result<Self> {
        sqlx::query("INSERT INTO sync_scope(job_id,collection) VALUES(?,?) ON CONFLICT DO NOTHING")
            .bind(job)
            .bind(kind)
            .execute(&mut *db)
            .await?;
        sqlx::query("UPDATE sync_scope SET invocation_requests=0,request_limit=? WHERE job_id=? AND collection=?").bind(limit).bind(job).bind(kind).execute(db).await?;
        Ok(Self { limit, used: 0 })
    }
    async fn get<A: Api>(
        &mut self,
        db: &mut SqliteConnection,
        api: &mut A,
        job: &str,
        kind: &str,
        path: &str,
        offset: i64,
    ) -> Result<Value> {
        if self.used >= self.limit {
            return Err(BudgetExhausted.into());
        }
        api.set_budget(Some(self.limit - self.used));
        let before = api.attempt_count();
        let result = api.get(path, Some(offset)).await;
        self.used += u32::try_from(api.attempt_count().saturating_sub(before))
            .unwrap_or(u32::MAX)
            .max(1);
        api.set_budget(None);
        sqlx::query("UPDATE sync_scope SET requests_total=requests_total+?,invocation_requests=? WHERE job_id=? AND collection=?")
            .bind(api.attempt_count().saturating_sub(before).max(1) as i64).bind(self.used).bind(job).bind(kind).execute(db).await?;
        result
    }
}
async fn initialize_scope(db: &mut SqliteConnection, job: &str, kind: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM sync_scope WHERE job_id=? AND collection=?")
            .bind(job)
            .bind(kind)
            .fetch_optional(&mut *tx)
            .await?;
    if state.is_none() || state.as_deref() == Some("pending") {
        let table = if kind == "track" { "musica" } else { "album" };
        sqlx::query("INSERT INTO sync_scope(job_id,collection,state) VALUES(?,?,'collecting') ON CONFLICT(job_id,collection) DO UPDATE SET state='collecting'")
            .bind(job).bind(kind).execute(&mut *tx).await?;
        sqlx::query(&format!(
            "INSERT INTO sync_baseline_item SELECT ?,?,id,saved_at FROM {table} WHERE is_saved=1"
        ))
        .bind(job)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
async fn reset_window(db: &mut SqliteConnection, job: &str, kind: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    for (table, column) in [("sync_seen_item", "item_type"), ("sync_page", "item_type")] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE job_id=? AND {column}=?"
        ))
        .bind(job)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE sync_scope SET state='collecting',next_offset=0,pages_processed=0,consecutive_known=0,changed_items=0,previous_added_at=NULL,head_signature=NULL,needs_head_check=0,stop_reason=NULL WHERE job_id=? AND collection=?")
        .bind(job).bind(kind).execute(&mut *tx).await?;
    sqlx::query("UPDATE sync_job SET next_offset=0,processed_items=0,total_items=NULL,coverage='inconclusive',updated_at=? WHERE id=?")
        .bind(now()).bind(job).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
async fn publish(db: &mut SqliteConnection, job: &str, kind: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    let scope=sqlx::query("SELECT head_signature,observed_total,stop_reason,state FROM sync_scope WHERE job_id=? AND collection=?").bind(job).bind(kind).fetch_one(&mut *tx).await?;
    if scope.get::<String, _>("state") != "ready" {
        bail!("Coleção incompleta não pode publicar estados");
    }
    let table = if kind == "track" { "musica" } else { "album" };
    let published=sqlx::query(&format!("UPDATE {table} SET is_saved=1,saved_at=(SELECT saved_at FROM sync_seen_item WHERE job_id=? AND item_type=? AND item_id={table}.id),last_seen_saved_sync=?,updated_at=? WHERE id IN(SELECT item_id FROM sync_seen_item WHERE job_id=? AND item_type=?)"))
        .bind(job).bind(kind).bind(job).bind(now()).bind(job).bind(kind).execute(&mut *tx).await?.rows_affected();
    sqlx::query("UPDATE library_sync_state SET last_incremental_completed_at=?,last_coverage='recent_window',last_successful_job_id=?,head_signature=?,remote_total_last_seen=?,ordering_observed='descending_in_window',watermark_added_at=MAX(COALESCE(watermark_added_at,''),COALESCE((SELECT MAX(saved_at) FROM sync_seen_item WHERE job_id=? AND item_type=?),'')) WHERE collection=?")
        .bind(now()).bind(job).bind(scope.get::<Option<String>,_>("head_signature")).bind(scope.get::<Option<i64>,_>("observed_total")).bind(job).bind(kind).bind(kind).execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE sync_scope SET state='published',published_items=? WHERE job_id=? AND collection=?",
    )
    .bind(published as i64)
    .bind(job)
    .bind(kind)
    .execute(&mut *tx)
    .await?;
    let next = if kind == "track" {
        "albums"
    } else {
        "album_tracks"
    };
    sqlx::query("UPDATE sync_job SET stage=?,next_offset=0,total_items=NULL,processed_items=0,updated_at=? WHERE id=?").bind(next).bind(now()).bind(job).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn incremental_collection<A: Api>(
    db: &mut SqliteConnection,
    api: &mut A,
    job: &str,
    kind: &str,
    limit: u32,
    known_limit: i64,
) -> Result<()> {
    initialize_scope(db, job, kind).await?;
    let mut budget = Budget::new(db, job, kind, limit).await?;
    let endpoint = if kind == "track" {
        "/me/tracks"
    } else {
        "/me/albums"
    };
    let scope = sqlx::query("SELECT * FROM sync_scope WHERE job_id=? AND collection=?")
        .bind(job)
        .bind(kind)
        .fetch_one(&mut *db)
        .await?;
    if scope.get::<String, _>("state") == "published" {
        return Ok(());
    }
    let mut offset: i64 = scope.get("next_offset");
    let mut pages: i64 = scope.get("pages_processed");
    let mut known: i64 = scope.get("consecutive_known");
    let mut changes: i64 = scope.get("changed_items");
    let mut previous: Option<String> = scope.get("previous_added_at");
    let mut head: Option<String> = scope.get("head_signature");
    let mut ready = scope.get::<String, _>("state") == "ready";
    let mut head_validated = false;
    if pages > 0 {
        let page: Page =
            serde_json::from_value(budget.get(db, api, job, kind, endpoint, 0).await?)?;
        page.validate(0, None)?;
        if head.as_deref() != Some(legacy_sync::fingerprint(&page, kind)?.as_str()) {
            reset_window(db, job, kind).await?;
            return Err(window_changed());
        }
        head_validated = true;
        if !ready {
            let anchors=sqlx::query("SELECT page_offset,fingerprint FROM sync_page WHERE job_id=? AND item_type=? AND page_offset>0 ORDER BY page_offset DESC LIMIT 2")
                .bind(job).bind(kind).fetch_all(&mut *db).await?;
            for anchor in anchors.iter().rev() {
                let at: i64 = anchor.get("page_offset");
                let page: Page =
                    serde_json::from_value(budget.get(db, api, job, kind, endpoint, at).await?)?;
                page.validate(at, None)?;
                if legacy_sync::fingerprint(&page, kind)? != anchor.get::<String, _>("fingerprint")
                {
                    reset_window(db, job, kind).await?;
                    return Err(window_changed());
                }
            }
        }
    }
    let bar = indicatif::ProgressBar::new_spinner();
    bar.set_style(indicatif::ProgressStyle::with_template("{spinner} {msg}")?);
    while !ready {
        // Reserve a terminal head check if this call could complete the window.
        if budget.used + 1 >= limit && pages > 0 && (changes > 0 || pages > 2) {
            return Err(BudgetExhausted.into());
        }
        let page: Page =
            serde_json::from_value(budget.get(db, api, job, kind, endpoint, offset).await?)?;
        let end = page.validate(offset, None)?;
        let fp = legacy_sync::fingerprint(&page, kind)?;
        if offset == 0 {
            head = Some(fp.clone());
        }
        let mut tx = db.begin().await?;
        let mut wholly_known = true;
        let mut page_ids = HashSet::new();
        let mut page_changes = 0;
        let mut last = previous.clone();
        for item in &page.items {
            let object = &item[if kind == "track" { "track" } else { "album" }];
            let id = database::identity(object)?;
            let date = legacy_sync::saved_at(item)?;
            if last.as_ref().is_some_and(|old| date > *old) {
                return Err(WorkPaused{reason:"ordering_inconclusive",message:"Ordem da janela recente incompatível com adições no início. Estados preservados; use --reconcile para uma verificação completa explícita.".into()}.into());
            }
            last = Some(date.clone());
            let baseline:Option<String>=sqlx::query_scalar("SELECT saved_at FROM sync_baseline_item WHERE job_id=? AND collection=? AND item_id=?")
                .bind(job).bind(kind).bind(&id).fetch_optional(&mut *tx).await?.flatten();
            if baseline.as_deref() != Some(date.as_str()) {
                wholly_known = false;
                page_changes += 1;
            }
            if !page_ids.insert(id.clone()) {
                drop(tx);
                reset_window(db, job, kind).await?;
                return Err(window_changed());
            }
            let inserted=sqlx::query("INSERT OR IGNORE INTO sync_seen_item(job_id,item_type,item_id,saved_at) VALUES(?,?,?,?)")
                .bind(job).bind(kind).bind(&id).bind(&date).execute(&mut *tx).await?.rows_affected();
            if inserted != 1 {
                drop(tx);
                reset_window(db, job, kind).await?;
                return Err(window_changed());
            }
            if kind == "track" {
                let album = database::album(&mut tx, &object["album"]).await?;
                database::track(&mut tx, object, &album).await?;
            } else {
                legacy_sync::import_album(&mut tx, job, object, false, 30).await?;
            }
        }
        previous = last;
        changes += page_changes;
        known = if wholly_known && !page.items.is_empty() {
            known + 1
        } else {
            0
        };
        pages += 1;
        offset += page.items.len() as i64;
        ready = end || known >= known_limit;
        let reason = if end {
            Some("end_of_collection")
        } else if ready {
            Some("known_pages")
        } else {
            None
        };
        sqlx::query("INSERT OR REPLACE INTO sync_page VALUES(?,?,?,?,?)")
            .bind(job)
            .bind(kind)
            .bind(page.offset)
            .bind(fp)
            .bind(page.total)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE sync_scope SET state=?,next_offset=?,pages_processed=?,consecutive_known=?,changed_items=?,previous_added_at=?,head_signature=?,observed_total=?,stop_reason=?,needs_head_check=? WHERE job_id=? AND collection=?")
            .bind(if ready{"ready"}else{"collecting"}).bind(offset).bind(pages).bind(known).bind(changes).bind(&previous).bind(&head).bind(page.total).bind(reason)
            .bind(i64::from(pages>1 && (changes>0 || pages>2))).bind(job).bind(kind).execute(&mut *tx).await?;
        sqlx::query("UPDATE sync_job SET next_offset=?,processed_items=?,total_items=?,updated_at=? WHERE id=?").bind(offset).bind(offset).bind(page.total).bind(now()).bind(job).execute(&mut *tx).await?;
        tx.commit().await?;
        head_validated = false;
        bar.set_message(format!(
            "{kind}: janela recente, {pages} páginas, {changes} novidades, {}/{} requisições",
            budget.used, limit
        ));
    }
    if pages > 1 && (changes > 0 || pages > 2) && !head_validated {
        let page: Page =
            serde_json::from_value(budget.get(db, api, job, kind, endpoint, 0).await?)?;
        page.validate(0, None)?;
        if head.as_deref() != Some(legacy_sync::fingerprint(&page, kind)?.as_str()) {
            reset_window(db, job, kind).await?;
            return Err(window_changed());
        }
    }
    bar.finish_and_clear();
    publish(db, job, kind).await
}

async fn incremental_album_tracks<A: Api>(
    db: &mut SqliteConnection,
    api: &mut A,
    job: &str,
    limit: u32,
) -> Result<()> {
    let mut budget = Budget::new(db, job, "album_tracks", limit).await?;
    sqlx::query(
        "UPDATE sync_scope SET state='collecting' WHERE job_id=? AND collection='album_tracks'",
    )
    .bind(job)
    .execute(&mut *db)
    .await?;
    loop {
        let row=sqlx::query("SELECT album_id,next_offset,total_items FROM sync_album_queue WHERE job_id=? AND status<>'completed' ORDER BY album_id LIMIT 1").bind(job).fetch_optional(&mut *db).await?;
        let Some(row) = row else {
            sqlx::query("UPDATE sync_scope SET state='completed',stop_reason='queue_empty' WHERE job_id=? AND collection='album_tracks'").bind(job).execute(&mut *db).await?;
            return Ok(());
        };
        let id: String = row.get("album_id");
        let offset: i64 = row.get("next_offset");
        let total: Option<i64> = row.get("total_items");
        let result = budget
            .get(
                db,
                api,
                job,
                "album_tracks",
                &format!("/albums/{id}/tracks"),
                offset,
            )
            .await;
        let page: Page = match result {
            Ok(v) => serde_json::from_value(v)?,
            Err(e) => {
                sqlx::query("UPDATE sync_album_queue SET last_error=?,attempts=attempts+1,updated_at=? WHERE job_id=? AND album_id=?").bind(e.to_string()).bind(now()).bind(job).bind(&id).execute(&mut *db).await?;
                return Err(e);
            }
        };
        let done = match page.validate(offset, total) {
            Ok(done) => done,
            Err(e) => {
                sqlx::query("UPDATE sync_album_queue SET next_offset=0,total_items=NULL,last_error=?,updated_at=? WHERE job_id=? AND album_id=?").bind(e.to_string()).bind(now()).bind(job).bind(&id).execute(&mut *db).await?;
                return Err(e);
            }
        };
        let mut tx = db.begin().await?;
        for track in &page.items {
            database::track(&mut tx, track, &id).await?;
        }
        sqlx::query("UPDATE sync_album_queue SET next_offset=?,total_items=?,status=?,attempts=attempts+1,last_error=NULL,updated_at=? WHERE job_id=? AND album_id=?")
            .bind(offset+page.items.len() as i64).bind(page.total).bind(if done{"completed"}else{"running"}).bind(now()).bind(job).bind(&id).execute(&mut *tx).await?;
        if done {
            sqlx::query("UPDATE album SET tracks_fetched_at=?,tracks_imported_total=? WHERE id=?")
                .bind(now())
                .bind(page.total)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        eprintln!(
            "Faixas de {id}: {}/{}, requisições {}/{}",
            offset + page.items.len() as i64,
            page.total,
            budget.used,
            limit
        );
    }
}

pub async fn run<A: Api>(
    db: &mut SqliteConnection,
    api: &mut A,
    job: &str,
    ttl: u32,
) -> Result<Value> {
    let mode: String = sqlx::query_scalar("SELECT sync_mode FROM sync_job WHERE id=?")
        .bind(job)
        .fetch_one(&mut *db)
        .await?;
    if mode != "incremental" {
        return legacy_sync::run(db, api, job, ttl).await;
    }
    loop {
        let row = sqlx::query(
            "SELECT stage,collection_budget,album_budget,known_pages FROM sync_job WHERE id=?",
        )
        .bind(job)
        .fetch_one(&mut *db)
        .await?;
        match row.get::<String, _>("stage").as_str() {
            "tracks" => {
                incremental_collection(
                    db,
                    api,
                    job,
                    "track",
                    row.get::<i64, _>("collection_budget") as u32,
                    row.get("known_pages"),
                )
                .await?
            }
            "albums" => {
                incremental_collection(
                    db,
                    api,
                    job,
                    "album",
                    row.get::<i64, _>("collection_budget") as u32,
                    row.get("known_pages"),
                )
                .await?
            }
            "album_tracks" => {
                incremental_album_tracks(db, api, job, row.get::<i64, _>("album_budget") as u32)
                    .await?;
                let scopes=sqlx::query("SELECT collection,changed_items,stop_reason,requests_total FROM sync_scope WHERE job_id=? ORDER BY collection").bind(job).fetch_all(&mut *db).await?;
                let items:Vec<Value>=scopes.iter().map(|r|json!({"collection":r.get::<String,_>("collection"),"changed_items":r.get::<i64,_>("changed_items"),"stop_reason":r.get::<Option<String>,_>("stop_reason"),"requests":r.get::<i64,_>("requests_total")})).collect();
                let summary = json!({"mode":"incremental","coverage":"recent_window","removals_checked":false,"scopes":items});
                let mut tx = db.begin().await?;
                sqlx::query("UPDATE sync_job SET stage='done',status='completed',coverage='recent_window',completed_at=?,updated_at=?,summary_json=?,last_error=NULL,pause_reason=NULL WHERE id=?").bind(now()).bind(now()).bind(summary.to_string()).bind(job).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM sync_baseline_item WHERE job_id=?")
                    .bind(job)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(summary);
            }
            "done" => return Ok(json!({"status":"completed","coverage":"recent_window"})),
            other => bail!("Etapa incremental desconhecida: {other}"),
        }
    }
}

pub async fn check_saved<A: Api>(
    db: &mut SqliteConnection,
    api: &mut A,
    uris: &[String],
) -> Result<Value> {
    check_cooldown(db).await?;
    let mut unique = HashSet::new();
    let mut ordered = Vec::new();
    for uri in uris {
        let parts: Vec<_> = uri.split(':').collect();
        if parts.len() != 3
            || parts[0] != "spotify"
            || !["track", "album"].contains(&parts[1])
            || parts[2].is_empty()
            || !parts[2].bytes().all(|c| c.is_ascii_alphanumeric())
        {
            bail!("URI inválida; use spotify:track:ID ou spotify:album:ID");
        }
        if unique.insert(uri.clone()) {
            ordered.push(uri.clone());
        }
    }
    let mut output = Vec::new();
    for batch in ordered.chunks(40) {
        let value = match api.contains(batch).await {
            Ok(value) => value,
            Err(error) => {
                remember_cooldown(db, &error).await?;
                return Err(error);
            }
        };
        let values = value
            .as_array()
            .context("Resposta contains inválida; nenhum estado deste lote foi alterado")?;
        if values.len() != batch.len() || values.iter().any(|v| !v.is_boolean()) {
            bail!("Resposta contains incompleta; nenhum estado deste lote foi alterado");
        }
        let mut tx = db.begin().await?;
        for (uri, saved) in batch.iter().zip(values) {
            let parts: Vec<_> = uri.split(':').collect();
            let table = if parts[1] == "track" {
                "musica"
            } else {
                "album"
            };
            let updated = sqlx::query(&format!(
                "UPDATE {table} SET is_saved=?,updated_at=? WHERE id=?"
            ))
            .bind(saved.as_bool().unwrap())
            .bind(now())
            .bind(parts[2])
            .execute(&mut *tx)
            .await?
            .rows_affected();
            output.push(json!({"uri":uri,"is_saved":saved,"local_updated":updated>0,"saved_at_verified":false}));
        }
        tx.commit().await?;
    }
    Ok(json!({"coverage":"selected_items","items":output}))
}

pub async fn status(db: &mut SqliteConnection) -> Result<Value> {
    let row:Option<String>=sqlx::query_scalar("SELECT json_object('id',id,'type',job_type,'mode',sync_mode,'status',status,'stage',stage,'coverage',coverage,'pause_reason',pause_reason,'next_offset',next_offset,'total_items',total_items,'requests',request_count,'retries',retry_count,'last_error',last_error,'retry_at',retry_at,'updated_at',updated_at,'summary',json(summary_json)) FROM sync_job ORDER BY started_at DESC LIMIT 1").fetch_optional(&mut *db).await?;
    let states=sqlx::query("SELECT collection,last_incremental_completed_at,last_full_reconciliation_at,last_coverage FROM library_sync_state ORDER BY collection").fetch_all(&mut *db).await?;
    let scope=sqlx::query("SELECT collection,state,next_offset,pages_processed,consecutive_known,stop_reason,requests_total,invocation_requests,request_limit FROM sync_scope WHERE job_id=(SELECT id FROM sync_job ORDER BY started_at DESC LIMIT 1) ORDER BY collection").fetch_all(&mut *db).await?;
    let queue:Option<String>=sqlx::query_scalar("SELECT json_object('album_id',album_id,'offset',next_offset,'status',status,'last_error',last_error) FROM sync_album_queue WHERE job_id=(SELECT id FROM sync_job ORDER BY started_at DESC LIMIT 1) AND status<>'completed' ORDER BY album_id LIMIT 1").fetch_optional(&mut *db).await?;
    Ok(
        json!({"job":row.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,"current_album":queue.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,
        "collections":states.iter().map(|r|json!({"collection":r.get::<String,_>("collection"),"last_incremental":r.get::<Option<String>,_>("last_incremental_completed_at"),"last_reconciliation":r.get::<Option<String>,_>("last_full_reconciliation_at"),"coverage":r.get::<String,_>("last_coverage")})).collect::<Vec<_>>(),
        "scopes":scope.iter().map(|r|json!({"collection":r.get::<String,_>("collection"),"state":r.get::<String,_>("state"),"offset":r.get::<i64,_>("next_offset"),"pages":r.get::<i64,_>("pages_processed"),"known_pages":r.get::<i64,_>("consecutive_known"),"stop_reason":r.get::<Option<String>,_>("stop_reason"),"requests":r.get::<i64,_>("requests_total"),"invocation_requests":r.get::<i64,_>("invocation_requests"),"request_limit":r.get::<Option<i64>,_>("request_limit")})).collect::<Vec<_>>() }),
    )
}
pub async fn warn_coverage(db: &mut SqliteConnection) -> Result<()> {
    let status:i64=sqlx::query_scalar("SELECT COUNT(*) FROM library_sync_state WHERE last_coverage<>'full_listing' OR last_full_reconciliation_at IS NULL OR julianday('now')-julianday(last_full_reconciliation_at)>30").fetch_one(db).await?;
    if status > 0 {
        eprintln!("Cobertura recente ou reconciliação antiga: is_saved representa a última confirmação conhecida; remoções antigas podem estar pendentes. Use sync --reconcile para verificar toda a biblioteca.");
    }
    Ok(())
}
