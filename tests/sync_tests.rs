use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use spotify_library_sync::{
    database,
    spotify::{Api, Paused},
    sync,
};
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::VecDeque;
use tempfile::TempDir;

struct Mock {
    responses: VecDeque<(&'static str, i64, Value)>,
    calls: usize,
    fail_after: Option<usize>,
}
#[async_trait]
impl Api for Mock {
    async fn get(&mut self, path: &str, offset: Option<i64>) -> Result<Value> {
        if self.fail_after == Some(self.calls) {
            bail!("simulated interruption");
        }
        self.calls += 1;
        let (expected, at, value) = self.responses.pop_front().expect("unexpected API call");
        assert_eq!((path, offset.unwrap_or(0)), (expected, at));
        Ok(value)
    }
}
fn mock(responses: Vec<(&'static str, i64, Value)>) -> Mock {
    Mock {
        responses: responses.into(),
        calls: 0,
        fail_after: None,
    }
}
fn page(offset: i64, total: i64, items: Vec<Value>) -> Value {
    let next = if offset + (items.len() as i64) < total {
        json!("https://api.spotify.com/v1/ignored")
    } else {
        Value::Null
    };
    json!({"offset":offset,"total":total,"items":items,"next":next})
}
fn artist() -> Value {
    json!({"id":"artist-1","name":"Singer","uri":"spotify:artist:artist-1"})
}
fn album(id: &str) -> Value {
    json!({"id":id,"name":id,"release_date":"2026-01","release_date_precision":"month","total_tracks":2,"artists":[artist()],"label":"Label","external_ids":{"upc":"123"}})
}
fn track(id: &str, album_id: &str) -> Value {
    json!({"id":id,"name":id,"album":album(album_id),"artists":[artist()],"duration_ms":123000,"disc_number":1,"track_number":1,"external_ids":{"isrc":"TEST123"}})
}
fn saved(key: &str, obj: Value) -> Value {
    let mut v = json!({"added_at":"2026-01-02T03:04:05Z"});
    v[key] = obj;
    v
}
async fn setup() -> Result<(TempDir, SqliteConnection)> {
    let dir = tempfile::tempdir()?;
    let db = database::open(&dir.path().join("db.sqlite")).await?;
    Ok((dir, db))
}
async fn count(db: &mut SqliteConnection, sql: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(sql).fetch_one(db).await?)
}
fn full_responses() -> Vec<(&'static str, i64, Value)> {
    let liked = page(0, 1, vec![saved("track", track("liked", "not-saved"))]);
    let mut a = album("saved-album");
    let mut first = track("album-track-1", "saved-album");
    first.as_object_mut().unwrap().remove("album");
    a["tracks"] = page(0, 2, vec![first]);
    let albums = page(0, 1, vec![saved("album", a)]);
    vec![
        ("/me/tracks", 0, liked.clone()),
        ("/me/tracks", 0, liked),
        ("/me/albums", 0, albums.clone()),
        ("/me/albums", 0, albums),
        (
            "/albums/saved-album/tracks",
            1,
            page(1, 2, vec![track("album-track-2", "saved-album")]),
        ),
    ]
}

#[tokio::test]
async fn initial_import_idempotence_cache_and_independent_saved_states() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = sync::prepare(&mut db, false, false).await?;
    let mut api = mock(full_responses());
    let summary = sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(summary["track_added"], 1);
    assert!(api.responses.is_empty());
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 3);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        1
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM album WHERE is_saved=1").await?,
        1
    );
    assert_eq!(
        count(&mut db, "SELECT is_saved FROM album WHERE id='not-saved'").await?,
        0
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica_artista").await?,
        3
    );
    let job = sync::prepare(&mut db, false, false).await?;
    let mut responses = full_responses();
    responses.pop();
    let mut api = mock(responses);
    let summary = sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(summary["track_added"], 0);
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 3);
    assert_eq!(api.calls, 4); // no album-track request while cache is fresh
    Ok(())
}

