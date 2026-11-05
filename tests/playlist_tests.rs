use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use spotify_library_sync::{
    database,
    playlist::{self, DateBasis, Kind, Options, PlaylistApi},
    spotify::{Api, Paused, WriteRejected},
};
use sqlx::{Connection, SqliteConnection};

fn options(kind: Kind) -> Options {
    Options {
        kind,
        from: None,
        to: None,
        date_basis: DateBasis::Added,
        name: None,
        public: false,
    }
}
async fn setup() -> Result<(tempfile::TempDir, SqliteConnection)> {
    let dir = tempfile::tempdir()?;
    let mut db = database::open(&dir.path().join("db.sqlite")).await?;
    database::bind_account(&mut db, "owner").await?;
    Ok((dir, db))
}
async fn album(
    db: &mut SqliteConnection,
    id: &str,
    saved: bool,
    date: &str,
    release: &str,
) -> Result<()> {
    database::album(
        db,
        &json!({"id":id,"name":id,"release_date":release,"total_tracks":1}),
    )
    .await?;
    sqlx::query("UPDATE album SET is_saved=?,saved_at=?,tracks_fetched_at='2026-01-01',tracks_imported_total=1 WHERE id=?").bind(saved).bind(date).bind(id).execute(db).await?;
    Ok(())
}
async fn track(
    db: &mut SqliteConnection,
    id: &str,
    album: &str,
    liked: bool,
    date: &str,
) -> Result<()> {
    database::track(
        db,
        &json!({"id":id,"uri":format!("spotify:track:{id}"),"name":id,"is_local":false}),
        album,
    )
    .await?;
    sqlx::query("UPDATE musica SET is_saved=?,saved_at=? WHERE id=?")
        .bind(liked)
        .bind(date)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}
