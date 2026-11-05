//! Followed-artist discovery, independent from library membership synchronization.
use crate::{
    database::{self, now},
    spotify::{Api, BudgetExhausted, Page, Paused},
    sync::{self, WorkPaused},
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize, Deserialize, clap::Args)]
pub struct Options {
    /// Limite global de tentativas HTTP nesta invocação; padrão 200 em novos jobs.
    #[arg(long,value_parser=clap::value_parser!(u32).range(5..=10000))]
    pub request_budget: Option<u32>,
    /// Máximo de artistas selecionados em uma execução nova.
    #[arg(long,default_value_t=100,value_parser=clap::value_parser!(u32).range(1..=10000))]
    pub max_artists: u32,
    #[arg(long,default_value_t=7,value_parser=clap::value_parser!(u32).range(0..=36500))]
    pub ttl_days: u32,
    /// Revalidar toda a listagem dos artistas selecionados, ignorando o TTL.
    #[arg(long)]
    pub full: bool,
    /// Descobrir lançamentos e deixar suas faixas na fila para discography tracks.
    #[arg(long)]
    pub catalog_only: bool,
    #[arg(long)]
    pub market: Option<String>,
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "album,single,compilation,appears_on"
    )]
    pub groups: Vec<String>,
    /// IDs de artistas seguidos para priorizar; ignora seu TTL.
    #[arg(long = "artist", value_delimiter = ',')]
    pub artists: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            request_budget: None,
            max_artists: 100,
            ttl_days: 7,
            full: false,
            catalog_only: false,
            market: None,
            groups: vec![
                "album".into(),
                "single".into(),
                "compilation".into(),
                "appears_on".into(),
            ],
            artists: vec![],
        }
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric())
}
fn paused(reason: &'static str, message: &str) -> anyhow::Error {
    WorkPaused {
        reason,
        message: message.into(),
    }
    .into()
}
impl Options {
    fn normalize(&mut self) -> Result<()> {
        if self
            .request_budget
            .is_some_and(|v| !(5..=10000).contains(&v))
            || !(1..=10000).contains(&self.max_artists)
            || self.ttl_days > 36500
        {
            bail!("Orçamento deve ser 5..10000, artistas 1..10000 e TTL até 36500 dias");
        }
        self.groups.sort();
        self.groups.dedup();
        self.artists.sort();
        self.artists.dedup();
        if self.groups.is_empty()
            || self
                .groups
                .iter()
                .any(|g| !["album", "single", "compilation", "appears_on"].contains(&g.as_str()))
        {
            bail!("Grupos válidos: album,single,compilation,appears_on");
        }
        if self.artists.iter().any(|a| !valid_id(a)) {
            bail!("Informe IDs Spotify válidos para os artistas");
        }
        if let Some(market) = &mut self.market {
            *market = market.to_ascii_uppercase();
            if market.len() != 2 || !market.bytes().all(|c| c.is_ascii_uppercase()) {
                bail!("Mercado deve ter duas letras, por exemplo BR");
            }
        }
        Ok(())
    }
    fn contexts(&self, account: &str) -> (String, String) {
        let album = json!({"account":account,"requested_market":self.market}).to_string();
        let catalog = json!({"album_context":album,"groups":self.groups}).to_string();
        (catalog, album)
    }
}
pub async fn prepare(
    db: &mut SqliteConnection,
    resume: bool,
    kind: &str,
    mut options: Options,
) -> Result<String> {
    sync::check_cooldown(db).await?;
    options.normalize()?;
    if !["sync", "tracks"].contains(&kind) {
        bail!("Tipo de execução inválido");
    }
    if let Some(job) =
        sqlx::query_scalar::<_, String>("SELECT id FROM discografia_job WHERE status<>'completed'")
            .fetch_optional(&mut *db)
            .await?
    {
        if !resume {
            bail!(
                "Há uma discografia pendente. Use discography sync --resume ou discography restart"
            );
        }
        sqlx::query("UPDATE discografia_job SET status='running',request_limit=COALESCE(?,request_limit),invocation_requests=0,last_error=NULL,pause_reason=NULL,updated_at=? WHERE id=?")
            .bind(options.request_budget).bind(now()).bind(&job).execute(db).await?;
        return Ok(job);
    }
    if resume {
        bail!("Nenhuma discografia pendente");
    }
    let id = format!(
        "disc-{}-{:016x}",
        Utc::now().timestamp_millis(),
        rand::random::<u64>()
    );
    sqlx::query("INSERT INTO discografia_job(id,kind,phase,options_json,request_limit,started_at,updated_at) VALUES(?,?,?,?,?,?,?)")
        .bind(&id).bind(kind).bind(if kind=="sync"{"following"}else{"tracks"}).bind(serde_json::to_string(&options)?).bind(options.request_budget.unwrap_or(200)).bind(now()).bind(now()).execute(db).await?;
    Ok(id)
}
pub async fn restart(db: &mut SqliteConnection) -> Result<()> {
    sync::check_cooldown(db).await?;
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE discografia_cache SET needs_full_refresh=1 WHERE EXISTS(SELECT 1 FROM discografia_artista_queue q JOIN discografia_job j ON j.id=q.job_id WHERE q.artista_id=discografia_cache.artista_id AND j.catalog_context=discografia_cache.catalog_context AND j.status<>'completed' AND q.state<>'completed')").execute(&mut *tx).await?;
    sqlx::query("UPDATE discografia_job SET status='completed',phase='abandoned',pause_reason='restarted',updated_at=? WHERE status<>'completed'").bind(now()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn record_error(
    db: &mut SqliteConnection,
    job: &str,
    error: &anyhow::Error,
) -> Result<()> {
    sync::remember_cooldown(db, error).await?;
    let work = error.downcast_ref::<WorkPaused>();
    sqlx::query(
        "UPDATE discografia_job SET status=?,pause_reason=?,last_error=?,updated_at=? WHERE id=?",
    )
    .bind(if error.is::<Paused>() {
        "rate_limited"
    } else if work.is_some() {
        "paused"
    } else {
        "failed"
    })
    .bind(work.map(|e| e.reason))
    .bind(error.to_string())
    .bind(now())
    .bind(job)
    .execute(db)
    .await?;
    Ok(())
}

enum Call<'a> {
    Profile,
    Following(Option<&'a str>),
    Releases(&'a str, i64),
    Tracks(&'a str, i64),
}
struct Runtime<'a, A: Api> {
    api: &'a mut A,
    job: String,
    opts: Options,
    limit: u32,
    used: u32,
    revalidated: HashSet<String>,
}
impl<A: Api> Runtime<'_, A> {
    async fn fetch(&mut self, db: &mut SqliteConnection, call: Call<'_>) -> Result<Value> {
        if self.used >= self.limit {
            return Err(paused("request_budget_exhausted","Orçamento da discografia esgotado. Continue com spotify-sync discography sync --resume; o progresso foi preservado."));
        }
        self.api.set_budget(Some(self.limit - self.used));
        let before = self.api.attempt_count();
        let result=match call{
            Call::Profile=>self.api.get("/me",None).await,
            Call::Following(after)=>self.api.followed_artists(after).await.context("Não foi possível ler os artistas seguidos. Se faltar permissão, execute login --discography"),
            Call::Releases(id,offset)=>self.api.artist_albums(id,offset,&self.opts.groups.join(","),self.opts.market.as_deref()).await,
            Call::Tracks(id,offset)=>self.api.album_tracks(id,offset,self.opts.market.as_deref()).await,
        };
        self.api.set_budget(None);
        let attempts = self.api.attempt_count().saturating_sub(before).max(1) as u32;
        self.used += attempts;
        sqlx::query("UPDATE discografia_job SET invocation_requests=?,request_count=request_count+?,retry_count=retry_count+?,updated_at=? WHERE id=?")
            .bind(self.used).bind(attempts).bind(attempts.saturating_sub(1)).bind(now()).bind(&self.job).execute(db).await?;
        match result {
            Err(e) if e.is::<BudgetExhausted>() => Err(paused(
                "request_budget_exhausted",
                "Orçamento consumido pelas tentativas HTTP. Retome com discography sync --resume.",
            )),
            other => other,
        }
    }
}

async fn reset_following(db: &mut SqliteConnection, job: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM discografia_followed_seen WHERE job_id=?")
        .bind(job)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM discografia_followed_cursor WHERE job_id=?")
        .bind(job)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE discografia_job SET following_cursor=NULL,following_total=NULL,updated_at=? WHERE id=?").bind(now()).bind(job).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
async fn following<A: Api>(db: &mut SqliteConnection, rt: &mut Runtime<'_, A>) -> Result<()> {
    loop {
        let row =
            sqlx::query("SELECT following_cursor,following_total FROM discografia_job WHERE id=?")
                .bind(&rt.job)
                .fetch_one(&mut *db)
                .await?;
        let cursor: Option<String> = row.get("following_cursor");
        let expected: Option<i64> = row.get("following_total");
        let value = rt.fetch(db, Call::Following(cursor.as_deref())).await?;
        let body = &value["artists"];
        let items = body["items"]
            .as_array()
            .context("Resposta de artistas seguidos sem items")?;
        let total = body["total"]
            .as_i64()
            .filter(|n| *n >= 0)
            .context("Total de artistas inválido")?;
        let next = !body["next"].is_null();
        let after = body.pointer("/cursors/after").and_then(Value::as_str);
        if items.len() > 50
            || expected.is_some_and(|n| n != total)
            || (next && (items.is_empty() || after.is_none() || after == cursor.as_deref()))
        {
            reset_following(db, &rt.job).await?;
            return Err(paused("following_changed","A lista de seguidos mudou ou sua paginação é inconsistente. Nenhum artista foi marcado como não seguido; retome a discografia."));
        }
        let mut tx = db.begin().await?;
        for item in items {
            let id = database::identity(item)?;
            if !valid_id(&id) {
                bail!("ID de artista inválido na resposta");
            }
            database::artist(&mut tx, item).await?;
            let inserted =
                sqlx::query("INSERT OR IGNORE INTO discografia_followed_seen VALUES(?,?)")
                    .bind(&rt.job)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            if inserted != 1 {
                drop(tx);
                reset_following(db, &rt.job).await?;
                return Err(paused("following_changed","Artista repetido na paginação de seguidos. Estados anteriores preservados; retome a discografia."));
            }
        }
        let seen: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM discografia_followed_seen WHERE job_id=?")
                .bind(&rt.job)
                .fetch_one(&mut *tx)
                .await?;
        if seen > total || (!next && seen != total) {
            drop(tx);
            reset_following(db, &rt.job).await?;
            return Err(paused("following_changed","Listagem incompleta de seguidos; não foi feita reconciliação de quem deixou de ser seguido."));
        }
        if next {
            let inserted =
                sqlx::query("INSERT OR IGNORE INTO discografia_followed_cursor VALUES(?,?)")
                    .bind(&rt.job)
                    .bind(after)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            if inserted != 1 {
                drop(tx);
                reset_following(db, &rt.job).await?;
                return Err(paused(
                    "following_changed",
                    "Cursor repetido na lista de seguidos; execução pausada.",
                ));
            }
        }
        sqlx::query("UPDATE discografia_job SET following_cursor=?,following_total=?,updated_at=? WHERE id=?").bind(if next{after}else{None}).bind(total).bind(now()).bind(&rt.job).execute(&mut *tx).await?;
        if !next {
            sqlx::query("UPDATE artista_seguido SET is_followed=0,checked_at=? WHERE artista_id NOT IN(SELECT artista_id FROM discografia_followed_seen WHERE job_id=?)").bind(now()).bind(&rt.job).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO artista_seguido SELECT artista_id,1,?,? FROM discografia_followed_seen WHERE job_id=? ON CONFLICT(artista_id) DO UPDATE SET is_followed=1,last_seen_job=excluded.last_seen_job,checked_at=excluded.checked_at")
                .bind(&rt.job).bind(now()).bind(&rt.job).execute(&mut *tx).await?;
            let context: String =
                sqlx::query_scalar("SELECT catalog_context FROM discografia_job WHERE id=?")
                    .bind(&rt.job)
                    .fetch_one(&mut *tx)
                    .await?;
            let candidates=sqlx::query("SELECT f.artista_id,c.last_checked_at,c.last_full_at,COALESCE(c.needs_full_refresh,0) needs_full_refresh FROM artista_seguido f LEFT JOIN discografia_cache c ON c.artista_id=f.artista_id AND c.catalog_context=? WHERE f.is_followed=1 ORDER BY (c.last_full_at IS NOT NULL),c.last_checked_at,f.artista_id")
                .bind(&context).fetch_all(&mut *tx).await?;
            for target in &rt.opts.artists {
                if !candidates
                    .iter()
                    .any(|r| r.get::<String, _>("artista_id") == *target)
                {
                    bail!("O artista {target} não está na lista de seguidos confirmada");
                }
            }
            let mut selected = 0;
            let cutoff = Utc::now() - chrono::Duration::days(rt.opts.ttl_days as i64);
            for candidate in candidates {
                let id: String = candidate.get("artista_id");
                let last: Option<String> = candidate.get("last_checked_at");
                let full: Option<String> = candidate.get("last_full_at");
                if !rt.opts.artists.is_empty() && !rt.opts.artists.contains(&id) {
                    continue;
                }
                let fresh = last
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                    .is_some_and(|d| d > cutoff);
                let needs_full = candidate.get::<i64, _>("needs_full_refresh") == 1;
                if !needs_full
                    && !rt.opts.full
                    && rt.opts.artists.is_empty()
                    && full.is_some()
                    && fresh
                {
                    continue;
                }
                if selected >= rt.opts.max_artists {
                    break;
                }
                let mode = if full.is_none() || rt.opts.full || needs_full {
                    "full"
                } else {
                    "recent"
                };
                sqlx::query(
                    "INSERT INTO discografia_artista_queue(job_id,artista_id,mode) VALUES(?,?,?)",
                )
                .bind(&rt.job)
                .bind(&id)
                .bind(mode)
                .execute(&mut *tx)
                .await?;
                sqlx::query("INSERT INTO discografia_release_baseline SELECT ?,artista_id,album_id FROM artista_lancamento WHERE artista_id=? AND catalog_context=?")
                    .bind(&rt.job).bind(&id).bind(&context).execute(&mut *tx).await?;
                selected += 1;
            }
            sqlx::query("UPDATE discografia_job SET phase='listings',updated_at=? WHERE id=?")
                .bind(now())
                .bind(&rt.job)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        eprintln!("Artistas seguidos: {seen}/{total}");
        if !next {
            return Ok(());
        }
    }
}
fn listing_fingerprint(page: &Page) -> Result<String> {
    let mut hash = Sha256::new();
    for item in &page.items {
        hash.update(serde_json::to_vec(&(
            database::identity(item)?,
            item.get("release_date"),
            item.get("total_tracks"),
        ))?);
        hash.update(b"\n");
    }
    Ok(hex::encode(hash.finalize()))
}
async fn enqueue_album(
    db: &mut SqliteConnection,
    id: &str,
    context: &str,
    source_total: Option<i64>,
    allow_library: bool,
) -> Result<()> {
    let existing=sqlx::query("SELECT source_total,state FROM discografia_album_queue WHERE album_id=? AND album_context=?").bind(id).bind(context).fetch_optional(&mut *db).await?;
    if let Some(existing) = existing {
        if existing.get::<Option<i64>, _>("source_total") == source_total {
            return Ok(());
        }
        sqlx::query("DELETE FROM discografia_album_position WHERE album_id=? AND album_context=?")
            .bind(id)
            .bind(context)
            .execute(&mut *db)
            .await?;
        sqlx::query("UPDATE discografia_album_queue SET state='pending',source_total=?,next_offset=0,total_items=NULL,completed_at=NULL,last_error=NULL WHERE album_id=? AND album_context=?").bind(source_total).bind(id).bind(context).execute(db).await?;
        return Ok(());
    }
    let cache = sqlx::query("SELECT tracks_fetched_at,tracks_imported_total FROM album WHERE id=?")
        .bind(id)
        .fetch_one(&mut *db)
        .await?;
    let reusable = allow_library
        && cache
            .get::<Option<String>, _>("tracks_fetched_at")
            .is_some()
        && source_total.is_some()
        && source_total == cache.get::<Option<i64>, _>("tracks_imported_total");
    sqlx::query("INSERT INTO discografia_album_queue(album_id,album_context,state,source_total,next_offset,total_items,completed_at) VALUES(?,?,?,?,?,?,?)")
        .bind(id).bind(context).bind(if reusable{"completed"}else{"pending"}).bind(source_total).bind(if reusable{source_total.unwrap_or(0)}else{0}).bind(if reusable{source_total}else{None}).bind(if reusable{Some(now())}else{None}).execute(db).await?;
    Ok(())
}
async fn reset_artist(db: &mut SqliteConnection, job: &str, id: &str) -> Result<()> {
    sqlx::query("UPDATE discografia_artista_queue SET next_offset=0,total_items=NULL,previous_offset=NULL,previous_fingerprint=NULL,known_pages=0,pages_processed=0,state='pending',last_error='catalog_changed' WHERE job_id=? AND artista_id=?")
        .bind(job).bind(id).execute(db).await?;
    Ok(())
}
// Artist releases may contain fewer items than the requested ten positions.
// Follow the cursor advertised by Spotify, never an API-supplied URL itself.
fn release_next(page: &Page, artist: &str, offset: i64, total: Option<i64>) -> Result<Option<i64>> {
    if page.offset != offset
        || offset < 0
        || page.total < 0
        || total.is_some_and(|n| n != page.total)
        || page.items.len() > 10
        || (page.total > 0 && offset >= page.total)
    {
        bail!("Offset, total ou quantidade inválidos");
    }
    let boundary = offset
        .checked_add(10)
        .context("Offset excedeu o intervalo válido")?;
    if let Some(next) = &page.next {
        let url = url::Url::parse(next).context("Cursor next inválido")?;
        let offsets: Vec<_> = url.query_pairs().filter(|(k, _)| k == "offset").collect();
        let next_offset = offsets.first().and_then(|(_, v)| v.parse::<i64>().ok());
        if url.scheme() != "https"
            || url.host_str() != Some("api.spotify.com")
            || url.path() != format!("/v1/artists/{artist}/albums")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || offsets.len() != 1
            || next_offset != Some(boundary)
            || boundary >= page.total
        {
            bail!("Cursor next incompatível com a página solicitada");
        }
        Ok(next_offset)
    } else {
        if boundary < page.total
            || offset > page.total
            || page.items.len() as i64 > page.total - offset
        {
            bail!("Fim da paginação incompatível com o total");
        }
        Ok(None)
    }
}
async fn listings<A: Api>(db: &mut SqliteConnection, rt: &mut Runtime<'_, A>) -> Result<()> {
    loop {
        let row=sqlx::query("SELECT * FROM discografia_artista_queue WHERE job_id=? AND state<>'completed' ORDER BY artista_id LIMIT 1").bind(&rt.job).fetch_optional(&mut *db).await?;
        let Some(row) = row else {
            sqlx::query("UPDATE discografia_job SET phase=?,updated_at=? WHERE id=?")
                .bind(if rt.opts.catalog_only {
                    "finish"
                } else {
                    "tracks"
                })
                .bind(now())
                .bind(&rt.job)
                .execute(db)
                .await?;
            return Ok(());
        };
        let artist: String = row.get("artista_id");
        let mut offset: i64 = row.get("next_offset");
        let expected: Option<i64> = row.get("total_items");
        let previous: Option<i64> = row.get("previous_offset");
        if rt.revalidated.insert(artist.clone()) && offset > 0 {
            if let Some(previous) = previous {
                let page: Page =
                    serde_json::from_value(rt.fetch(db, Call::Releases(&artist, previous)).await?)?;
                if release_next(&page, &artist, previous, expected).is_err()
                    || Some(listing_fingerprint(&page)?)
                        != row.get::<Option<String>, _>("previous_fingerprint")
                {
                    reset_artist(db, &rt.job, &artist).await?;
                    return Err(paused("artist_catalog_changed","A paginação mudou para um artista. Apenas sua listagem foi reposicionada; retome a discografia."));
                }
                // Repair checkpoints written by versions that advanced by the
                // number of returned releases instead of the server cursor.
                offset = release_next(&page, &artist, previous, expected)?
                    .context("Artista pendente sem próxima página")?;
            }
        }
        let page: Page =
            serde_json::from_value(rt.fetch(db, Call::Releases(&artist, offset)).await?)?;
        if page.items.len() > 10 {
            bail!("Endpoint de discografia retornou mais de 10 lançamentos por página");
        }
        let next_offset = match release_next(&page, &artist, offset, expected) {
            Ok(v) => v,
            Err(error) => {
                reset_artist(db, &rt.job, &artist).await?;
                return Err(paused("artist_catalog_changed", &format!("Paginação inconsistente do artista {artist}. Offset solicitado: {offset}; recebido: {}; total esperado: {expected:?}; recebido: {}; itens: {}; next: {:?}. Causa: {error}. Outros artistas foram preservados; retome a discografia.", page.offset, page.total, page.items.len(), page.next)));
            }
        };
        let end = next_offset.is_none();
        let cursor = next_offset.unwrap_or(page.total);
        let contexts =
            sqlx::query("SELECT catalog_context,album_context FROM discografia_job WHERE id=?")
                .bind(&rt.job)
                .fetch_one(&mut *db)
                .await?;
        let catalog: String = contexts.get("catalog_context");
        let album_context: String = contexts.get("album_context");
        let mode: String = row.get("mode");
        let mut tx = db.begin().await?;
        let mut known = true;
        for object in &page.items {
            let id = database::identity(object)?;
            if !valid_id(&id) {
                bail!("ID de álbum inválido");
            }
            let before:i64=sqlx::query_scalar("SELECT COUNT(*) FROM discografia_release_baseline WHERE job_id=? AND artista_id=? AND album_id=?").bind(&rt.job).bind(&artist).bind(&id).fetch_one(&mut *tx).await?;
            if before == 0 {
                known = false;
            }
            database::album(&mut tx, object).await?;
            sqlx::query("INSERT INTO artista_lancamento VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(artista_id,album_id,catalog_context) DO UPDATE SET album_group=COALESCE(excluded.album_group,artista_lancamento.album_group),last_seen_at=excluded.last_seen_at,last_seen_job=excluded.last_seen_job")
                .bind(&artist).bind(&id).bind(&catalog).bind(&album_context).bind(rt.opts.groups.join(",")).bind(object.get("album_group").and_then(Value::as_str)).bind(now()).bind(now()).bind(&rt.job).execute(&mut *tx).await?;
            enqueue_album(
                &mut tx,
                &id,
                &album_context,
                object.get("total_tracks").and_then(Value::as_i64),
                rt.opts.market.is_none(),
            )
            .await?;
        }
        let consecutive = if known && !page.items.is_empty() {
            row.get::<i64, _>("known_pages") + 1
        } else {
            0
        };
        let finished = end || (mode == "recent" && consecutive >= 2);
        let coverage = if end { "full_listing" } else { "recent_window" };
        sqlx::query("UPDATE discografia_artista_queue SET state=?,next_offset=?,total_items=?,previous_offset=?,previous_fingerprint=?,known_pages=?,pages_processed=pages_processed+1,stop_reason=?,last_error=NULL WHERE job_id=? AND artista_id=?")
            .bind(if finished{"completed"}else{"running"}).bind(cursor).bind(page.total).bind(offset).bind(listing_fingerprint(&page)?).bind(consecutive)
            .bind(if end{Some("end_of_catalog")}else if finished{Some("known_pages")}else{None}).bind(&rt.job).bind(&artist).execute(&mut *tx).await?;
        if finished {
            sqlx::query("INSERT INTO discografia_cache(artista_id,catalog_context,last_checked_at,last_full_at,coverage,last_job_id) VALUES(?,?,?,?,?,?) ON CONFLICT(artista_id,catalog_context) DO UPDATE SET last_checked_at=excluded.last_checked_at,last_full_at=COALESCE(excluded.last_full_at,discografia_cache.last_full_at),coverage=excluded.coverage,last_job_id=excluded.last_job_id,needs_full_refresh=0")
                .bind(&artist).bind(&catalog).bind(now()).bind(if end{Some(now())}else{None}).bind(coverage).bind(&rt.job).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        eprintln!(
            "Discografia {artist}: posição {}/{}, modo {mode}; requisições {}/{}",
            cursor, page.total, rt.used, rt.limit
        );
    }
}
async fn tracks<A: Api>(db: &mut SqliteConnection, rt: &mut Runtime<'_, A>) -> Result<()> {
    loop {
        let contexts =
            sqlx::query("SELECT catalog_context,album_context FROM discografia_job WHERE id=?")
                .bind(&rt.job)
                .fetch_one(&mut *db)
                .await?;
        let catalog: String = contexts.get("catalog_context");
        let context: String = contexts.get("album_context");
        let row=sqlx::query("SELECT q.* FROM discografia_album_queue q WHERE q.album_context=? AND q.state<>'completed' AND EXISTS(SELECT 1 FROM artista_lancamento l JOIN artista_seguido f ON f.artista_id=l.artista_id AND f.is_followed=1 WHERE l.album_id=q.album_id AND l.catalog_context=?) ORDER BY q.album_id LIMIT 1")
            .bind(&context).bind(&catalog).fetch_optional(&mut *db).await?;
        let Some(row) = row else {
            sqlx::query("UPDATE discografia_job SET phase='finish',updated_at=? WHERE id=?")
                .bind(now())
                .bind(&rt.job)
                .execute(db)
                .await?;
            return Ok(());
        };
        let id: String = row.get("album_id");
        let offset: i64 = row.get("next_offset");
        let expected: Option<i64> = row.get("total_items");
        // A library sync may have finished this album since it was enqueued.
        if rt.opts.market.is_none() {
            let cached = sqlx::query(
                "SELECT tracks_fetched_at,tracks_imported_total,total_tracks FROM album WHERE id=?",
            )
            .bind(&id)
            .fetch_one(&mut *db)
            .await?;
            if cached
                .get::<Option<String>, _>("tracks_fetched_at")
                .is_some()
                && cached.get::<Option<i64>, _>("tracks_imported_total")
                    == row.get::<Option<i64>, _>("source_total")
                && row.get::<Option<i64>, _>("source_total").is_some()
            {
                sqlx::query("UPDATE discografia_album_queue SET state='completed',completed_at=?,next_offset=COALESCE(source_total,0),total_items=source_total WHERE album_id=? AND album_context=?").bind(now()).bind(&id).bind(&context).execute(&mut *db).await?;
                continue;
            }
        }
        let result = rt.fetch(db, Call::Tracks(&id, offset)).await;
        let page: Page = match result {
            Ok(v) => serde_json::from_value(v)?,
            Err(e) => {
                sqlx::query("UPDATE discografia_album_queue SET last_error=?,attempts=attempts+1 WHERE album_id=? AND album_context=?").bind(e.to_string()).bind(&id).bind(&context).execute(&mut *db).await?;
                return Err(e);
            }
        };
        let end = match page.validate(offset, expected) {
            Ok(end) => end,
            Err(_) => {
                let mut tx = db.begin().await?;
                sqlx::query(
                    "DELETE FROM discografia_album_position WHERE album_id=? AND album_context=?",
                )
                .bind(&id)
                .bind(&context)
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE discografia_album_queue SET next_offset=0,total_items=NULL,state='pending',last_error='album_changed' WHERE album_id=? AND album_context=?").bind(&id).bind(&context).execute(&mut *tx).await?;
                tx.commit().await?;
                return Err(paused("album_changed","As faixas de um álbum mudaram durante a paginação. Apenas esse álbum foi reposicionado; retome a discografia."));
            }
        };
        let mut tx = db.begin().await?;
        for (i, item) in page.items.iter().enumerate() {
            let track = database::track(&mut tx, item, &id).await?;
            sqlx::query("INSERT INTO discografia_album_position VALUES(?,?,?,?) ON CONFLICT(album_id,album_context,position) DO UPDATE SET musica_id=excluded.musica_id")
                .bind(&id).bind(&context).bind(offset+i as i64).bind(track).execute(&mut *tx).await?;
        }
        if end {
            let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM discografia_album_position WHERE album_id=? AND album_context=?").bind(&id).bind(&context).fetch_one(&mut *tx).await?;
            if count != page.total {
                bail!("Álbum incompleto; cobertura de posições não corresponde ao total");
            }
            if rt.opts.market.is_none() {
                sqlx::query(
                    "UPDATE album SET tracks_fetched_at=?,tracks_imported_total=? WHERE id=?",
                )
                .bind(now())
                .bind(page.total)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query("UPDATE discografia_album_queue SET state=?,next_offset=?,total_items=?,attempts=attempts+1,last_error=NULL,completed_at=? WHERE album_id=? AND album_context=?")
            .bind(if end{"completed"}else{"running"}).bind(offset+page.items.len() as i64).bind(page.total).bind(if end{Some(now())}else{None}).bind(&id).bind(&context).execute(&mut *tx).await?;
        tx.commit().await?;
        eprintln!(
            "Faixas {id}: {}/{}, requisições {}/{}",
            offset + page.items.len() as i64,
            page.total,
            rt.used,
            rt.limit
        );
    }
}

pub async fn run<A: Api>(db: &mut SqliteConnection, api: &mut A, job: &str) -> Result<Value> {
    sync::check_cooldown(db).await?;
    let row =
        sqlx::query("SELECT options_json,request_limit,status FROM discografia_job WHERE id=?")
            .bind(job)
            .fetch_one(&mut *db)
            .await?;
    if row.get::<String, _>("status") == "completed" {
        return status(db).await;
    }
    let mut rt = Runtime {
        api,
        job: job.to_owned(),
        opts: serde_json::from_str(row.get("options_json"))?,
        limit: row.get::<i64, _>("request_limit") as u32,
        used: 0,
        revalidated: HashSet::new(),
    };
    sqlx::query("UPDATE discografia_job SET invocation_requests=0 WHERE id=?")
        .bind(job)
        .execute(&mut *db)
        .await?;
    let profile = rt.fetch(db, Call::Profile).await?;
    let account = profile["id"].as_str().context("Perfil Spotify sem ID")?;
    database::bind_account(db, account).await?;
    let (catalog, album) = rt.opts.contexts(account);
    sqlx::query("UPDATE discografia_job SET account_id=?,catalog_context=?,album_context=?,updated_at=? WHERE id=?").bind(account).bind(catalog).bind(album).bind(now()).bind(job).execute(&mut *db).await?;
    loop {
        let phase: String = sqlx::query_scalar("SELECT phase FROM discografia_job WHERE id=?")
            .bind(job)
            .fetch_one(&mut *db)
            .await?;
        match phase.as_str() {
            "following" => following(db, &mut rt).await?,
            "listings" => listings(db, &mut rt).await?,
            "tracks" => tracks(db, &mut rt).await?,
            "finish" => {
                let mut tx = db.begin().await?;
                sqlx::query("UPDATE discografia_job SET status='completed',phase='done',completed_at=?,updated_at=?,last_error=NULL,pause_reason=NULL WHERE id=?").bind(now()).bind(now()).bind(job).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM discografia_release_baseline WHERE job_id=?")
                    .bind(job)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return status(db).await;
            }
            "done" => return status(db).await,
            _ => bail!("Etapa de discografia desconhecida: {phase}"),
        }
    }
}

pub async fn status(db: &mut SqliteConnection) -> Result<Value> {
    let job:Option<String>=sqlx::query_scalar("SELECT json_object('id',id,'kind',kind,'status',status,'phase',phase,'request_limit',request_limit,'requests',request_count,'retries',retry_count,'invocation_requests',invocation_requests,'following_total',following_total,'following_cursor',following_cursor,'last_error',last_error,'pause_reason',pause_reason,'updated_at',updated_at,'completed_at',completed_at,'options',json(options_json)) FROM discografia_job ORDER BY started_at DESC LIMIT 1").fetch_optional(&mut *db).await?;
    let counts=sqlx::query("SELECT (SELECT COUNT(*) FROM artista_seguido WHERE is_followed=1) followed,(SELECT COUNT(*) FROM discografia_artista_queue WHERE job_id=(SELECT id FROM discografia_job ORDER BY started_at DESC LIMIT 1)) selected,(SELECT COUNT(*) FROM discografia_artista_queue WHERE job_id=(SELECT id FROM discografia_job ORDER BY started_at DESC LIMIT 1) AND state='completed') listed,(SELECT COUNT(DISTINCT album_id) FROM artista_lancamento) albums,(SELECT COUNT(*) FROM discografia_album_queue WHERE state<>'completed') pending_albums,(SELECT COUNT(*) FROM discografia_album_queue WHERE state='completed') complete_album_contexts").fetch_one(&mut *db).await?;
    let active:i64=sqlx::query_scalar("SELECT COUNT(*) FROM discografia_album_queue q WHERE q.state<>'completed' AND q.album_context=(SELECT album_context FROM discografia_job ORDER BY started_at DESC LIMIT 1) AND EXISTS(SELECT 1 FROM artista_lancamento l JOIN artista_seguido f ON f.artista_id=l.artista_id AND f.is_followed=1 WHERE l.album_id=q.album_id AND l.catalog_context=(SELECT catalog_context FROM discografia_job ORDER BY started_at DESC LIMIT 1))").fetch_one(&mut *db).await?;
    Ok(
        json!({"job":job.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,"followed_artists":counts.get::<i64,_>("followed"),"selected_artists":counts.get::<i64,_>("selected"),"artist_listings_completed":counts.get::<i64,_>("listed"),"unique_albums":counts.get::<i64,_>("albums"),"pending_albums_active_context":active,"pending_albums_all_contexts":counts.get::<i64,_>("pending_albums"),"completed_album_contexts":counts.get::<i64,_>("complete_album_contexts")}),
    )
}
pub async fn artists(db: &mut SqliteConnection, limit: u32, offset: u32) -> Result<Value> {
    let rows=sqlx::query("SELECT a.id,a.name,a.spotify_url,f.checked_at,c.last_checked_at,c.last_full_at,c.coverage,(SELECT COUNT(*) FROM artista_lancamento l WHERE l.artista_id=a.id AND l.catalog_context=c.catalog_context) known_albums,(SELECT COUNT(*) FROM artista_lancamento l JOIN discografia_album_queue q ON q.album_id=l.album_id AND q.album_context=l.album_context WHERE l.artista_id=a.id AND l.catalog_context=c.catalog_context AND q.state<>'completed') pending_albums FROM artista_seguido f JOIN artista a ON a.id=f.artista_id LEFT JOIN discografia_cache c ON c.artista_id=a.id AND c.catalog_context=(SELECT catalog_context FROM discografia_job ORDER BY started_at DESC LIMIT 1) WHERE f.is_followed=1 ORDER BY a.name,a.id LIMIT ? OFFSET ?")
        .bind(limit).bind(offset).fetch_all(db).await?;
    Ok(json!(rows.iter().map(|r|json!({"id":r.get::<String,_>("id"),"name":r.get::<Option<String>,_>("name"),"spotify_url":r.get::<Option<String>,_>("spotify_url"),"is_followed":true,"checked_at":r.get::<String,_>("checked_at"),"last_catalog_check":r.get::<Option<String>,_>("last_checked_at"),"last_full_listing":r.get::<Option<String>,_>("last_full_at"),"coverage":r.get::<Option<String>,_>("coverage"),"known_albums":r.get::<i64,_>("known_albums"),"pending_albums":r.get::<i64,_>("pending_albums"),"known_catalog_tracks_complete":r.get::<Option<String>,_>("last_full_at").is_some() && r.get::<i64,_>("pending_albums")==0})).collect::<Vec<_>>()))
}

pub async fn albums(
    db: &mut SqliteConnection,
    artist: &str,
    limit: u32,
    offset: u32,
) -> Result<Value> {
    if !valid_id(artist) {
        bail!("ID de artista inválido");
    }
    let rows=sqlx::query("SELECT DISTINCT a.id,a.name,a.release_date,a.spotify_url,a.is_saved FROM artista_lancamento l JOIN album a ON a.id=l.album_id WHERE l.artista_id=? ORDER BY a.release_date DESC,a.id LIMIT ? OFFSET ?")
        .bind(artist).bind(limit).bind(offset).fetch_all(db).await?;
    Ok(json!(rows.iter().map(|r|json!({"id":r.get::<String,_>("id"),"name":r.get::<Option<String>,_>("name"),"release_date":r.get::<Option<String>,_>("release_date"),"spotify_url":r.get::<Option<String>,_>("spotify_url"),"is_saved":r.get::<i64,_>("is_saved")==1})).collect::<Vec<_>>()))
}
