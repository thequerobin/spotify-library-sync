//! Checkpoint-preserving full imports, with a separately resumable confirmation pass.
use crate::{
    database::{self, now},
    legacy_sync,
    spotify::{Api, BudgetExhausted, Page},
    sync::WorkPaused,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Serialize, Deserialize)]
struct Anchor {
    id: String,
    saved_at: String,
}
fn anchors(page: &Page, kind: &str) -> Result<Vec<Anchor>> {
    let key = if kind == "track" { "track" } else { "album" };
    page.items
        .iter()
        .rev()
        .take(5)
        .rev()
        .map(|v| {
            Ok(Anchor {
                id: database::identity(&v[key])?,
                saved_at: legacy_sync::saved_at(v)?,
            })
        })
        .collect()
}
fn pause(reason: &'static str, message: &str) -> anyhow::Error {
    WorkPaused {
        reason,
        message: message.into(),
    }
    .into()
}

async fn initialize(db: &mut SqliteConnection, job: &str, kind: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO full_scan_checkpoint(job_id,collection,phase,scan_offset,scan_total,updated_at) SELECT id,?,CASE WHEN total_items IS NOT NULL AND next_offset>=total_items THEN 'verifying' ELSE 'collecting' END,next_offset,total_items,? FROM sync_job WHERE id=?")
        .bind(kind).bind(now()).bind(job).execute(db).await?;
    Ok(())
}
async fn import_item(
    db: &mut SqliteConnection,
    job: &str,
    kind: &str,
    item: &Value,
    full: bool,
    ttl: u32,
) -> Result<String> {
    let object = &item[if kind == "track" { "track" } else { "album" }];
    if kind == "track" {
        let id = database::album(db, &object["album"]).await?;
        database::track(db, object, &id).await
    } else {
        legacy_sync::import_album(db, job, object, full, ttl).await
    }
}
fn validate_unique(page: &Page, kind: &str) -> Result<()> {
    let mut ids = HashSet::new();
    for a in anchors_all(page, kind)? {
        if !ids.insert(a.id) {
            return Err(pause("pagination_changed","A página contém IDs repetidos. O último lote confirmado foi preservado; use sync --resume para localizar a referência novamente."));
        }
    }
    Ok(())
}
fn anchors_all(page: &Page, kind: &str) -> Result<Vec<Anchor>> {
    let key = if kind == "track" { "track" } else { "album" };
    page.items
        .iter()
        .map(|v| {
            Ok(Anchor {
                id: database::identity(&v[key])?,
                saved_at: legacy_sync::saved_at(v)?,
            })
        })
        .collect()
}

struct Search<'a, A: Api> {
    api: &'a mut A,
    remaining: u32,
    pages: HashMap<i64, Page>,
}
impl<A: Api> Search<'_, A> {
    async fn fetch(&mut self, path: &str, offset: i64) -> Result<Page> {
        if let Some(page) = self.pages.get(&offset) {
            return Ok(page.clone());
        }
        if self.remaining == 0 {
            return Err(BudgetExhausted.into());
        }
        self.api.set_budget(Some(self.remaining));
        let before = self.api.attempt_count();
        let value = self.api.get(path, Some(offset)).await;
        self.api.set_budget(None);
        self.remaining = self
            .remaining
            .saturating_sub(self.api.attempt_count().saturating_sub(before).max(1) as u32);
        let page: Page = serde_json::from_value(value?)?;
        // Spotify can return an empty page for an offset now beyond the end.
        if !(page.items.is_empty()
            && offset > page.total
            && page.offset == offset
            && page.total >= 0)
        {
            page.validate(offset, None)?;
        }
        self.pages.insert(offset, page.clone());
        Ok(page)
    }
}

