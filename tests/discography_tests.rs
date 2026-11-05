use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use spotify_library_sync::{
    database,
    discography::{self, Options},
    spotify::{Api, Paused},
    sync,
};
use sqlx::{Connection, SqliteConnection};
use std::collections::{HashMap, HashSet};
use tempfile::TempDir;

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Profile,
    Follow(Option<String>),
    Albums(String, i64, String, Option<String>),
    Tracks(String, i64, Option<String>),
}
struct Mock {
    followed: Vec<String>,
    releases: HashMap<String, Vec<Value>>,
    tracks: HashMap<String, Vec<Value>>,
    calls: Vec<Call>,
    fail_at: Option<usize>,
    quota_at: Option<usize>,
    malformed_follow: bool,
    release_pages: HashMap<(String, i64), Value>,
}
impl Mock {
    fn new(ids: &[&str]) -> Self {
        Self {
            followed: ids.iter().map(|s| s.to_string()).collect(),
            releases: HashMap::new(),
            tracks: HashMap::new(),
            calls: vec![],
            fail_at: None,
            quota_at: None,
            malformed_follow: false,
            release_pages: HashMap::new(),
        }
    }
    fn hit(&mut self, call: Call) -> Result<()> {
        let at = self.calls.len();
        self.calls.push(call);
        if self.quota_at == Some(at) {
            return Err(Paused {
                message: "QUOTA_EXCEEDED".into(),
                retry_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
            }
            .into());
        }
        if self.fail_at == Some(at) {
            bail!("simulated network failure");
        }
        Ok(())
    }
}
fn artist(id: &str) -> Value {
    json!({"id":id,"name":id,"uri":format!("spotify:artist:{id}"),"external_urls":{"spotify":format!("https://open.spotify.com/artist/{id}")}})
}
fn album(id: &str, artists: &[&str], total: i64) -> Value {
    json!({"id":id,"name":id,"total_tracks":total,"release_date":"2026-01","release_date_precision":"month","album_type":"album","artists":artists.iter().map(|a|artist(a)).collect::<Vec<_>>()})
}
fn track(id: &str, artists: &[&str]) -> Value {
    json!({"id":id,"name":id,"artists":artists.iter().map(|a|artist(a)).collect::<Vec<_>>(),"duration_ms":123000,"disc_number":1,"track_number":1})
}
fn page(items: &[Value], offset: i64, size: usize) -> Value {
    let start = (offset as usize).min(items.len());
    let end = (start + size).min(items.len());
    json!({"offset":offset,"total":items.len(),"items":items[start..end],"next":if end<items.len(){Some("ignored")}else{None}})
}
#[async_trait]
impl Api for Mock {
    async fn get(&mut self, path: &str, _offset: Option<i64>) -> Result<Value> {
        assert_eq!(path, "/me");
        self.hit(Call::Profile)?;
        Ok(json!({"id":"account"}))
    }
    async fn followed_artists(&mut self, after: Option<&str>) -> Result<Value> {
        self.hit(Call::Follow(after.map(str::to_owned)))?;
        let start = after
            .and_then(|id| self.followed.iter().position(|v| v == id))
            .map_or(0, |i| i + 1);
        let end = (start + 50).min(self.followed.len());
        let next = if self.malformed_follow {
            true
        } else {
            end < self.followed.len()
        };
        Ok(
            json!({"artists":{"items":self.followed[start..end].iter().map(|a|artist(a)).collect::<Vec<_>>(),"total":self.followed.len(),"next":if next{Some("ignored")}else{None},"cursors":{"after":if self.malformed_follow{after.map(str::to_owned)}else if next{Some(self.followed[end-1].clone())}else{None}}}}),
        )
    }
    async fn artist_albums(
        &mut self,
        id: &str,
        offset: i64,
        groups: &str,
        market: Option<&str>,
    ) -> Result<Value> {
        self.hit(Call::Albums(
            id.into(),
            offset,
            groups.into(),
            market.map(str::to_owned),
        ))?;
        if let Some(page) = self.release_pages.get(&(id.to_owned(), offset)) {
            return Ok(page.clone());
        }
        let mut response = page(
            self.releases.get(id).map(Vec::as_slice).unwrap_or(&[]),
            offset,
            10,
        );
        if !response["next"].is_null() {
            response["next"] = json!(format!(
                "https://api.spotify.com/v1/artists/{id}/albums?offset={}&limit=10",
                offset + 10
            ));
        }
        Ok(response)
    }
    async fn album_tracks(&mut self, id: &str, offset: i64, market: Option<&str>) -> Result<Value> {
        self.hit(Call::Tracks(id.into(), offset, market.map(str::to_owned)))?;
        let data = self
            .tracks
            .get(id)
            .unwrap_or_else(|| panic!("Unexpected album request: {id}"));
        Ok(page(data, offset, 50))
    }
    fn attempt_count(&self) -> u64 {
        self.calls.len() as u64
    }
}
async fn setup() -> Result<(TempDir, SqliteConnection)> {
    let dir = tempfile::tempdir()?;
    let db = database::open(&dir.path().join("db.sqlite")).await?;
    Ok((dir, db))
}
async fn n(db: &mut SqliteConnection, sql: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(sql).fetch_one(db).await?)
}
async fn execute(db: &mut SqliteConnection, api: &mut Mock, options: Options) -> Result<Value> {
    let job = discography::prepare(db, false, "sync", options).await?;
    discography::run(db, api, &job).await
}

