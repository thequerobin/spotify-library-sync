use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use spotify_library_sync::{
    database,
    spotify::{Api, BudgetExhausted},
    sync::{self, Options, WorkPaused},
};
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::VecDeque;
use tempfile::TempDir;
const OLD: &str = "2026-10-01T00:00:00.000Z";
const NEW: &str = "2026-11-05T00:00:00.000Z";
fn track(id: &str, date: &str) -> Value {
    json!({"added_at":date,"track":{"id":id,"name":id,"album":{"id":"parent","name":"Parent","artists":[]},"artists":[]}})
}
fn album(id: &str, date: &str) -> Value {
    json!({"added_at":date,"album":{"id":id,"name":id,"total_tracks":0,"artists":[],"tracks":{"offset":0,"total":0,"next":null,"items":[]}}})
}
struct Library {
    tracks: Vec<Value>,
    albums: Vec<Value>,
    calls: Vec<(String, i64)>,
    fail_at: Option<usize>,
    mutate_at: Option<usize>,
    contains: VecDeque<Value>,
    album_pages: VecDeque<Value>,
}
impl Library {
    fn new(n: usize, a: usize) -> Self {
        Self {
            tracks: (0..n).map(|i| track(&format!("T{i:06}"), OLD)).collect(),
            albums: (0..a).map(|i| album(&format!("A{i:06}"), OLD)).collect(),
            calls: vec![],
            fail_at: None,
            mutate_at: None,
            contains: VecDeque::new(),
            album_pages: VecDeque::new(),
        }
    }
}
#[async_trait]
impl Api for Library {
    async fn get(&mut self, path: &str, offset: Option<i64>) -> Result<Value> {
        if self.fail_at == Some(self.calls.len()) {
            bail!("simulated network failure");
        }
        if self.mutate_at == Some(self.calls.len()) {
            self.tracks.insert(0, track("Concurrent", NEW));
            self.mutate_at = None;
        }
        let offset = offset.unwrap_or(0);
        self.calls.push((path.to_owned(), offset));
        if path == "/me" {
            return Ok(json!({"id":"account"}));
        }
        if path.starts_with("/albums/") {
            return self
                .album_pages
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("Unexpected album call: {path}"));
        }
        let items = match path {
            "/me/tracks" => &self.tracks,
            "/me/albums" => &self.albums,
            _ => bail!("Unexpected expensive call: {path}"),
        };
        let start = (offset as usize).min(items.len());
        let end = (start + 50).min(items.len());
        Ok(
            json!({"offset":offset,"total":items.len(),"items":items[start..end],"next":if end<items.len(){Some("ignored")}else{None}}),
        )
    }
    async fn contains(&mut self, uris: &[String]) -> Result<Value> {
        assert!(uris.len() <= 40);
        self.calls.push(("contains".into(), uris.len() as i64));
        Ok(self
            .contains
            .pop_front()
            .expect("contains response missing"))
    }
    fn attempt_count(&self) -> u64 {
        self.calls.len() as u64
    }
}
async fn setup(n: usize, a: usize) -> Result<(TempDir, SqliteConnection)> {
    let dir = tempfile::tempdir()?;
    let mut db = database::open(&dir.path().join("db.sqlite")).await?;
    database::bind_account(&mut db, "account").await?;
    let stamp = database::now();
    sqlx::query("INSERT INTO album(id,name,raw_json,metadata_fetched_at,created_at,updated_at) VALUES('parent','Parent','{}',?,?,?)").bind(&stamp).bind(&stamp).bind(&stamp).execute(&mut db).await?;
    if n > 0 {
        sqlx::query("WITH RECURSIVE n(i) AS(SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i+1<?) INSERT INTO musica(id,album_id,name,is_saved,saved_at,raw_json,metadata_fetched_at,created_at,updated_at) SELECT printf('T%06d',i),'parent',printf('T%06d',i),1,?,'{}',?,?,? FROM n")
 .bind(n as i64).bind(OLD).bind(&stamp).bind(&stamp).bind(&stamp).execute(&mut db).await?;
    }
    if a > 0 {
        sqlx::query("WITH RECURSIVE n(i) AS(SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i+1<?) INSERT INTO album(id,name,is_saved,saved_at,total_tracks,tracks_imported_total,tracks_fetched_at,raw_json,metadata_fetched_at,created_at,updated_at) SELECT printf('A%06d',i),printf('A%06d',i),1,?,0,0,'2000-01-01T00:00:00Z','{}',?,?,? FROM n")
 .bind(a as i64).bind(OLD).bind(&stamp).bind(&stamp).bind(&stamp).execute(&mut db).await?;
    }
    sqlx::query("UPDATE library_sync_state SET initial_import_completed_at=?,last_full_reconciliation_at=?,last_coverage='full_listing'").bind(&stamp).bind(&stamp).execute(&mut db).await?;
    Ok((dir, db))
}
async fn count(db: &mut SqliteConnection, sql: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(sql).fetch_one(db).await?)
}