#[tokio::test]
async fn interrupted_job_resumes_from_committed_page_and_does_not_change_saved_state() -> Result<()>
{
    let (dir, mut db) = setup().await?;
    let first = page(0, 2, vec![saved("track", track("one", "a"))]);
    let second = page(1, 2, vec![saved("track", track("two", "a"))]);
    let mut api = mock(vec![("/me/tracks", 0, first.clone())]);
    api.fail_after = Some(1);
    let job = sync::prepare(&mut db, false, false).await?;
    let error = sync::run(&mut db, &mut api, &job, 30).await.unwrap_err();
    sync::record_error(&mut db, &job, &error).await?;
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        0
    );
    assert_eq!(count(&mut db, "SELECT next_offset FROM sync_job").await?, 1);
    db.close().await?;
    let mut db = database::open(&dir.path().join("db.sqlite")).await?;
    assert_eq!(sync::prepare(&mut db, true, false).await?, job);
    let empty = page(0, 0, vec![]);
    let mut api = mock(vec![
        ("/me/tracks", 1, second.clone()),
        ("/me/tracks", 0, first),
        ("/me/tracks", 1, second),
        ("/me/albums", 0, empty.clone()),
        ("/me/albums", 0, empty),
    ]);
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn removal_reconciles_only_after_complete_success_and_preserves_history() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = sync::prepare(&mut db, false, false).await?;
    sync::run(&mut db, &mut mock(full_responses()), &job, 30).await?;
    let next = sync::prepare(&mut db, false, false).await?;
    let empty = page(0, 0, vec![]);
    let mut api = mock(vec![
        ("/me/tracks", 0, empty.clone()),
        ("/me/tracks", 0, empty.clone()),
    ]);
    api.fail_after = Some(2);
    assert!(sync::run(&mut db, &mut api, &next, 30).await.is_err());
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        1
    );
    let mut api = mock(vec![
        ("/me/albums", 0, empty.clone()),
        ("/me/albums", 0, empty),
    ]);
    let summary = sync::run(&mut db, &mut api, &next, 30).await?;
    assert_eq!(summary["track_removed"], 1);
    assert_eq!(summary["album_removed"], 1);
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 3);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        0
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT COUNT(*) FROM musica WHERE saved_at IS NOT NULL"
        )
        .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn mutation_with_same_total_is_detected_and_restarts_collection() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = sync::prepare(&mut db, false, false).await?;
    let a = page(0, 1, vec![saved("track", track("old", "a"))]);
    let b = page(0, 1, vec![saved("track", track("new", "a"))]);
    let error = sync::run(
        &mut db,
        &mut mock(vec![("/me/tracks", 0, a), ("/me/tracks", 0, b)]),
        &job,
        30,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("mudou"));
    assert_eq!(count(&mut db, "SELECT next_offset FROM sync_job").await?, 0);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM sync_seen_item").await?,
        0
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn partial_objects_and_nulls_preserve_full_metadata() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    database::album(&mut db, &album("a")).await?;
    database::track(&mut db, &track("t", "a"), "a").await?;
    database::album(&mut db, &json!({"id":"a","name":"New","label":null})).await?;
    database::track(
        &mut db,
        &json!({"id":"t","name":"New Track","external_ids":null}),
        "a",
    )
    .await?;
    let r = sqlx::query("SELECT label,upc,name FROM album WHERE id='a'")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(r.get::<String, _>("label"), "Label");
    assert_eq!(r.get::<String, _>("upc"), "123");
    assert_eq!(r.get::<String, _>("name"), "New");
    let isrc: String = sqlx::query_scalar("SELECT isrc FROM musica WHERE id='t'")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(isrc, "TEST123");
    Ok(())
}

#[tokio::test]
async fn rollback_keeps_page_and_checkpoint_atomic() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = sync::prepare(&mut db, false, false).await?;
    let p = page(
        0,
        2,
        vec![
            saved("track", track("good", "a")),
            saved("track", json!({"name":"missing id"})),
        ],
    );
    assert!(
        sync::run(&mut db, &mut mock(vec![("/me/tracks", 0, p)]), &job, 30)
            .await
            .is_err()
    );
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 0);
    assert_eq!(count(&mut db, "SELECT next_offset FROM sync_job").await?, 0);
    Ok(())
}

#[tokio::test]
async fn account_isolation_and_rate_limit_deadline_are_persistent() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    database::bind_account(&mut db, "first").await?;
    database::bind_account(&mut db, "first").await?;
    assert!(database::bind_account(&mut db, "other").await.is_err());
    let job = sync::prepare(&mut db, false, false).await?;
    let error = anyhow::Error::new(Paused {
        message: "QUOTA_EXCEEDED".into(),
        retry_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
    });
    sync::record_error(&mut db, &job, &error).await?;
    assert!(sync::prepare(&mut db, true, false).await.is_err());
    let status: String = sqlx::query_scalar("SELECT status FROM sync_job")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(status, "rate_limited");
    Ok(())
}

