ALTER TABLE sync_job ADD COLUMN sync_mode TEXT NOT NULL DEFAULT 'legacy';
ALTER TABLE sync_job ADD COLUMN coverage TEXT NOT NULL DEFAULT 'full_listing';
ALTER TABLE sync_job ADD COLUMN pause_reason TEXT;
ALTER TABLE sync_job ADD COLUMN collection_budget INTEGER NOT NULL DEFAULT 20;
ALTER TABLE sync_job ADD COLUMN album_budget INTEGER NOT NULL DEFAULT 20;
ALTER TABLE sync_job ADD COLUMN known_pages INTEGER NOT NULL DEFAULT 2;
CREATE TABLE library_sync_state (
 collection TEXT PRIMARY KEY CHECK(collection IN('track','album')),
 account_id TEXT NOT NULL DEFAULT '', initial_import_completed_at TEXT,
 last_incremental_completed_at TEXT, last_full_reconciliation_at TEXT,
 watermark_added_at TEXT, remote_total_last_seen INTEGER, head_signature TEXT,
 ordering_observed TEXT, last_coverage TEXT NOT NULL DEFAULT 'inconclusive', last_successful_job_id TEXT
);
INSERT INTO library_sync_state(collection,account_id,initial_import_completed_at,last_full_reconciliation_at,last_successful_job_id,last_coverage)
 SELECT kind,COALESCE((SELECT id FROM account LIMIT 1),''),
 (SELECT MIN(completed_at) FROM sync_job WHERE status='completed' AND stage='done'),
 (SELECT MAX(completed_at) FROM sync_job WHERE status='completed' AND stage='done'),
 (SELECT id FROM sync_job WHERE status='completed' AND stage='done' ORDER BY completed_at DESC LIMIT 1),
 CASE WHEN EXISTS(SELECT 1 FROM sync_job WHERE status='completed' AND stage='done') THEN 'full_listing' ELSE 'inconclusive' END
 FROM (SELECT 'track' kind UNION ALL SELECT 'album');
CREATE TABLE sync_baseline_item (
 job_id TEXT NOT NULL REFERENCES sync_job(id), collection TEXT NOT NULL,
 item_id TEXT NOT NULL, saved_at TEXT, PRIMARY KEY(job_id,collection,item_id)
);
CREATE TABLE sync_scope (
 job_id TEXT NOT NULL REFERENCES sync_job(id), collection TEXT NOT NULL,
 state TEXT NOT NULL DEFAULT 'pending', next_offset INTEGER NOT NULL DEFAULT 0,
 pages_processed INTEGER NOT NULL DEFAULT 0, consecutive_known INTEGER NOT NULL DEFAULT 0,
 changed_items INTEGER NOT NULL DEFAULT 0, previous_added_at TEXT, head_signature TEXT,
 observed_total INTEGER, stop_reason TEXT, published_items INTEGER NOT NULL DEFAULT 0,
 needs_head_check INTEGER NOT NULL DEFAULT 0, requests_total INTEGER NOT NULL DEFAULT 0,
 invocation_requests INTEGER NOT NULL DEFAULT 0, request_limit INTEGER,
 PRIMARY KEY(job_id,collection)
);
