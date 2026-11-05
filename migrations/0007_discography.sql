CREATE TABLE artista_seguido (
 artista_id TEXT PRIMARY KEY REFERENCES artista(id), is_followed INTEGER NOT NULL DEFAULT 0 CHECK(is_followed IN(0,1)),
 last_seen_job TEXT, checked_at TEXT NOT NULL
);
CREATE TABLE discografia_job (
 id TEXT PRIMARY KEY, kind TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'running', phase TEXT NOT NULL,
 options_json TEXT NOT NULL CHECK(json_valid(options_json)), account_id TEXT,
 catalog_context TEXT, album_context TEXT,
 following_cursor TEXT, following_total INTEGER, request_limit INTEGER NOT NULL,
 request_count INTEGER NOT NULL DEFAULT 0, retry_count INTEGER NOT NULL DEFAULT 0,
 invocation_requests INTEGER NOT NULL DEFAULT 0, last_error TEXT, pause_reason TEXT,
 started_at TEXT NOT NULL, updated_at TEXT NOT NULL, completed_at TEXT
);
CREATE UNIQUE INDEX one_discography_job ON discografia_job((1)) WHERE status<>'completed';
CREATE TABLE discografia_followed_seen (
 job_id TEXT NOT NULL REFERENCES discografia_job(id), artista_id TEXT NOT NULL REFERENCES artista(id),
 PRIMARY KEY(job_id,artista_id)
);
CREATE TABLE discografia_followed_cursor (
 job_id TEXT NOT NULL REFERENCES discografia_job(id), cursor TEXT NOT NULL, PRIMARY KEY(job_id,cursor)
);
CREATE TABLE discografia_artista_queue (
 job_id TEXT NOT NULL REFERENCES discografia_job(id), artista_id TEXT NOT NULL REFERENCES artista(id),
 mode TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'pending', next_offset INTEGER NOT NULL DEFAULT 0,
 total_items INTEGER, previous_offset INTEGER, previous_fingerprint TEXT,
 known_pages INTEGER NOT NULL DEFAULT 0, pages_processed INTEGER NOT NULL DEFAULT 0,
 stop_reason TEXT, last_error TEXT, PRIMARY KEY(job_id,artista_id)
);
CREATE TABLE discografia_release_baseline (
 job_id TEXT NOT NULL REFERENCES discografia_job(id), artista_id TEXT NOT NULL REFERENCES artista(id),
 album_id TEXT NOT NULL REFERENCES album(id), PRIMARY KEY(job_id,artista_id,album_id)
);
CREATE TABLE artista_lancamento (
 artista_id TEXT NOT NULL REFERENCES artista(id), album_id TEXT NOT NULL REFERENCES album(id),
 catalog_context TEXT NOT NULL, album_context TEXT NOT NULL, groups_filter TEXT NOT NULL,
 album_group TEXT, first_seen_at TEXT NOT NULL, last_seen_at TEXT NOT NULL, last_seen_job TEXT NOT NULL,
 PRIMARY KEY(artista_id,album_id,catalog_context)
);
CREATE INDEX lancamento_album ON artista_lancamento(album_context,album_id);
CREATE TABLE discografia_cache (
 artista_id TEXT NOT NULL REFERENCES artista(id), catalog_context TEXT NOT NULL,
 last_checked_at TEXT NOT NULL, last_full_at TEXT, coverage TEXT NOT NULL,
 last_job_id TEXT NOT NULL, needs_full_refresh INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(artista_id,catalog_context)
);
CREATE TABLE discografia_album_queue (
 album_id TEXT NOT NULL REFERENCES album(id), album_context TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'pending',
 next_offset INTEGER NOT NULL DEFAULT 0, total_items INTEGER, source_total INTEGER,
 attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, completed_at TEXT,
 PRIMARY KEY(album_id,album_context)
);
CREATE TABLE discografia_album_position (
 album_id TEXT NOT NULL, album_context TEXT NOT NULL, position INTEGER NOT NULL, musica_id TEXT NOT NULL REFERENCES musica(id),
 PRIMARY KEY(album_id,album_context,position),
 FOREIGN KEY(album_id,album_context) REFERENCES discografia_album_queue(album_id,album_context)
);