#[tokio::test]
async fn large_library_without_changes_uses_five_calls_with_profile_and_never_refreshes_expired_cache(
) -> Result<()> {
    let (_dir, mut db) = setup(21948, 150).await?;
    let mut api = Library::new(21948, 150);
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    api.get("/me", None).await?;
    let result = sync::run(&mut db, &mut api, &job, 0).await?;
    assert_eq!(api.calls.len(), 5);
    assert_eq!(result["coverage"], "recent_window");
    assert_eq!(
        api.calls,
        vec![
            ("/me".into(), 0),
            ("/me/tracks".into(), 0),
            ("/me/tracks".into(), 50),
            ("/me/albums".into(), 0),
            ("/me/albums".into(), 50)
        ]
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        21948
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM sync_album_queue").await?,
        0
    );
    Ok(())
}
#[tokio::test]
async fn five_new_tracks_stop_after_three_pages_and_one_head_check() -> Result<()> {
    let (_dir, mut db) = setup(200, 150).await?;
    let mut api = Library::new(200, 150);
    let additions: Vec<_> = (0..5).map(|i| track(&format!("New{i}"), NEW)).collect();
    api.tracks.splice(0..0, additions);
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(api.calls.len(), 6);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        205
    );
    let pages: Vec<_> = api
        .calls
        .iter()
        .filter(|(path, _)| path == "/me/tracks")
        .map(|(_, offset)| *offset)
        .collect();
    assert_eq!(pages, vec![0, 50, 100, 0]);
    Ok(())
}
#[tokio::test]
async fn equal_totals_and_timestamp_ties_do_not_hide_new_or_reliked_catalog_items() -> Result<()> {
    let (_dir, mut db) = setup(200, 0).await?;
    let mut api = Library::new(200, 0);
    api.tracks.pop();
    api.tracks.insert(0, track("UnknownSameTime", OLD));
    sqlx::query("UPDATE musica SET is_saved=0 WHERE id='T000010'")
        .execute(&mut db)
        .await?;
    api.tracks
        .iter_mut()
        .find(|v| v["track"]["id"] == "T000010")
        .unwrap()["added_at"] = json!(OLD);
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(
        count(&mut db, "SELECT is_saved FROM musica WHERE id='T000010'").await?,
        1
    );
    assert_eq!(
        count(&mut db, "SELECT is_saved FROM musica WHERE id='T000199'").await?,
        1
    ); // unseen removal is not invented
    assert_eq!(
        count(
            &mut db,
            "SELECT is_saved FROM musica WHERE id='UnknownSameTime'"
        )
        .await?,
        1
    );
    Ok(())
}
#[tokio::test]
async fn relike_date_is_updated_even_for_an_existing_saved_id() -> Result<()> {
    let (_dir, mut db) = setup(150, 0).await?;
    let mut api = Library::new(150, 0);
    let mut item = api.tracks.remove(100);
    item["added_at"] = json!(NEW);
    api.tracks.insert(0, item);
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    sync::run(&mut db, &mut api, &job, 30).await?;
    let date: String = sqlx::query_scalar("SELECT saved_at FROM musica WHERE id='T000100'")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(date, NEW);
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 150);
    let pages: Vec<_> = api
        .calls
        .iter()
        .filter(|(path, _)| path == "/me/tracks")
        .map(|(_, offset)| *offset)
        .collect();
    assert_eq!(pages, vec![0, 50, 100, 0]);
    Ok(())
}