#[tokio::test]
async fn followed_artists_only_and_shared_albums_are_imported_once_without_changing_saved_states(
) -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A", "B"]);
    let shared = album("Shared", &["A", "Guest"], 2);
    api.releases.insert("A".into(), vec![shared.clone()]);
    api.releases.insert("B".into(), vec![shared]);
    api.tracks.insert(
        "Shared".into(),
        vec![track("One", &["A", "Guest"]), track("Two", &["B"])],
    );
    let result = execute(&mut db, &mut api, Options::default()).await?;
    assert_eq!(result["followed_artists"], 2);
    assert_eq!(result["unique_albums"], 1);
    assert_eq!(
        api.calls
            .iter()
            .filter(|c| matches!(c, Call::Tracks(..)))
            .count(),
        1
    );
    assert!(!api
        .calls
        .iter()
        .any(|c| matches!(c,Call::Albums(a,..) if a=="Guest")));
    assert_eq!(
        n(&mut db, "SELECT COUNT(*) FROM artista_lancamento").await?,
        2
    );
    assert_eq!(
        n(&mut db, "SELECT COUNT(*) FROM album WHERE is_saved=1").await?,
        0
    );
    assert_eq!(
        n(&mut db, "SELECT COUNT(*) FROM musica WHERE is_saved=1").await?,
        0
    );
    assert_eq!(
        n(
            &mut db,
            "SELECT COUNT(*) FROM library_sync_state WHERE initial_import_completed_at IS NOT NULL"
        )
        .await?,
        0
    );
    assert_eq!(n(&mut db, "SELECT COUNT(*) FROM sync_job").await?, 0);
    Ok(())
}