/// Request the pending offset first, then search backwards (and near the total-adjusted position).
/// The identity/date anchor is the last imported item, never MAX(added_at).
async fn locate<A: Api>(
    db: &mut SqliteConnection,
    api: &mut A,
    job: &str,
    kind: &str,
    path: &str,
    verify: bool,
) -> Result<(i64, i64, HashMap<i64, Page>)> {
    let row = sqlx::query("SELECT * FROM full_scan_checkpoint WHERE job_id=? AND collection=?")
        .bind(job)
        .bind(kind)
        .fetch_one(&mut *db)
        .await?;
    let offset: i64 = row.get(if verify {
        "verify_offset"
    } else {
        "scan_offset"
    });
    let old_total: Option<i64> = row.get(if verify { "verify_total" } else { "scan_total" });
    let stored: Option<String> = row.get(if verify {
        "verify_anchor_json"
    } else {
        "scan_anchor_json"
    });
    let marker: Vec<Anchor> = stored
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default();
    let limit: i64 = sqlx::query_scalar("SELECT resume_search_pages FROM sync_job WHERE id=?")
        .bind(job)
        .fetch_one(&mut *db)
        .await?;
    let mut search = Search {
        api,
        remaining: limit as u32,
        pages: HashMap::new(),
    };
    let first = search.fetch(path, offset).await?;
    let total = first.total;
    if total == 0 {
        return Ok((0, 0, search.pages));
    }
    let old_page: Option<String> = if !verify && marker.is_empty() {
        sqlx::query_scalar("SELECT fingerprint FROM sync_page WHERE job_id=? AND item_type=? ORDER BY page_offset DESC LIMIT 1").bind(job).bind(kind).fetch_optional(&mut *db).await?
    } else {
        None
    };
    let adjusted = (offset + total - old_total.unwrap_or(total))
        .max(0)
        .min(total.saturating_sub(1));
    let mut positions = vec![
        offset,
        (offset - 50).max(0),
        adjusted,
        ((adjusted / 50) * 50 - 50).max(0),
    ];
    for n in 2..=limit {
        positions.push((offset - n * 50).max(0));
    }
    let mut visited = HashSet::new();
    for position in positions {
        if !visited.insert(position) {
            continue;
        }
        if search.remaining == 0 && !search.pages.contains_key(&position) {
            break;
        }
        let page = search.fetch(path, position).await?;
        if page.offset >= page.total && page.items.is_empty() {
            continue;
        }
        validate_unique(&page, kind)?;
        let items = anchors_all(&page, kind)?;
        // Prefer the most recent surviving imported anchor. A removed last item can fall back to its predecessor.
        if !marker.is_empty() {
            for anchor in marker.iter().rev() {
                if let Some(index) = items
                    .iter()
                    .position(|a| a.id == anchor.id && a.saved_at == anchor.saved_at)
                {
                    return Ok((page.offset + index as i64 + 1, total, search.pages));
                }
            }
        }
        // Legacy jobs have no ordered marker; deleted boundary IDs can also fall back to a confirmed overlap.
        if !verify && old_page.as_deref() == Some(legacy_sync::fingerprint(&page, kind)?.as_str()) {
            return Ok((page.offset + page.items.len() as i64, total, search.pages));
        }
        for (index, item) in items.iter().enumerate().rev() {
            let sql = if verify {
                "SELECT COUNT(*) FROM full_scan_verified_item WHERE job_id=? AND collection=? AND item_id=? AND saved_at=?"
            } else {
                "SELECT COUNT(*) FROM sync_seen_item WHERE job_id=? AND item_type=? AND item_id=? AND saved_at=?"
            };
            let known: i64 = sqlx::query_scalar(sql)
                .bind(job)
                .bind(kind)
                .bind(&item.id)
                .bind(&item.saved_at)
                .fetch_one(&mut *db)
                .await?;
            if known == 1 {
                return Ok((page.offset + index as i64 + 1, total, search.pages));
            }
        }
    }
    Err(pause("resume_anchor_missing","A referência próxima ao checkpoint não foi encontrada dentro do limite. O progresso foi preservado; tente sync --resume --resume-search-pages 30. Nenhuma coleção foi reiniciada automaticamente."))
}
async fn reset_confirmation(db: &mut SqliteConnection, job: &str, kind: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM full_scan_verified_item WHERE job_id=? AND collection=?")
        .bind(job)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE full_scan_checkpoint SET phase='verifying',verify_offset=0,verify_total=NULL,verify_anchor_json=NULL,verify_head=NULL,updated_at=? WHERE job_id=? AND collection=?")
        .bind(now()).bind(job).bind(kind).execute(&mut *tx).await?;
    // The first-pass offset remains in full_scan_checkpoint.scan_offset.
    sqlx::query("UPDATE sync_job SET next_offset=0,processed_items=0,updated_at=? WHERE id=?")
        .bind(now())
        .bind(job)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn collect<A: Api>(
    db: &mut SqliteConnection,
    api: &mut A,
    job: &str,
    kind: &str,
    full: bool,
    ttl: u32,
) -> Result<()> {
    initialize(db, job, kind).await?;
    let row=sqlx::query("SELECT phase,scan_offset,scan_total FROM full_scan_checkpoint WHERE job_id=? AND collection=?").bind(job).bind(kind).fetch_one(&mut *db).await?;
    let phase: String = row.get("phase");
    if phase == "verified" {
        return Ok(());
    }
    let path = if kind == "track" {
        "/me/tracks"
    } else {
        "/me/albums"
    };
    if phase == "collecting" {
        let mut offset: i64 = row.get("scan_offset");
        let mut total: Option<i64> = row.get("scan_total");
        let mut cache = HashMap::new();
        if offset > 0 {
            let (position, current, pages) = locate(db, api, job, kind, path, false).await?;
            eprintln!("Retomada {kind}: checkpoint {offset}, referência localizada em {position}; catálogo preservado.");
            offset = position;
            total = Some(current);
            cache = pages;
        }
        let bar = indicatif::ProgressBar::new(total.unwrap_or(0) as u64);
        bar.set_style(indicatif::ProgressStyle::with_template(
            "{msg} [{bar:30}] {pos}/{len}",
        )?);
        bar.set_message(if kind == "track" {
            "Músicas curtidas — importação"
        } else {
            "Álbuns salvos — importação"
        });
        bar.set_position(offset as u64);
        while total.is_none() || offset < total.unwrap() {
            let page: Page = match cache.remove(&offset) {
                Some(p) => p,
                None => serde_json::from_value(api.get(path, Some(offset)).await?)?,
            };
            page.validate(offset, None)?;
            validate_unique(&page, kind)?;
            let marker = serde_json::to_string(&anchors(&page, kind)?)?;
            let fp = legacy_sync::fingerprint(&page, kind)?;
            let mut tx = db.begin().await?;
            for item in &page.items {
                let id = import_item(&mut tx, job, kind, item, full, ttl).await?;
                sqlx::query("INSERT INTO sync_seen_item VALUES(?,?,?,?) ON CONFLICT(job_id,item_type,item_id) DO UPDATE SET saved_at=excluded.saved_at")
                    .bind(job).bind(kind).bind(id).bind(legacy_sync::saved_at(item)?).execute(&mut *tx).await?;
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
            sqlx::query("UPDATE full_scan_checkpoint SET scan_offset=?,scan_total=?,scan_anchor_json=?,updated_at=? WHERE job_id=? AND collection=?")
                .bind(next).bind(page.total).bind(marker).bind(now()).bind(job).bind(kind).execute(&mut *tx).await?;
            sqlx::query("UPDATE sync_job SET next_offset=?,processed_items=?,total_items=?,updated_at=? WHERE id=?").bind(next).bind(next).bind(page.total).bind(now()).bind(job).execute(&mut *tx).await?;
            tx.commit().await?;
            offset = next;
            total = Some(page.total);
            bar.set_length(page.total as u64);
            bar.set_position(next as u64);
        }
        bar.finish_and_clear();
        sqlx::query("UPDATE full_scan_checkpoint SET phase='verifying',scan_offset=?,scan_total=?,updated_at=? WHERE job_id=? AND collection=?").bind(offset).bind(total).bind(now()).bind(job).bind(kind).execute(&mut *db).await?;
    }
    confirm(db, api, job, kind, path, full, ttl).await
}

async fn confirm<A: Api>(
    db: &mut SqliteConnection,
    api: &mut A,
    job: &str,
    kind: &str,
    path: &str,
    full: bool,
    ttl: u32,
) -> Result<()> {
    let row=sqlx::query("SELECT verify_offset,verify_total,verify_head FROM full_scan_checkpoint WHERE job_id=? AND collection=?").bind(job).bind(kind).fetch_one(&mut *db).await?;
    let mut offset: i64 = row.get("verify_offset");
    let mut total: Option<i64> = row.get("verify_total");
    let mut cache = HashMap::new();
    if offset > 0 {
        let head: Page = serde_json::from_value(api.get(path, Some(0)).await?)?;
        head.validate(0, None)?;
        if row.get::<Option<String>, _>("verify_head").as_deref()
            != Some(legacy_sync::fingerprint(&head, kind)?.as_str())
            || total != Some(head.total)
        {
            reset_confirmation(db, job, kind).await?;
            return Err(pause("confirmation_changed","O cabeçalho mudou durante a conferência. A primeira passagem foi preservada; sync --resume repetirá somente a conferência."));
        }
        let (position, current, pages) = match locate(db, api, job, kind, path, true).await {
            Ok(found) => found,
            Err(error)
                if error
                    .downcast_ref::<WorkPaused>()
                    .is_some_and(|p| p.reason == "resume_anchor_missing") =>
            {
                reset_confirmation(db, job, kind).await?;
                return Err(pause("confirmation_changed","A referência da conferência desapareceu. A primeira passagem foi preservada; use sync --resume para repetir somente a conferência."));
            }
            Err(error) => return Err(error),
        };
        if position != offset || Some(current) != total {
            reset_confirmation(db, job, kind).await?;
            return Err(pause("confirmation_changed","A posição da conferência mudou. A importação continua concluída; retome para refazer somente a conferência."));
        }
        cache = pages;
    }
    let bar = indicatif::ProgressBar::new(total.unwrap_or(0) as u64);
    bar.set_style(indicatif::ProgressStyle::with_template(
        "{msg} [{bar:30}] {pos}/{len}",
    )?);
    bar.set_message(format!("Conferindo {kind} — importação preservada"));
    bar.set_position(offset as u64);
    loop {
        if total.is_some_and(|n| offset >= n) {
            break;
        }
        let page: Page = match cache.remove(&offset) {
            Some(p) => p,
            None => serde_json::from_value(api.get(path, Some(offset)).await?)?,
        };
        if page.validate(offset, total).is_err() {
            reset_confirmation(db, job, kind).await?;
            return Err(pause("confirmation_changed","A coleção mudou durante a conferência. A primeira passagem foi preservada; use sync --resume."));
        }
        validate_unique(&page, kind)?;
        let mut tx = db.begin().await?;
        for item in &page.items {
            let id = database::identity(&item[if kind == "track" { "track" } else { "album" }])?;
            let date = legacy_sync::saved_at(item)?;
            let inserted =
                sqlx::query("INSERT OR IGNORE INTO full_scan_verified_item VALUES(?,?,?,?)")
                    .bind(job)
                    .bind(kind)
                    .bind(&id)
                    .bind(&date)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            if inserted != 1 {
                drop(tx);
                reset_confirmation(db, job, kind).await?;
                return Err(pause("confirmation_changed","IDs repetidos entre páginas da conferência. A importação foi preservada; use sync --resume."));
            }
            let same:i64=sqlx::query_scalar("SELECT COUNT(*) FROM sync_seen_item WHERE job_id=? AND item_type=? AND item_id=? AND saved_at=?").bind(job).bind(kind).bind(&id).bind(&date).fetch_one(&mut *tx).await?;
            if same == 0 {
                import_item(&mut tx, job, kind, item, full, ttl).await?;
            }
        }
        let next = offset + page.items.len() as i64;
        sqlx::query("UPDATE full_scan_checkpoint SET verify_offset=?,verify_total=?,verify_anchor_json=?,verify_head=CASE WHEN ?=0 THEN ? ELSE verify_head END,updated_at=? WHERE job_id=? AND collection=?")
            .bind(next).bind(page.total).bind(serde_json::to_string(&anchors(&page,kind)?)?).bind(offset).bind(legacy_sync::fingerprint(&page,kind)?).bind(now()).bind(job).bind(kind).execute(&mut *tx).await?;
        sqlx::query("UPDATE sync_job SET next_offset=?,processed_items=?,total_items=?,updated_at=? WHERE id=?").bind(next).bind(next).bind(page.total).bind(now()).bind(job).execute(&mut *tx).await?;
        tx.commit().await?;
        offset = next;
        total = Some(page.total);
        bar.set_length(page.total as u64);
        bar.set_position(next as u64);
        if page.next.is_none() {
            break;
        }
    }
    bar.finish_and_clear();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM full_scan_verified_item WHERE job_id=? AND collection=?",
    )
    .bind(job)
    .bind(kind)
    .fetch_one(&mut *db)
    .await?;
    if Some(count) != total {
        reset_confirmation(db, job, kind).await?;
        return Err(pause(
            "confirmation_changed",
            "Contagem inconsistente na conferência. A primeira passagem foi preservada.",
        ));
    }
    let difference:i64=sqlx::query_scalar("SELECT EXISTS(SELECT item_id,saved_at FROM sync_seen_item WHERE job_id=? AND item_type=? EXCEPT SELECT item_id,saved_at FROM full_scan_verified_item WHERE job_id=? AND collection=?) OR EXISTS(SELECT item_id,saved_at FROM full_scan_verified_item WHERE job_id=? AND collection=? EXCEPT SELECT item_id,saved_at FROM sync_seen_item WHERE job_id=? AND item_type=?)")
        .bind(job).bind(kind).bind(job).bind(kind).bind(job).bind(kind).bind(job).bind(kind).fetch_one(&mut *db).await?;
    if difference != 0 {
        let mut tx = db.begin().await?;
        sqlx::query("DELETE FROM sync_seen_item WHERE job_id=? AND item_type=?")
            .bind(job)
            .bind(kind)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO sync_seen_item SELECT job_id,collection,item_id,saved_at FROM full_scan_verified_item WHERE job_id=? AND collection=?").bind(job).bind(kind).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM full_scan_verified_item WHERE job_id=? AND collection=?")
            .bind(job)
            .bind(kind)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE full_scan_checkpoint SET verify_offset=0,verify_total=NULL,verify_anchor_json=NULL,verify_head=NULL,updated_at=? WHERE job_id=? AND collection=?").bind(now()).bind(job).bind(kind).execute(&mut *tx).await?;
        sqlx::query("UPDATE sync_job SET next_offset=0,processed_items=0,updated_at=? WHERE id=?")
            .bind(now())
            .bind(job)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(pause("confirmation_changed","A biblioteca mudou entre importação e conferência. O catálogo e a primeira passagem foram preservados; sync --resume repetirá somente a conferência antes de confirmar remoções."));
    }
    sqlx::query("UPDATE full_scan_checkpoint SET phase='verified',updated_at=? WHERE job_id=? AND collection=?").bind(now()).bind(job).bind(kind).execute(db).await?;
    Ok(())
}
