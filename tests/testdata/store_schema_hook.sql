-- index idx_candidates_create_key (on candidates)
CREATE UNIQUE INDEX idx_candidates_create_key
  ON candidates(repo_key, rule) WHERE candidate_kind = 'create';

-- index idx_candidates_fix_key (on candidates)
CREATE UNIQUE INDEX idx_candidates_fix_key
  ON candidates(repo_key, target_hook_name, target_source_file) WHERE candidate_kind = 'fix';

-- index idx_candidates_repo_status (on candidates)
CREATE INDEX idx_candidates_repo_status ON candidates(repo_key, status);

-- index idx_feedback_session (on feedback_events)
CREATE INDEX idx_feedback_session ON feedback_events(session_id);

-- index idx_feedback_source (on feedback_events)
CREATE INDEX idx_feedback_source ON feedback_events(source_kind);

-- index idx_observations_dedup (on candidate_observations)
CREATE INDEX idx_observations_dedup ON candidate_observations(dedup_key);

-- index idx_verdicts_dedup (on verdicts)
CREATE INDEX idx_verdicts_dedup ON verdicts(dedup_key);

-- table candidate_observations (on candidate_observations)
CREATE TABLE candidate_observations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  candidate_id INTEGER NOT NULL REFERENCES candidates(id),
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  session_id TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  UNIQUE(candidate_id, dedup_key)
);

-- table candidates (on candidates)
CREATE TABLE candidates (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_key TEXT NOT NULL,
  candidate_kind TEXT NOT NULL CHECK (candidate_kind IN ('create', 'fix')),
  rule TEXT NOT NULL,
  source_kind TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('watching', 'pr_open', 'stale', 'accepted', 'rejected')),
  pr_url TEXT,
  pr_opened_at TEXT,
  target_source_file TEXT,
  target_hook_name TEXT,
  misfire_class TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  generation INTEGER NOT NULL,
  resolved_at TEXT,
  origin_repo_key TEXT,
  pack_name TEXT,
  announced_status TEXT,
  pr_title TEXT,
  CHECK (
    (candidate_kind = 'create' AND target_source_file IS NULL AND target_hook_name IS NULL
      AND misfire_class IS NULL)
    OR (candidate_kind = 'fix' AND target_source_file IS NOT NULL AND target_hook_name IS NOT NULL)
  )
);

-- table cc_transcript_schema_v1 (on cc_transcript_schema_v1)
CREATE TABLE cc_transcript_schema_v1 (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  schema_identity TEXT NOT NULL,
  schema_version INTEGER NOT NULL CHECK (schema_version = 1),
  ddl_fingerprint TEXT NOT NULL CHECK (length(ddl_fingerprint) = 64),
  object_fingerprint TEXT NOT NULL CHECK (length(object_fingerprint) = 64)
) STRICT;

-- table feedback_events (on feedback_events)
CREATE TABLE feedback_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL UNIQUE,
  source_kind TEXT NOT NULL,
  session_id TEXT,
  event_uuid TEXT,
  occurred_at TEXT NOT NULL,
  text TEXT NOT NULL,
  payload_json TEXT,
  context_json TEXT NOT NULL,
  cc_version TEXT,
  ingested_at TEXT NOT NULL,
  triage TEXT
);

-- table files (on files)
CREATE TABLE files (
  path TEXT PRIMARY KEY,
  mtime REAL NOT NULL
);

-- table pr_states (on pr_states)
CREATE TABLE pr_states (
  pr_url TEXT PRIMARY KEY,
  state TEXT NOT NULL,
  merged_at TEXT,
  fetched_at TEXT NOT NULL
);

-- table repos (on repos)
CREATE TABLE repos (
  repo_key TEXT PRIMARY KEY,
  watching INTEGER NOT NULL
);

-- table review_meta (on review_meta)
CREATE TABLE review_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- table review_triage (on review_triage)
CREATE TABLE review_triage (
  dedup_key TEXT PRIMARY KEY REFERENCES feedback_events(dedup_key) ON DELETE CASCADE,
  triage TEXT NOT NULL CHECK (triage IN ('junk', 'keep'))
);

-- table spawn_runs (on spawn_runs)
CREATE TABLE spawn_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at TEXT NOT NULL,
  finished_at TEXT NOT NULL,
  transcript TEXT NOT NULL,
  ok INTEGER NOT NULL,
  error TEXT,
  report_json TEXT,
  CHECK ((ok = 1) = (error IS NULL))
);

-- table verdicts (on verdicts)
CREATE TABLE verdicts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  role TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  model TEXT NOT NULL,
  category TEXT NOT NULL,
  accepted INTEGER NOT NULL,
  summary TEXT NOT NULL,
  confidence REAL NOT NULL,
  rationale TEXT NOT NULL,
  canonical_key TEXT,
  fidelity TEXT NOT NULL CHECK(fidelity IN ('full','summary')),
  judged_at TEXT NOT NULL,
  UNIQUE(dedup_key, role, prompt_version)
);