#[tokio::test]
async fn changed_dates_update_tracks_and_albums_without_requiring_chronological_order() -> Result<()>
{
    let (_dir, mut db) = setup(300, 300).await?;
    let mut api = Library::new(300, 300);
    let older = "2026-09-01T00:00:00.000Z";
    // Include a confirmed ID with a previously unknown date.
    sqlx::query("UPDATE album SET saved_at=NULL WHERE id='A000050'")
        .execute(&mut db)
        .await?;
    for items in [&mut api.tracks, &mut api.albums] {
        items[10]["added_at"] = json!(NEW);
        items[49]["added_at"] = json!(older);
        items[50]["added_at"] = json!(NEW);
    }
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    let summary = sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(summary["coverage"], "recent_window");
    for (table, prefix, endpoint) in [("musica", "T", "/me/tracks"), ("album", "A", "/me/albums")] {
        for (index, expected) in [(10, NEW), (49, older), (50, NEW)] {
            let row = sqlx::query(&format!("SELECT saved_at,is_saved FROM {table} WHERE id=?"))
                .bind(format!("{prefix}{index:06}"))
                .fetch_one(&mut db)
                .await?;
            assert_eq!(row.get::<String, _>("saved_at"), expected);
            assert_eq!(row.get::<i64, _>("is_saved"), 1);
        }
        assert_eq!(
            count(
                &mut db,
                &format!("SELECT COUNT(*) FROM {table} WHERE is_saved=1")
            )
            .await?,
            300
        );
        let pages: Vec<_> = api
            .calls
            .iter()
            .filter(|(path, _)| path == endpoint)
            .map(|(_, offset)| *offset)
            .collect();
        // Both changed pages reset the known-page counter. No full scan occurs.
        assert_eq!(pages, vec![0, 50, 100, 150, 0]);
    }
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 300);
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM album").await?, 301);
    assert!(api
        .calls
        .iter()
        .all(|(path, _)| !path.starts_with("/albums/")));
    Ok(())
}

#[tokio::test]
async fn changed_date_at_page_boundary_survives_interruption_and_resume() -> Result<()> {
    let (dir, mut db) = setup(300, 0).await?;
    let mut api = Library::new(300, 0);
    let older = "2026-09-01T00:00:00.000Z";
    api.tracks[49]["added_at"] = json!(older);
    api.tracks[50]["added_at"] = json!(NEW);
    api.fail_at = Some(1);
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    let error = sync::run(&mut db, &mut api, &job, 30).await.unwrap_err();
    assert!(error.to_string().contains("simulated network failure"));
    sync::record_error(&mut db, &job, &error).await?;
    assert_eq!(
        count(
            &mut db,
            "SELECT next_offset FROM sync_scope WHERE collection='track'"
        )
        .await?,
        50
    );
    let previous: String =
        sqlx::query_scalar("SELECT previous_added_at FROM sync_scope WHERE collection='track'")
            .fetch_one(&mut db)
            .await?;
    assert_eq!(previous, OLD);
    let date: String = sqlx::query_scalar("SELECT saved_at FROM musica WHERE id='T000049'")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(date, OLD, "an unfinished collection must not publish dates");
    db.close().await?;

    db = database::open(&dir.path().join("db.sqlite")).await?;
    api.fail_at = None;
    api.calls.clear();
    assert_eq!(
        sync::prepare_sync(&mut db, true, &Options::default()).await?,
        job
    );
    sync::run(&mut db, &mut api, &job, 30).await?;
    let pages: Vec<_> = api
        .calls
        .iter()
        .filter(|(path, _)| path == "/me/tracks")
        .map(|(_, offset)| *offset)
        .collect();
    assert_eq!(pages, vec![0, 50, 100, 150, 0]);
    let dates: Vec<String> = sqlx::query_scalar(
        "SELECT saved_at FROM musica WHERE id IN ('T000049','T000050') ORDER BY id",
    )
    .fetch_all(&mut db)
    .await?;
    assert_eq!(dates, vec![older.to_owned(), NEW.to_owned()]);
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 300);
    Ok(())
}