#[tokio::test]
async fn local_selection_uses_principal_album_for_lsua_and_all_saved_links_for_ussa() -> Result<()>
{
    let (_dir, mut db) = setup().await?;
    album(&mut db, "saved", true, "2026-02-10T00:00:00Z", "2020").await?;
    album(&mut db, "other", true, "2026-03-01T00:00:00Z", "2021").await?;
    album(&mut db, "unsaved", false, "2026-01-01T00:00:00Z", "2026-02").await?;
    track(&mut db, "liked", "unsaved", true, "2026-02-05T00:00:00Z").await?;
    track(&mut db, "savedLiked", "saved", true, "2026-02-05T00:00:00Z").await?;
    track(&mut db, "unliked", "unsaved", false, "").await?;
    database::track(&mut db, &json!({"id":"unliked"}), "saved").await?;
    database::track(&mut db, &json!({"id":"unliked"}), "other").await?;
    database::track(&mut db, &json!({"id":"savedLiked"}), "unsaved").await?;
    let lsua = playlist::preview(&mut db, &options(Kind::Lsua)).await?;
    assert_eq!(
        lsua.candidates
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        ["liked"]
    );
    let ussa = playlist::preview(&mut db, &options(Kind::Ussa)).await?;
    assert_eq!(ussa.count, 1);
    assert_eq!(ussa.candidates[0].id, "unliked");
    assert_eq!(lsua.name, "LSUA - All");
    assert_eq!(ussa.name, "USSA - All");
    Ok(())
}
#[tokio::test]
async fn dates_accept_calendar_periods_use_album_saved_date_and_overlap_release_precision(
) -> Result<()> {
    let (_dir, mut db) = setup().await?;
    album(&mut db, "a", true, "2026-02-28T23:59:59Z", "2020").await?;
    track(&mut db, "t", "a", false, "2026-06-01T00:00:00Z").await?;
    let mut opts = options(Kind::Ussa);
    opts.from = Some("02/2026".into());
    assert_eq!(playlist::preview(&mut db, &opts).await?.count, 1);
    assert_eq!(opts.default_name()?, "USSA - 02-2026");
    opts.from = Some("03/2026".into());
    assert_eq!(playlist::preview(&mut db, &opts).await?.count, 0);
    opts.date_basis = DateBasis::Release;
    opts.from = Some("10/05/2020".into());
    assert_eq!(playlist::preview(&mut db, &opts).await?.count, 1);
    opts.to = Some("06/2020".into());
    assert_eq!(opts.default_name()?, "USSA - 10-05-2020 to 06-2020");
    opts.from = Some("31/02/2026".into());
    assert!(playlist::preview(&mut db, &opts).await.is_err());
    opts.from = Some("2027".into());
    opts.to = Some("2020".into());
    assert!(playlist::preview(&mut db, &opts).await.is_err());
    opts.from = None;
    opts.to = Some("2020".into());
    assert_eq!(opts.default_name()?, "USSA - Through 2020");
    Ok(())
}
#[tokio::test]
async fn local_preview_reports_missing_baselines_pending_albums_and_never_imports() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    album(&mut db, "a", true, "2026-01-01T00:00:00Z", "2020").await?;
    track(&mut db, "t", "a", false, "").await?;
    sqlx::query("UPDATE album SET tracks_fetched_at=NULL")
        .execute(&mut db)
        .await?;
    let preview = playlist::preview(&mut db, &options(Kind::Ussa)).await?;
    assert!(preview.incomplete);
    assert!(preview
        .warnings
        .iter()
        .any(|s| s.contains("faixas incompletas")));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_job")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(count, 0);
    Ok(())
}
#[tokio::test]
async fn invalid_local_unavailable_and_duplicate_uris_are_filtered_without_network() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    album(&mut db, "a", false, "", "2020").await?;
    for id in ["good", "duplicate", "local", "blocked", "missing"] {
        track(&mut db, id, "a", true, "2026-01-01T00:00:00Z").await?;
    }
    sqlx::query("UPDATE musica SET spotify_uri='spotify:track:good' WHERE id='duplicate'")
        .execute(&mut db)
        .await?;
    sqlx::query("UPDATE musica SET is_local=1 WHERE id='local'")
        .execute(&mut db)
        .await?;
    sqlx::query("UPDATE musica SET is_playable=0 WHERE id='blocked'")
        .execute(&mut db)
        .await?;
    sqlx::query("UPDATE musica SET spotify_uri=NULL WHERE id='missing'")
        .execute(&mut db)
        .await?;
    let preview = playlist::preview(&mut db, &options(Kind::Lsua)).await?;
    assert_eq!(preview.count, 1);
    assert!(!preview.public);
    Ok(())
}
#[derive(Default)]
struct Mock {
    account: String,
    remote: Vec<String>,
    description: String,
    created: usize,
    batches: Vec<usize>,
    fail_create: bool,
    reject_create: bool,
    hide_created: bool,
    fail_append: bool,
    apply_failed_append: bool,
    fail_on_batch: usize,
    reads: Vec<String>,
    remaining: Option<u32>,
}
impl Mock {
    fn charge(&mut self) -> Result<()> {
        if let Some(remaining) = &mut self.remaining {
            if *remaining == 0 {
                return Err(spotify_library_sync::spotify::BudgetExhausted.into());
            }
            *remaining -= 1;
        }
        Ok(())
    }
}
#[async_trait]
impl Api for Mock {
    fn set_budget(&mut self, remaining: Option<u32>) {
        self.remaining = remaining;
    }
    async fn get(&mut self, path: &str, _offset: Option<i64>) -> Result<Value> {
        self.charge()?;
        self.reads.push(path.into());
        assert_eq!(path, "/me", "No library data may be fetched");
        Ok(json!({"id":if self.account.is_empty(){"owner"}else{&self.account}}))
    }
}
fn page(items: Vec<Value>, offset: i64, total: usize) -> Value {
    let end = offset as usize + items.len();
    json!({"items":items,"offset":offset,"total":total,"next":if end<total {Some("ignored-untrusted-url")}else{None}})
}
#[async_trait]
impl PlaylistApi for Mock {
    async fn create_playlist(
        &mut self,
        _name: &str,
        _public: bool,
        description: &str,
    ) -> Result<Value> {
        self.charge()?;
        if self.reject_create {
            self.reject_create = false;
            return Err(anyhow::anyhow!("forbidden").context(WriteRejected));
        }
        self.created += 1;
        self.description = description.into();
        if self.fail_create {
            self.fail_create = false;
            bail!("create response lost");
        }
        Ok(json!({"id":"playlistID"}))
    }
    async fn append_playlist(&mut self, id: &str, uris: &[String]) -> Result<Value> {
        self.charge()?;
        assert_eq!(id, "playlistID");
        assert!(uris.len() <= 100);
        self.batches.push(uris.len());
        let failed = self.fail_append || self.batches.len() == self.fail_on_batch;
        self.fail_append = false;
        if !failed || self.apply_failed_append {
            self.remote.extend_from_slice(uris);
        }
        if failed {
            bail!("append response lost");
        }
        Ok(json!({"snapshot_id":"snapshot"}))
    }
    async fn own_playlists(&mut self, offset: i64) -> Result<Value> {
        self.charge()?;
        self.reads.push("playlists".into());
        let items = if self.hide_created {
            vec![]
        } else {
            vec![json!({"id":"playlistID","owner":{"id":"owner"},"description":self.description})]
        };
        let total = items.len();
        Ok(page(items, offset, total))
    }
    async fn playlist_items(&mut self, id: &str, offset: i64) -> Result<Value> {
        self.charge()?;
        assert_eq!(id, "playlistID");
        self.reads.push("playlist_items".into());
        Ok(page(
            self.remote
                .iter()
                .skip(offset as usize)
                .take(50)
                .map(|u| json!({"item":{"uri":u}}))
                .collect(),
            offset,
            self.remote.len(),
        ))
    }
}
async fn job_with_tracks(db: &mut SqliteConnection, n: usize) -> Result<String> {
    album(db, "a", false, "", "2020").await?;
    for i in 0..n {
        track(db, &format!("t{i:04}"), "a", true, "2026-01-01T00:00:00Z").await?;
    }
    let opts = options(Kind::Lsua);
    let preview = playlist::preview(db, &opts).await?;
    playlist::freeze(db, &opts, &preview).await
}
#[tokio::test]
async fn creates_in_batches_and_never_reads_remote_library_or_mutates_saved_state() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 250).await?;
    let mut api = Mock::default();
    let result = playlist::run(&mut db, &mut api, &job).await?;
    assert_eq!(result["tracks"], 250);
    assert_eq!(api.batches, [100, 100, 50]);
    assert_eq!(api.created, 1);
    assert_eq!(api.reads, ["/me"]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM musica WHERE is_saved=1")
            .fetch_one(&mut db)
            .await?,
        250
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM album WHERE is_saved=1")
            .fetch_one(&mut db)
            .await?,
        0
    );
    playlist::run(&mut db, &mut api, &job).await?;
    assert_eq!(api.created, 1);
    assert_eq!(api.batches.len(), 3);
    Ok(())
}
#[tokio::test]
async fn resume_after_applied_append_reconciles_frozen_plan_without_duplicates() -> Result<()> {
    let (dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 250).await?;
    let mut api = Mock {
        fail_on_batch: 2,
        apply_failed_append: true,
        ..Default::default()
    };
    assert!(playlist::run(&mut db, &mut api, &job).await.is_err());
    assert_eq!(api.remote.len(), 200);
    // Change the library after selecting: the job must still send the original 250 tracks.
    sqlx::query("UPDATE musica SET is_saved=0")
        .execute(&mut db)
        .await?;
    db.close().await?;
    let mut db = database::open(&dir.path().join("db.sqlite")).await?;
    playlist::run(&mut db, &mut api, &job).await?;
    assert_eq!(api.remote.len(), 250);
    assert_eq!(api.created, 1);
    assert_eq!(api.batches, [100, 100, 50]);
    assert_eq!(
        api.remote
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        250
    );
    Ok(())
}
#[tokio::test]
async fn resume_after_unapplied_append_retries_only_that_batch() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 120).await?;
    let mut api = Mock {
        fail_on_batch: 2,
        ..Default::default()
    };
    assert!(playlist::run(&mut db, &mut api, &job).await.is_err());
    assert_eq!(api.remote.len(), 100);
    playlist::run(&mut db, &mut api, &job).await?;
    assert_eq!(api.remote.len(), 120);
    assert_eq!(api.batches, [100, 20, 20]);
    Ok(())
}
#[tokio::test]
async fn lost_create_response_recovers_playlist_by_job_marker_without_second_create() -> Result<()>
{
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 3).await?;
    let mut api = Mock {
        fail_create: true,
        ..Default::default()
    };
    assert!(playlist::run(&mut db, &mut api, &job).await.is_err());
    playlist::run(&mut db, &mut api, &job).await?;
    assert_eq!(api.created, 1);
    assert_eq!(api.remote.len(), 3);
    Ok(())
}
#[tokio::test]
async fn uncertain_creation_not_found_never_creates_another_automatically() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 3).await?;
    let mut api = Mock {
        fail_create: true,
        hide_created: true,
        ..Default::default()
    };
    assert!(playlist::run(&mut db, &mut api, &job).await.is_err());
    let error = playlist::run(&mut db, &mut api, &job).await.unwrap_err();
    assert!(error.to_string().contains("Nenhuma outra"));
    assert_eq!(api.created, 1);
    Ok(())
}
#[tokio::test]
async fn explicit_create_rejection_allows_a_later_normal_resume() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 3).await?;
    let mut api = Mock {
        reject_create: true,
        ..Default::default()
    };
    assert!(playlist::run(&mut db, &mut api, &job).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM playlist_job")
            .fetch_one(&mut db)
            .await?,
        "ready"
    );
    playlist::run(&mut db, &mut api, &job).await?;
    assert_eq!(api.created, 1);
    Ok(())
}
#[tokio::test]
async fn external_edits_pause_without_appending_or_overwriting() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 120).await?;
    let mut api = Mock {
        fail_on_batch: 2,
        ..Default::default()
    };
    assert!(playlist::run(&mut db, &mut api, &job).await.is_err());
    api.remote[0] = "spotify:track:edited".into();
    assert!(playlist::run(&mut db, &mut api, &job)
        .await
        .unwrap_err()
        .to_string()
        .contains("modificada"));
    assert_eq!(api.batches.len(), 2);
    Ok(())
}
#[tokio::test]
async fn account_mismatch_cannot_create_or_fill_playlist() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 2).await?;
    let mut api = Mock {
        account: "someoneElse".into(),
        ..Default::default()
    };
    assert!(playlist::run(&mut db, &mut api, &job).await.is_err());
    assert_eq!(api.created, 0);
    assert!(api.batches.is_empty());
    Ok(())
}
#[tokio::test]
async fn playlist_quota_persists_the_shared_cooldown() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 2).await?;
    let error = Paused {
        message: "quota".into(),
        retry_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
    }
    .into();
    playlist::record_error(&mut db, &job, &error).await?;
    let mut api = Mock::default();
    assert!(playlist::run(&mut db, &mut api, &job).await.is_err());
    assert!(api.reads.is_empty());
    Ok(())
}
#[tokio::test]
async fn empty_selection_never_creates_a_job_and_bad_name_is_rejected() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let mut opts = options(Kind::Lsua);
    let preview = playlist::preview(&mut db, &opts).await?;
    assert!(playlist::freeze(&mut db, &opts, &preview).await.is_err());
    opts.name = Some("  ".into());
    assert!(playlist::preview(&mut db, &opts).await.is_err());
    Ok(())
}