#[tokio::test]
async fn full_sync_refreshes_cached_album_tracks() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = sync::prepare(&mut db, false, false).await?;
    sync::run(&mut db, &mut mock(full_responses()), &job, 30).await?;
    let job = sync::prepare(&mut db, false, true).await?;
    let mut api = mock(full_responses());
    sync::run(&mut db, &mut api, &job, 30).await?;
    assert_eq!(api.calls, 5);
    Ok(())
}

#[tokio::test]
async fn duplicate_items_block_removals_and_cannot_loop_forever() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = sync::prepare(&mut db, false, false).await?;
    let p = page(
        0,
        2,
        vec![
            saved("track", track("same", "a")),
            saved("track", track("same", "a")),
        ],
    );
    assert!(
        sync::run(&mut db, &mut mock(vec![("/me/tracks", 0, p)]), &job, 30)
            .await
            .is_err()
    );
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM musica").await?, 0);
    Ok(())
}

#[tokio::test]
async fn migration_foreign_keys_and_finalization_guard() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    assert_eq!(count(&mut db, "PRAGMA foreign_keys").await?, 1);
    assert!(
        sqlx::query("INSERT INTO album_musica VALUES('missing','missing',1,1)")
            .execute(&mut db)
            .await
            .is_err()
    );
    let job = sync::prepare(&mut db, false, false).await?;
    assert!(database::finish(&mut db, &job).await.is_err());
    let tx = db.begin().await?;
    drop(tx);
    Ok(())
}

#[tokio::test]
async fn exact_original_json_is_preserved_separately_from_merged_metadata() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let full = album("a");
    let partial = json!({"id":"a","name":"New","label":null});
    database::album(&mut db, &full).await?;
    database::album(&mut db, &partial).await?;
    database::album(&mut db, &partial).await?;
    let originals: Vec<String> = sqlx::query_scalar(
        "SELECT raw_json FROM catalog_observation WHERE object_type='album' AND object_id='a'",
    )
    .fetch_all(&mut db)
    .await?;
    assert_eq!(originals.len(), 2);
    assert!(originals
        .iter()
        .any(|s| serde_json::from_str::<Value>(s).unwrap() == full));
    assert!(originals
        .iter()
        .any(|s| serde_json::from_str::<Value>(s).unwrap() == partial));
    Ok(())
}

#[tokio::test]
async fn expired_cache_and_changed_album_count_trigger_fetches() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = sync::prepare(&mut db, false, false).await?;
    sync::run(&mut db, &mut mock(full_responses()), &job, 30).await?;
    sqlx::query("UPDATE album SET tracks_fetched_at='2000-01-01T00:00:00Z'")
        .execute(&mut db)
        .await?;
    let job = sync::prepare(&mut db, false, false).await?;
    sync::run(&mut db, &mut mock(full_responses()), &job, 30).await?;
    let job = sync::prepare(&mut db, false, false).await?;
    let mut responses = full_responses();
    for (_, _, value) in &mut responses {
        if let Some(a) = value.pointer_mut("/items/0/album") {
            a["total_tracks"] = json!(3);
            a["tracks"]["total"] = json!(3);
        }
    }
    responses[4].2 = page(
        1,
        3,
        vec![
            track("album-track-2", "saved-album"),
            track("album-track-3", "saved-album"),
        ],
    );
    sync::run(&mut db, &mut mock(responses), &job, 30).await?;
    assert_eq!(
        count(
            &mut db,
            "SELECT tracks_imported_total FROM album WHERE id='saved-album'"
        )
        .await?,
        3
    );
    Ok(())
}

#[tokio::test]
async fn multiple_album_links_preserve_principal_track_position_and_nested_metadata() -> Result<()>
{
    let (_dir, mut db) = setup().await?;
    database::album(&mut db, &album("first")).await?;
    database::album(&mut db, &album("other")).await?;
    let mut rich = track("same", "first");
    rich["artists"][0]["genres"] = json!(["rock"]);
    database::track(&mut db, &rich, "first").await?;
    let mut simple = track("same", "other");
    simple["track_number"] = json!(9);
    database::track(&mut db, &simple, "other").await?;
    let row = sqlx::query("SELECT album_id,track_number,raw_json FROM musica WHERE id='same'")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(row.get::<String, _>("album_id"), "first");
    assert_eq!(row.get::<i64, _>("track_number"), 1);
    let raw: Value = serde_json::from_str(row.get("raw_json"))?;
    assert_eq!(raw["artists"][0]["genres"], json!(["rock"]));
    assert_eq!(
        count(
            &mut db,
            "SELECT track_number FROM album_musica WHERE album_id='other'"
        )
        .await?,
        9
    );
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM album_musica").await?,
        2
    );
    Ok(())
}