#[tokio::test]
async fn relike_between_interruption_and_resume_revalidates_the_head() -> Result<()> {
    let (_dir, mut db) = setup(300, 0).await?;
    let mut api = Library::new(300, 0);
    api.fail_at = Some(1);
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    let error = sync::run(&mut db, &mut api, &job, 30).await.unwrap_err();
    sync::record_error(&mut db, &job, &error).await?;
    api.fail_at = None;
    let mut item = api.tracks.remove(200);
    item["added_at"] = json!(NEW);
    api.tracks.insert(0, item);
    sync::prepare_sync(&mut db, true, &Options::default()).await?;
    let error = sync::run(&mut db, &mut api, &job, 30).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<WorkPaused>().unwrap().reason,
        "window_changed"
    );
    sync::record_error(&mut db, &job, &error).await?;
    sync::prepare_sync(&mut db, true, &Options::default()).await?;
    sync::run(&mut db, &mut api, &job, 30).await?;
    let date: String = sqlx::query_scalar("SELECT saved_at FROM musica WHERE id='T000200'")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(date, NEW);
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 300);
    assert!(api
        .calls
        .iter()
        .all(|(path, _)| path == "/me/tracks" || path == "/me/albums"));
    Ok(())
}
#[tokio::test]
async fn interrupted_budgeted_discovery_retains_baseline_and_eventually_advances_beyond_first_budget(
) -> Result<()> {
    let (dir, mut db) = setup(150, 0).await?;
    let mut api = Library::new(150, 0);
    api.tracks
        .splice(0..0, (0..300).map(|i| track(&format!("New{i:06}"), NEW)));
    let opts = Options {
        collection_budget: Some(5),
        ..Options::default()
    };
    let job = sync::prepare_sync(&mut db, false, &opts).await?;
    let mut offsets = vec![];
    let mut completed = false;
    for _ in 0..10 {
        let result = sync::run(&mut db, &mut api, &job, 30).await;
        match result {
            Ok(_) => {
                completed = true;
                break;
            }
            Err(e) => {
                assert!(e.is::<BudgetExhausted>(), "{e:#}");
                sync::record_error(&mut db, &job, &e).await?;
                assert_eq!(
                    count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
                    150
                );
                assert_eq!(
                    count(
                        &mut db,
                        "SELECT COUNT(*) FROM sync_baseline_item WHERE collection='track'"
                    )
                    .await?,
                    150
                );
                let offset = count(
                    &mut db,
                    "SELECT next_offset FROM sync_scope WHERE collection='track'",
                )
                .await?;
                assert!(offsets.last().is_none_or(|old| offset > *old));
                offsets.push(offset);
                db.close().await?;
                db = database::open(&dir.path().join("db.sqlite")).await?;
                assert_eq!(
                    sync::prepare_sync(&mut db, true, &Options::default()).await?,
                    job
                );
            }
        }
    }
    assert!(completed, "resume must converge");
    assert!(offsets.len() > 1);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        450
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM sync_baseline_item").await?,
        0
    );
    Ok(())
}
#[tokio::test]
async fn failed_album_stage_does_not_repeat_published_tracks() -> Result<()> {
    let (_dir, mut db) = setup(150, 150).await?;
    let mut api = Library::new(150, 150);
    api.fail_at = Some(2);
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    let e = sync::run(&mut db, &mut api, &job, 30).await.unwrap_err();
    sync::record_error(&mut db, &job, &e).await?;
    assert_eq!(
        count(
            &mut db,
            "SELECT COUNT(*) FROM sync_scope WHERE collection='track' AND state='published'"
        )
        .await?,
        1
    );
    let track_calls = api.calls.iter().filter(|(p, _)| p == "/me/tracks").count();
    api.fail_at = None;
    sync::prepare_sync(&mut db, true, &Options::default()).await?;
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(
        api.calls.iter().filter(|(p, _)| p == "/me/tracks").count(),
        track_calls
    );
    Ok(())
}
#[tokio::test]
async fn concurrent_head_edit_pauses_recent_window_and_never_runs_full_scan() -> Result<()> {
    let (_dir, mut db) = setup(200, 0).await?;
    let mut api = Library::new(200, 0);
    api.tracks.insert(0, track("New", NEW));
    api.mutate_at = Some(3);
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    let e = sync::run(&mut db, &mut api, &job, 30).await.unwrap_err();
    assert!(e.is::<WorkPaused>());
    sync::record_error(&mut db, &job, &e).await?;
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        200
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT next_offset FROM sync_scope WHERE collection='track'"
        )
        .await?,
        0
    );
    assert_eq!(api.calls.len(), 4);
    assert!(api.calls.iter().all(|(p, _)| p == "/me/tracks"));
    sync::prepare_sync(&mut db, true, &Options::default()).await?;
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        202
    );
    Ok(())
}
#[tokio::test]
async fn unknown_item_out_of_order_never_publishes_or_advances_frontier() -> Result<()> {
    let (_dir, mut db) = setup(150, 0).await?;
    let mut api = Library::new(150, 0);
    api.tracks[10] = track("UnknownOutOfOrder", NEW);
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    let e = sync::run(&mut db, &mut api, &job, 30).await.unwrap_err();
    assert_eq!(
        e.downcast_ref::<WorkPaused>().unwrap().reason,
        "ordering_inconclusive"
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT next_offset FROM sync_scope WHERE collection='track'"
        )
        .await?,
        0
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM sync_seen_item").await?,
        0
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        150
    );
    Ok(())
}
#[tokio::test]
async fn explicit_reconcile_detects_old_removals_and_full_guard_rejects_incremental() -> Result<()>
{
    let (_dir, mut db) = setup(150, 0).await?;
    let mut api = Library::new(149, 0);
    let job = sync::prepare_sync(
        &mut db,
        false,
        &Options {
            reconcile: true,
            ..Options::default()
        },
    )
    .await?;
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(
        count(&mut db, "SELECT is_saved FROM musica WHERE id='T000149'").await?,
        0
    );
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    sqlx::query("UPDATE sync_job SET stage='reconcile' WHERE id=?")
        .bind(&job)
        .execute(&mut db)
        .await?;
    assert!(database::finish(&mut db, &job).await.is_err());
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        149
    );
    Ok(())
}
#[tokio::test]
async fn selected_contains_batches_validate_response_before_mutating_and_do_not_fetch_unknown_metadata(
) -> Result<()> {
    let (_dir, mut db) = setup(100, 0).await?;
    let mut api = Library::new(100, 0);
    let uris: Vec<_> = (0..41).map(|i| format!("spotify:track:T{i:06}")).collect();
    api.contains.push_back(json!(vec![false; 40]));
    api.contains.push_back(json!([true]));
    sync::check_saved(&mut db, &mut api, &uris).await?;
    assert_eq!(
        api.calls,
        vec![("contains".into(), 40), ("contains".into(), 1)]
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        60
    );
    api.contains.push_back(json!([]));
    assert!(
        sync::check_saved(&mut db, &mut api, &["spotify:track:T000040".into()])
            .await
            .is_err()
    );
    assert_eq!(
        count(&mut db, "SELECT is_saved FROM musica WHERE id='T000040'").await?,
        1
    );
    api.contains.push_back(json!([true]));
    let result = sync::check_saved(&mut db, &mut api, &["spotify:track:Unknown".into()]).await?;
    assert_eq!(result["items"][0]["local_updated"], false);
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 100);
    Ok(())
}
#[tokio::test]
async fn restart_preserves_pending_album_work_without_requeueing_completed_copies() -> Result<()> {
    let (_dir, mut db) = setup(100, 1).await?;
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    sqlx::query("INSERT INTO sync_album_queue(job_id,album_id,status,next_offset,total_items,updated_at) VALUES(?,'A000000','pending',50,100,?)").bind(&job).bind(database::now()).execute(&mut db).await?;
    sync::restart(&mut db).await?;
    let next = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    let row = sqlx::query("SELECT next_offset FROM sync_album_queue WHERE job_id=?")
        .bind(&next)
        .fetch_one(&mut db)
        .await?;
    assert_eq!(row.get::<i64, _>("next_offset"), 50);
    sqlx::query("UPDATE sync_album_queue SET status='completed' WHERE job_id=?")
        .bind(&next)
        .execute(&mut db)
        .await?;
    sync::restart(&mut db).await?;
    let last = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sync_album_queue WHERE job_id=?")
            .bind(last)
            .fetch_one(&mut db)
            .await?,
        0
    );
    Ok(())
}
#[tokio::test]
async fn migration_recognizes_completed_v010_jobs_and_preserves_inflight_legacy_resume(
) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy.db");
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let mut db = SqliteConnection::connect_with(&opts).await?;
    for sql in [
        include_str!("../migrations/0001_initial.sql"),
        include_str!("../migrations/0002_observations.sql"),
        include_str!("../migrations/0003_track_cache_version.sql"),
    ] {
        sqlx::raw_sql(sql).execute(&mut db).await?;
    }
    // Establish migration checksums exactly as sqlx does for the first three versions.
    sqlx::raw_sql("CREATE TABLE _sqlx_migrations(version BIGINT PRIMARY KEY,description TEXT NOT NULL,installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,success BOOLEAN NOT NULL,checksum BLOB NOT NULL,execution_time BIGINT NOT NULL)").execute(&mut db).await?;
    let migrator = sqlx::migrate!();
    for m in migrator.iter().take(3) {
        sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(?,?,1,?,0)").bind(m.version).bind(&m.description).bind(m.checksum.as_ref()).execute(&mut db).await?;
    }
    sqlx::query("INSERT INTO sync_job(id,job_type,status,stage,started_at,updated_at,completed_at) VALUES('old','normal','completed','done',?,?,?)").bind(OLD).bind(OLD).bind(OLD).execute(&mut db).await?;
    sqlx::query("INSERT INTO sync_job(id,job_type,status,stage,next_offset,started_at,updated_at) VALUES('pending','normal','failed','tracks',50,?,?)").bind(OLD).bind(OLD).execute(&mut db).await?;
    db.close().await?;
    let mut db = database::open(&path).await?;
    assert_eq!(
        count(
            &mut db,
            "SELECT COUNT(*) FROM library_sync_state WHERE initial_import_completed_at IS NOT NULL"
        )
        .await?,
        2
    );
    assert_eq!(
        sync::prepare_sync(&mut db, true, &Options::default()).await?,
        "pending"
    );
    let mode: String = sqlx::query_scalar("SELECT sync_mode FROM sync_job WHERE id='pending'")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(mode, "legacy");
    assert_eq!(
        count(
            &mut db,
            "SELECT next_offset FROM sync_job WHERE id='pending'"
        )
        .await?,
        50
    );
    sync::restart(&mut db).await?;
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    let mode: String = sqlx::query_scalar("SELECT sync_mode FROM sync_job WHERE id=?")
        .bind(job)
        .fetch_one(&mut db)
        .await?;
    assert_eq!(mode, "incremental");
    Ok(())
}
#[tokio::test]
async fn fresh_database_starts_initial_and_selected_states_alone_do_not_fake_a_complete_baseline(
) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut db = database::open(&dir.path().join("new.db")).await?;
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    let mode: String = sqlx::query_scalar("SELECT sync_mode FROM sync_job WHERE id=?")
        .bind(&job)
        .fetch_one(&mut db)
        .await?;
    assert_eq!(mode, "initial");
    let mut api = Library::new(0, 0);
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(
        count(
            &mut db,
            "SELECT COUNT(*) FROM library_sync_state WHERE initial_import_completed_at IS NOT NULL"
        )
        .await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn targeted_check_rate_limit_survives_restart_without_any_sync_job() -> Result<()> {
    let (dir, mut db) = setup(1, 0).await?;
    let e = anyhow::Error::new(spotify_library_sync::spotify::Paused {
        message: "HTTP 429".into(),
        retry_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
    });
    sync::remember_cooldown(&mut db, &e).await?;
    db.close().await?;
    let mut db = database::open(&dir.path().join("db.sqlite")).await?;
    assert!(sync::check_cooldown(&mut db).await.is_err());
    assert!(sync::restart(&mut db).await.is_err());
    assert!(sync::prepare_sync(&mut db, false, &Options::default())
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn album_queue_budget_resumes_only_remaining_tracks_after_publishing_collections(
) -> Result<()> {
    let (_dir, mut db) = setup(150, 100).await?;
    let mut api = Library::new(150, 100);
    let mut new = album("NewAlbum", NEW);
    new["album"]["total_tracks"] = json!(4);
    new["album"]["tracks"] = json!({"offset":0,"total":4,"next":"ignored","items":[{"id":"AlbumTrack0","name":"0","artists":[]}]});
    api.albums.insert(0, new);
    api.album_pages.push_back(json!({"offset":1,"total":4,"next":"ignored","items":[{"id":"AlbumTrack1","name":"1","artists":[]}]}));
    api.album_pages.push_back(json!({"offset":2,"total":4,"next":null,"items":[{"id":"AlbumTrack2","name":"2","artists":[]},{"id":"AlbumTrack3","name":"3","artists":[]}]}));
    let job = sync::prepare_sync(
        &mut db,
        false,
        &Options {
            album_budget: Some(1),
            ..Options::default()
        },
    )
    .await?;
    let e = sync::run(&mut db, &mut api, &job, 30).await.unwrap_err();
    assert!(e.is::<BudgetExhausted>());
    sync::record_error(&mut db, &job, &e).await?;
    assert_eq!(
        count(&mut db, "SELECT is_saved FROM album WHERE id='NewAlbum'").await?,
        1
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT next_offset FROM sync_album_queue WHERE album_id='NewAlbum'"
        )
        .await?,
        2
    );
    let collection_calls = api
        .calls
        .iter()
        .filter(|(p, _)| p.starts_with("/me/"))
        .count();
    sync::prepare_sync(&mut db, true, &Options::default()).await?;
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(
        api.calls
            .iter()
            .filter(|(p, _)| p.starts_with("/me/"))
            .count(),
        collection_calls
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT tracks_imported_total FROM album WHERE id='NewAlbum'"
        )
        .await?,
        4
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT COUNT(*) FROM musica WHERE id LIKE 'AlbumTrack%' AND is_saved=0"
        )
        .await?,
        4
    );
    Ok(())
}

#[tokio::test]
async fn changed_album_track_count_is_refreshed_inside_recent_window() -> Result<()> {
    let (_dir, mut db) = setup(0, 100).await?;
    let mut api = Library::new(0, 100);
    api.albums[0]["album"]["total_tracks"] = json!(2);
    api.albums[0]["album"]["tracks"] =
        json!({"offset":0,"total":2,"next":"ignored","items":[{"id":"Changed0","artists":[]}]});
    api.album_pages.push_back(
        json!({"offset":1,"total":2,"next":null,"items":[{"id":"Changed1","artists":[]}]}),
    );
    let job = sync::prepare_sync(&mut db, false, &Options::default()).await?;
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(
        count(
            &mut db,
            "SELECT tracks_imported_total FROM album WHERE id='A000000'"
        )
        .await?,
        2
    );
    assert_eq!(
        api.calls
            .iter()
            .filter(|(p, _)| p.starts_with("/albums/"))
            .count(),
        1
    );
    Ok(())
}
