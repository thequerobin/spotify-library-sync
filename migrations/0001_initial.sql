CREATE TABLE account (id TEXT PRIMARY KEY, created_at TEXT NOT NULL);
CREATE TABLE album (
 id TEXT PRIMARY KEY, name TEXT, album_type TEXT, total_tracks INTEGER,
 release_date TEXT, release_date_precision TEXT, spotify_uri TEXT, spotify_url TEXT,
 href TEXT, image_url TEXT, label TEXT, popularity INTEGER, upc TEXT, ean TEXT,
 is_saved INTEGER NOT NULL DEFAULT 0 CHECK(is_saved IN(0,1)), saved_at TEXT,
 last_seen_saved_sync TEXT, metadata_fetched_at TEXT NOT NULL,
 tracks_fetched_at TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
 raw_json TEXT NOT NULL CHECK(json_valid(raw_json))
);
CREATE TABLE musica (
 id TEXT PRIMARY KEY, album_id TEXT REFERENCES album(id), name TEXT, duration_ms INTEGER,
 track_number INTEGER, disc_number INTEGER, explicit INTEGER, is_local INTEGER,
 is_playable INTEGER, spotify_uri TEXT, spotify_url TEXT, href TEXT, isrc TEXT,
 popularity INTEGER, preview_url TEXT,
 is_saved INTEGER NOT NULL DEFAULT 0 CHECK(is_saved IN(0,1)), saved_at TEXT,
 last_seen_saved_sync TEXT, metadata_fetched_at TEXT NOT NULL,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
 raw_json TEXT NOT NULL CHECK(json_valid(raw_json))
);
CREATE TABLE artista (
 id TEXT PRIMARY KEY, name TEXT, spotify_uri TEXT, spotify_url TEXT, href TEXT,
 raw_json TEXT NOT NULL CHECK(json_valid(raw_json)), created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE musica_artista (
 musica_id TEXT NOT NULL REFERENCES musica(id), artista_id TEXT NOT NULL REFERENCES artista(id),
 position INTEGER NOT NULL, PRIMARY KEY(musica_id,artista_id)
);
CREATE TABLE album_artista (
 album_id TEXT NOT NULL REFERENCES album(id), artista_id TEXT NOT NULL REFERENCES artista(id),
 position INTEGER NOT NULL, PRIMARY KEY(album_id,artista_id)
);
CREATE TABLE album_musica (
 album_id TEXT NOT NULL REFERENCES album(id), musica_id TEXT NOT NULL REFERENCES musica(id),
 disc_number INTEGER, track_number INTEGER, PRIMARY KEY(album_id,musica_id)
);
CREATE TABLE sync_job (
 id TEXT PRIMARY KEY, job_type TEXT NOT NULL, status TEXT NOT NULL,
 stage TEXT NOT NULL, next_offset INTEGER NOT NULL DEFAULT 0,
 total_items INTEGER, processed_items INTEGER NOT NULL DEFAULT 0,
 request_count INTEGER NOT NULL DEFAULT 0, retry_count INTEGER NOT NULL DEFAULT 0,
 error_count INTEGER NOT NULL DEFAULT 0, last_error TEXT, retry_at TEXT,
 summary_json TEXT, started_at TEXT NOT NULL, updated_at TEXT NOT NULL, completed_at TEXT
);
CREATE UNIQUE INDEX one_active_job ON sync_job((1)) WHERE status <> 'completed';
CREATE TABLE sync_album_queue (
 job_id TEXT NOT NULL REFERENCES sync_job(id), album_id TEXT NOT NULL REFERENCES album(id),
 status TEXT NOT NULL DEFAULT 'pending', next_offset INTEGER NOT NULL DEFAULT 0,
 total_items INTEGER, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT,
 updated_at TEXT NOT NULL, PRIMARY KEY(job_id,album_id)
);
CREATE TABLE sync_seen_item (
 job_id TEXT NOT NULL REFERENCES sync_job(id), item_type TEXT NOT NULL,
 item_id TEXT NOT NULL, saved_at TEXT NOT NULL,
 PRIMARY KEY(job_id,item_type,item_id)
);
CREATE TABLE sync_page (
 job_id TEXT NOT NULL REFERENCES sync_job(id), item_type TEXT NOT NULL,
 page_offset INTEGER NOT NULL, fingerprint TEXT NOT NULL, total INTEGER NOT NULL,
 PRIMARY KEY(job_id,item_type,page_offset)
);
CREATE INDEX album_saved ON album(is_saved);
CREATE INDEX album_saved_at ON album(saved_at);
CREATE INDEX album_release ON album(release_date);
CREATE INDEX musica_saved ON musica(is_saved);
CREATE INDEX musica_album ON musica(album_id);
CREATE INDEX musica_saved_at ON musica(saved_at);
CREATE INDEX musica_artista_reverse ON musica_artista(artista_id);
CREATE INDEX album_artista_reverse ON album_artista(artista_id);
CREATE INDEX album_musica_reverse ON album_musica(musica_id);
