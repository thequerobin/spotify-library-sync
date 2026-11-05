ALTER TABLE sync_job ADD COLUMN resume_search_pages INTEGER NOT NULL DEFAULT 10;
CREATE TABLE full_scan_checkpoint (
 job_id TEXT NOT NULL REFERENCES sync_job(id), collection TEXT NOT NULL,
 phase TEXT NOT NULL DEFAULT 'collecting', scan_offset INTEGER NOT NULL DEFAULT 0,
 scan_total INTEGER, scan_anchor_json TEXT,
 verify_offset INTEGER NOT NULL DEFAULT 0, verify_total INTEGER, verify_anchor_json TEXT,
 verify_head TEXT, updated_at TEXT NOT NULL,
 PRIMARY KEY(job_id,collection)
);
CREATE TABLE full_scan_verified_item (
 job_id TEXT NOT NULL REFERENCES sync_job(id), collection TEXT NOT NULL,
 item_id TEXT NOT NULL, saved_at TEXT NOT NULL,
 PRIMARY KEY(job_id,collection,item_id)
);