#[tokio::test]
async fn request_budget_pauses_after_committed_batches_and_resume_advances() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 250).await?;
    let mut api = Mock::default();
    api.set_budget(Some(3));
    let error = playlist::run(&mut db, &mut api, &job).await.unwrap_err();
    assert!(error.is::<spotify_library_sync::spotify::BudgetExhausted>());
    assert_eq!(api.remote.len(), 100);
    assert_eq!(api.created, 1);
    api.set_budget(Some(20));
    playlist::run(&mut db, &mut api, &job).await?;
    assert_eq!(api.remote.len(), 250);
    assert_eq!(api.created, 1);
    assert_eq!(api.batches, [100, 100, 50]);
    Ok(())
}
#[tokio::test]
async fn partial_uncertain_batch_is_never_blindly_reappended() -> Result<()> {
    let (_dir, mut db) = setup().await?;
    let job = job_with_tracks(&mut db, 120).await?;
    let mut api = Mock {
        fail_append: true,
        apply_failed_append: true,
        ..Default::default()
    };
    assert!(playlist::run(&mut db, &mut api, &job).await.is_err());
    api.remote.truncate(50);
    assert!(playlist::run(&mut db, &mut api, &job)
        .await
        .unwrap_err()
        .to_string()
        .contains("parcialmente"));
    assert_eq!(api.batches.len(), 1);
    Ok(())
}