#[tokio::test]
async fn complete_library_album_is_reused_and_liked_track_remains_liked() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let a = album("Cached", &["A"], 1);
    database::album(&mut db, &a).await?;
    database::track(&mut db, &track("Liked", &["A"]), "Cached").await?;
    sqlx::query(
        "UPDATE album SET is_saved=1,tracks_fetched_at=?,tracks_imported_total=1 WHERE id='Cached'",
    )
    .bind(database::now())
    .execute(&mut db)
    .await?;
    sqlx::query("UPDATE musica SET is_saved=1 WHERE id='Liked'")
        .execute(&mut db)
        .await?;
    let mut api = Mock::new(&["A"]);
    api.releases.insert("A".into(), vec![a]);
    execute(&mut db, &mut api, Options::default()).await?;
    assert!(!api.calls.iter().any(|c| matches!(c, Call::Tracks(..))));
    assert_eq!(
        n(&mut db, "SELECT is_saved FROM album WHERE id='Cached'").await?,
        1
    );
    assert_eq!(
        n(&mut db, "SELECT is_saved FROM musica WHERE id='Liked'").await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn full_artist_listing_uses_ten_item_pages_and_cache_prevents_repeat_scans() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases.insert(
        "A".into(),
        (0..25)
            .map(|i| album(&format!("R{i:03}"), &["A"], 0))
            .collect(),
    );
    for i in 0..25 {
        api.tracks.insert(format!("R{i:03}"), vec![]);
    }
    execute(&mut db, &mut api, Options::default()).await?;
    let pages: Vec<_> = api
        .calls
        .iter()
        .filter_map(|c| {
            if let Call::Albums(_, o, ..) = c {
                Some(*o)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(pages, vec![0, 10, 20]);
    api.calls.clear();
    execute(&mut db, &mut api, Options::default()).await?;
    assert_eq!(api.calls, vec![Call::Profile, Call::Follow(None)]);
    Ok(())
}

#[tokio::test]
async fn newly_released_album_is_detected_by_targeted_incremental_update() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases.insert(
        "A".into(),
        (0..35)
            .map(|i| album(&format!("Old{i:03}"), &["A"], 0))
            .collect(),
    );
    for i in 0..35 {
        api.tracks.insert(format!("Old{i:03}"), vec![]);
    }
    execute(&mut db, &mut api, Options::default()).await?;
    api.calls.clear();
    api.releases
        .get_mut("A")
        .unwrap()
        .insert(0, album("NewAlbum", &["A"], 1));
    api.tracks
        .insert("NewAlbum".into(), vec![track("NewTrack", &["A"])]);
    execute(
        &mut db,
        &mut api,
        Options {
            artists: vec!["A".into()],
            ..Options::default()
        },
    )
    .await?;
    let pages: Vec<_> = api
        .calls
        .iter()
        .filter_map(|c| {
            if let Call::Albums(_, o, ..) = c {
                Some(*o)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(pages, vec![0, 10, 20]);
    assert_eq!(
        api.calls
            .iter()
            .filter(|c| matches!(c, Call::Tracks(..)))
            .count(),
        1
    );
    let coverage: String = sqlx::query_scalar("SELECT coverage FROM discografia_cache")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(coverage, "recent_window");
    assert_eq!(
        n(
            &mut db,
            "SELECT COUNT(*) FROM musica WHERE id='NewTrack' AND is_saved=0"
        )
        .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn budget_pauses_and_resume_survives_database_reopen_and_reaches_next_pages() -> Result<()> {
    let (dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases.insert(
        "A".into(),
        (0..45)
            .map(|i| album(&format!("R{i:03}"), &["A"], 0))
            .collect(),
    );
    for i in 0..45 {
        api.tracks.insert(format!("R{i:03}"), vec![]);
    }
    let job = discography::prepare(
        &mut db,
        false,
        "sync",
        Options {
            request_budget: Some(5),
            ..Options::default()
        },
    )
    .await?;
    let e = discography::run(&mut db, &mut api, &job).await.unwrap_err();
    assert_eq!(
        e.downcast_ref::<sync::WorkPaused>().unwrap().reason,
        "request_budget_exhausted"
    );
    discography::record_error(&mut db, &job, &e).await?;
    assert_eq!(
        n(&mut db, "SELECT next_offset FROM discografia_artista_queue").await?,
        30
    );
    assert_eq!(api.calls.len(), 5);
    db.close().await?;
    let mut db = database::open(&dir.path().join("db.sqlite")).await?;
    assert_eq!(
        discography::prepare(
            &mut db,
            true,
            "sync",
            Options {
                request_budget: Some(200),
                ..Options::default()
            }
        )
        .await?,
        job
    );
    api.calls.clear();
    discography::run(&mut db, &mut api, &job).await?;
    let pages: Vec<_> = api
        .calls
        .iter()
        .filter_map(|c| {
            if let Call::Albums(_, o, ..) = c {
                Some(*o)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(pages, vec![20, 30, 40]);
    assert!(!api.calls.iter().any(|c| matches!(c, Call::Follow(_))));
    Ok(())
}

#[tokio::test]
async fn followed_cursor_is_committed_and_unfollow_is_not_applied_after_partial_failure(
) -> Result<()> {
    let (_dir, mut db) = setup().await?;
    database::artist(&mut db, &artist("Existing")).await?;
    sqlx::query("INSERT INTO artista_seguido VALUES('Existing',1,NULL,?)")
        .bind(database::now())
        .execute(&mut db)
        .await?;
    let mut api = Mock::new(&[]);
    api.followed = (0..120).map(|i| format!("A{i:03}")).collect();
    api.fail_at = Some(2);
    let job = discography::prepare(&mut db, false, "sync", Options::default()).await?;
    let e = discography::run(&mut db, &mut api, &job).await.unwrap_err();
    discography::record_error(&mut db, &job, &e).await?;
    assert_eq!(
        n(
            &mut db,
            "SELECT is_followed FROM artista_seguido WHERE artista_id='Existing'"
        )
        .await?,
        1
    );
    let cursor: String = sqlx::query_scalar("SELECT following_cursor FROM discografia_job")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(cursor, "A049");
    api.fail_at = None;
    api.calls.clear();
    discography::prepare(&mut db, true, "sync", Options::default()).await?;
    discography::run(&mut db, &mut api, &job).await?;
    assert_eq!(api.calls[1], Call::Follow(Some("A049".into())));
    assert_eq!(
        n(
            &mut db,
            "SELECT is_followed FROM artista_seguido WHERE artista_id='Existing'"
        )
        .await?,
        0
    );
    assert_eq!(
        n(
            &mut db,
            "SELECT COUNT(*) FROM artista_seguido WHERE is_followed=1"
        )
        .await?,
        120
    );
    Ok(())
}

#[tokio::test]
async fn catalog_only_defers_tracks_and_track_command_uses_shared_pending_queue() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases
        .insert("A".into(), vec![album("Record", &["A"], 2)]);
    api.tracks.insert(
        "Record".into(),
        vec![track("One", &["A"]), track("Two", &["A"])],
    );
    execute(
        &mut db,
        &mut api,
        Options {
            catalog_only: true,
            ..Options::default()
        },
    )
    .await?;
    assert!(!api.calls.iter().any(|c| matches!(c, Call::Tracks(..))));
    assert_eq!(
        n(
            &mut db,
            "SELECT COUNT(*) FROM discografia_album_queue WHERE state='pending'"
        )
        .await?,
        1
    );
    api.calls.clear();
    let job = discography::prepare(&mut db, false, "tracks", Options::default()).await?;
    discography::run(&mut db, &mut api, &job).await?;
    assert_eq!(api.calls.len(), 2);
    assert_eq!(api.calls[0], Call::Profile);
    assert!(matches!(&api.calls[1],Call::Tracks(id,0,None) if id=="Record"));
    Ok(())
}

#[tokio::test]
async fn track_queue_resumes_from_its_page_without_repeating_artist_listing() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases
        .insert("A".into(), vec![album("Long", &["A"], 120)]);
    api.tracks.insert(
        "Long".into(),
        (0..120)
            .map(|i| track(&format!("T{i:03}"), &["A"]))
            .collect(),
    );
    let job = discography::prepare(
        &mut db,
        false,
        "sync",
        Options {
            request_budget: Some(5),
            ..Options::default()
        },
    )
    .await?;
    let e = discography::run(&mut db, &mut api, &job).await.unwrap_err();
    discography::record_error(&mut db, &job, &e).await?;
    assert_eq!(
        n(&mut db, "SELECT next_offset FROM discografia_album_queue").await?,
        100
    );
    api.calls.clear();
    discography::prepare(&mut db, true, "sync", Options::default()).await?;
    discography::run(&mut db, &mut api, &job).await?;
    assert_eq!(
        api.calls,
        vec![Call::Profile, Call::Tracks("Long".into(), 100, None)]
    );
    assert_eq!(
        n(
            &mut db,
            "SELECT tracks_imported_total FROM album WHERE id='Long'"
        )
        .await?,
        120
    );
    Ok(())
}

#[tokio::test]
async fn catalog_context_separates_market_and_filter_cache() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases
        .insert("A".into(), vec![album("Release", &["A"], 0)]);
    api.tracks.insert("Release".into(), vec![]);
    execute(&mut db, &mut api, Options::default()).await?;
    api.calls.clear();
    execute(
        &mut db,
        &mut api,
        Options {
            market: Some("br".into()),
            groups: vec!["single".into()],
            ..Options::default()
        },
    )
    .await?;
    assert!(api
        .calls
        .iter()
        .any(|c| matches!(c,Call::Albums(_,0,g,Some(m)) if g=="single" && m=="BR")));
    assert!(api
        .calls
        .iter()
        .any(|c| matches!(c,Call::Tracks(_,0,Some(m)) if m=="BR")));
    assert_eq!(
        n(&mut db, "SELECT COUNT(*) FROM discografia_cache").await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn unfollowed_artist_retains_catalog_and_is_not_automatically_refreshed() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A", "B"]);
    api.releases
        .insert("A".into(), vec![album("Keep", &["A"], 0)]);
    api.tracks.insert("Keep".into(), vec![]);
    execute(&mut db, &mut api, Options::default()).await?;
    api.calls.clear();
    api.followed = vec!["B".into()];
    execute(
        &mut db,
        &mut api,
        Options {
            full: true,
            ..Options::default()
        },
    )
    .await?;
    assert!(!api
        .calls
        .iter()
        .any(|c| matches!(c,Call::Albums(a,..) if a=="A")));
    assert_eq!(
        n(&mut db, "SELECT COUNT(*) FROM album WHERE id='Keep'").await?,
        1
    );
    assert_eq!(
        n(
            &mut db,
            "SELECT is_followed FROM artista_seguido WHERE artista_id='A'"
        )
        .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn malformed_followed_response_does_not_clear_previous_followed_states() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    execute(&mut db, &mut api, Options::default()).await?;
    api.malformed_follow = true;
    let job = discography::prepare(&mut db, false, "sync", Options::default()).await?;
    assert!(discography::run(&mut db, &mut api, &job).await.is_err());
    assert_eq!(
        n(
            &mut db,
            "SELECT is_followed FROM artista_seguido WHERE artista_id='A'"
        )
        .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn quota_cooldown_is_shared_with_library_sync_and_survives_restart() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.quota_at = Some(1);
    let job = discography::prepare(&mut db, false, "sync", Options::default()).await?;
    let error = discography::run(&mut db, &mut api, &job).await.unwrap_err();
    discography::record_error(&mut db, &job, &error).await?;
    assert!(sync::check_cooldown(&mut db).await.is_err());
    assert!(discography::restart(&mut db).await.is_err());
    assert!(
        sync::prepare_sync(&mut db, false, &sync::Options::default())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn thousand_followed_artists_are_paginated_and_rotated_in_batches_of_one_hundred(
) -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&[]);
    api.followed = (0..1000).map(|i| format!("A{i:04}")).collect();
    execute(&mut db, &mut api, Options::default()).await?;
    assert_eq!(
        api.calls
            .iter()
            .filter(|c| matches!(c, Call::Follow(_)))
            .count(),
        20
    );
    assert_eq!(
        api.calls
            .iter()
            .filter(|c| matches!(c, Call::Albums(..)))
            .count(),
        100
    );
    let old: HashSet<_> = api
        .calls
        .iter()
        .filter_map(|c| {
            if let Call::Albums(id, ..) = c {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();
    api.calls.clear();
    execute(&mut db, &mut api, Options::default()).await?;
    assert_eq!(
        api.calls
            .iter()
            .filter(|c| matches!(c, Call::Albums(..)))
            .count(),
        100
    );
    assert!(api
        .calls
        .iter()
        .filter_map(|c| if let Call::Albums(id, ..) = c {
            Some(id)
        } else {
            None
        })
        .all(|id| !old.contains(id)));
    assert_eq!(
        n(&mut db, "SELECT COUNT(*) FROM discografia_cache").await?,
        200
    );
    Ok(())
}

#[tokio::test]
async fn explicit_full_revalidates_all_pages_while_reusing_completed_album_tracks() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases.insert(
        "A".into(),
        (0..35)
            .map(|i| album(&format!("R{i:03}"), &["A"], 0))
            .collect(),
    );
    for i in 0..35 {
        api.tracks.insert(format!("R{i:03}"), vec![]);
    }
    execute(&mut db, &mut api, Options::default()).await?;
    api.calls.clear();
    execute(
        &mut db,
        &mut api,
        Options {
            full: true,
            ..Options::default()
        },
    )
    .await?;
    assert_eq!(
        api.calls
            .iter()
            .filter(|c| matches!(c, Call::Albums(..)))
            .count(),
        4
    );
    assert!(!api.calls.iter().any(|c| matches!(c, Call::Tracks(..))));
    Ok(())
}

#[tokio::test]
async fn targeted_artist_must_be_followed_and_account_isolation_is_preserved() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    let e = execute(
        &mut db,
        &mut api,
        Options {
            artists: vec!["NotFollowed".into()],
            ..Options::default()
        },
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("não está na lista"));
    assert!(!api.calls.iter().any(|c| matches!(c, Call::Albums(..))));
    discography::restart(&mut db).await?;
    sqlx::query("UPDATE account SET id='different'")
        .execute(&mut db)
        .await?;
    let job = discography::prepare(&mut db, false, "sync", Options::default()).await?;
    assert!(discography::run(&mut db, &mut api, &job).await.is_err());
    Ok(())
}

#[tokio::test]
async fn changed_track_count_invalidates_only_that_album_cache() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases
        .insert("A".into(), vec![album("Record", &["A"], 1)]);
    api.tracks
        .insert("Record".into(), vec![track("Old", &["A"])]);
    execute(&mut db, &mut api, Options::default()).await?;
    api.calls.clear();
    api.releases.get_mut("A").unwrap()[0]["total_tracks"] = json!(2);
    api.tracks
        .get_mut("Record")
        .unwrap()
        .push(track("Added", &["A"]));
    execute(
        &mut db,
        &mut api,
        Options {
            artists: vec!["A".into()],
            ..Options::default()
        },
    )
    .await?;
    assert_eq!(
        api.calls
            .iter()
            .filter(|c| matches!(c, Call::Tracks(..)))
            .count(),
        1
    );
    assert_eq!(
        n(
            &mut db,
            "SELECT tracks_imported_total FROM album WHERE id='Record'"
        )
        .await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn restart_of_incomplete_recent_update_forces_safe_full_listing_of_affected_artist(
) -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases.insert(
        "A".into(),
        (0..35)
            .map(|i| album(&format!("Old{i:03}"), &["A"], 0))
            .collect(),
    );
    for i in 0..35 {
        api.tracks.insert(format!("Old{i:03}"), vec![]);
    }
    execute(&mut db, &mut api, Options::default()).await?;
    let new: Vec<_> = (0..40)
        .map(|i| album(&format!("New{i:03}"), &["A"], 0))
        .collect();
    for i in 0..40 {
        api.tracks.insert(format!("New{i:03}"), vec![]);
    }
    api.releases.get_mut("A").unwrap().splice(0..0, new);
    let job = discography::prepare(
        &mut db,
        false,
        "sync",
        Options {
            request_budget: Some(5),
            artists: vec!["A".into()],
            ..Options::default()
        },
    )
    .await?;
    let e = discography::run(&mut db, &mut api, &job).await.unwrap_err();
    discography::record_error(&mut db, &job, &e).await?;
    discography::restart(&mut db).await?;
    assert_eq!(
        n(&mut db, "SELECT needs_full_refresh FROM discografia_cache").await?,
        1
    );
    api.calls.clear();
    execute(&mut db, &mut api, Options::default()).await?;
    assert_eq!(
        api.calls
            .iter()
            .filter(|c| matches!(c, Call::Albums(..)))
            .count(),
        8
    );
    assert_eq!(
        n(&mut db, "SELECT needs_full_refresh FROM discografia_cache").await?,
        0
    );
    assert_eq!(
        n(&mut db, "SELECT COUNT(*) FROM artista_lancamento").await?,
        75
    );
    Ok(())
}

#[tokio::test]
async fn status_does_not_call_an_artist_complete_when_album_tracks_are_pending() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.releases
        .insert("A".into(), vec![album("Pending", &["A"], 1)]);
    api.tracks
        .insert("Pending".into(), vec![track("Song", &["A"])]);
    execute(
        &mut db,
        &mut api,
        Options {
            catalog_only: true,
            ..Options::default()
        },
    )
    .await?;
    let artists = discography::artists(&mut db, 100, 0).await?;
    assert_eq!(artists[0]["known_catalog_tracks_complete"], false);
    assert_eq!(artists[0]["pending_albums"], 1);
    let state = discography::status(&mut db).await?;
    assert_eq!(state["pending_albums_active_context"], 1);
    let job = discography::prepare(&mut db, false, "tracks", Options::default()).await?;
    discography::run(&mut db, &mut api, &job).await?;
    assert_eq!(
        discography::artists(&mut db, 100, 0).await?[0]["known_catalog_tracks_complete"],
        true
    );
    Ok(())
}

#[tokio::test]
async fn catalog_change_on_resume_repositions_only_current_artist_and_keeps_others_completed(
) -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A", "B"]);
    api.releases.insert(
        "B".into(),
        (0..35)
            .map(|i| album(&format!("R{i:03}"), &["B"], 0))
            .collect(),
    );
    for i in 0..35 {
        api.tracks.insert(format!("R{i:03}"), vec![]);
    }
    let job = discography::prepare(
        &mut db,
        false,
        "sync",
        Options {
            request_budget: Some(5),
            ..Options::default()
        },
    )
    .await?;
    let e = discography::run(&mut db, &mut api, &job).await.unwrap_err();
    discography::record_error(&mut db, &job, &e).await?;
    assert_eq!(n(&mut db,"SELECT COUNT(*) FROM discografia_artista_queue WHERE artista_id='A' AND state='completed'").await?,1);
    api.releases
        .get_mut("B")
        .unwrap()
        .insert(0, album("New", &["B"], 0));
    api.tracks.insert("New".into(), vec![]);
    api.calls.clear();
    discography::prepare(&mut db, true, "sync", Options::default()).await?;
    let e = discography::run(&mut db, &mut api, &job).await.unwrap_err();
    assert_eq!(
        e.downcast_ref::<sync::WorkPaused>().unwrap().reason,
        "artist_catalog_changed"
    );
    assert_eq!(
        n(
            &mut db,
            "SELECT next_offset FROM discografia_artista_queue WHERE artista_id='B'"
        )
        .await?,
        0
    );
    assert!(!api
        .calls
        .iter()
        .any(|c| matches!(c,Call::Albums(a,..) if a=="A")));
    Ok(())
}

fn release_page(offset: i64, total: i64, count: usize, next: Option<i64>) -> Value {
    json!({"offset":offset,"total":total,"items":(0..count).map(|i|album(&format!("R{offset}N{i}"), &["A"],0)).collect::<Vec<_>>(),"next":next.map(|n|format!("https://api.spotify.com/v1/artists/A/albums?offset={n}&limit=10"))})
}

#[tokio::test]
async fn sparse_release_pages_follow_server_offsets_including_empty_and_short_terminal_pages(
) -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.release_pages
        .insert(("A".into(), 0), release_page(0, 30, 7, Some(10)));
    api.release_pages
        .insert(("A".into(), 10), release_page(10, 30, 0, Some(20)));
    api.release_pages
        .insert(("A".into(), 20), release_page(20, 30, 3, None));
    execute(
        &mut db,
        &mut api,
        Options {
            catalog_only: true,
            ..Options::default()
        },
    )
    .await?;
    let offsets: Vec<_> = api
        .calls
        .iter()
        .filter_map(|c| {
            if let Call::Albums(_, offset, ..) = c {
                Some(*offset)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(offsets, vec![0, 10, 20]);
    assert_eq!(
        n(&mut db, "SELECT COUNT(*) FROM artista_lancamento").await?,
        10
    );
    assert_eq!(
        n(
            &mut db,
            "SELECT COUNT(*) FROM discografia_artista_queue WHERE state='completed'"
        )
        .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn sparse_release_resume_repairs_old_item_count_cursor() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut api = Mock::new(&["A"]);
    api.release_pages
        .insert(("A".into(), 0), release_page(0, 20, 7, Some(10)));
    api.release_pages
        .insert(("A".into(), 10), release_page(10, 20, 5, None));
    api.fail_at = Some(3);
    let job = discography::prepare(
        &mut db,
        false,
        "sync",
        Options {
            catalog_only: true,
            ..Options::default()
        },
    )
    .await?;
    let error = discography::run(&mut db, &mut api, &job).await.unwrap_err();
    discography::record_error(&mut db, &job, &error).await?;
    sqlx::query("UPDATE discografia_artista_queue SET next_offset=7 WHERE artista_id='A'")
        .execute(&mut db)
        .await?;
    api.fail_at = None;
    api.calls.clear();
    discography::prepare(&mut db, true, "sync", Options::default()).await?;
    discography::run(&mut db, &mut api, &job).await?;
    let offsets: Vec<_> = api
        .calls
        .iter()
        .filter_map(|c| {
            if let Call::Albums(_, offset, ..) = c {
                Some(*offset)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(offsets, vec![0, 10]);
    assert_eq!(
        n(&mut db, "SELECT COUNT(*) FROM artista_lancamento").await?,
        12
    );
    Ok(())
}

#[tokio::test]
async fn invalid_release_cursors_and_premature_end_never_complete_artist() -> Result<()> {
    for invalid in [Some(0), Some(5), Some(20), None] {
        let (_dir, mut db) = setup().await?;
        let mut api = Mock::new(&["A"]);
        api.release_pages
            .insert(("A".into(), 0), release_page(0, 30, 7, invalid));
        let error = execute(
            &mut db,
            &mut api,
            Options {
                catalog_only: true,
                ..Options::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<sync::WorkPaused>().unwrap().reason,
            "artist_catalog_changed"
        );
        assert!(error.to_string().contains("Offset solicitado: 0"));
        assert_eq!(
            n(
                &mut db,
                "SELECT COUNT(*) FROM discografia_artista_queue WHERE state='completed'"
            )
            .await?,
            0
        );
    }
    Ok(())
}
