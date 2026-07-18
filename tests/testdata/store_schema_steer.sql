-- index idx_feedback_session (on feedback_events)
CREATE INDEX idx_feedback_session ON feedback_events(session_id);

-- index idx_feedback_source (on feedback_events)
CREATE INDEX idx_feedback_source ON feedback_events(source_kind);

-- index idx_gate_sample_kind (on gate_sample)
CREATE INDEX idx_gate_sample_kind ON gate_sample(kind);

-- index idx_gate_sample_session (on gate_sample)
CREATE INDEX idx_gate_sample_session ON gate_sample(session_id);

-- index idx_refinement_dedup (on refinement)
CREATE INDEX idx_refinement_dedup ON refinement(dedup_key);

-- index idx_triage_dedup (on triage)
CREATE INDEX idx_triage_dedup ON triage(dedup_key);

-- table exemplar_embedding (on exemplar_embedding)
CREATE TABLE exemplar_embedding (
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  model TEXT NOT NULL,
  text_digest TEXT NOT NULL,
  dim INTEGER NOT NULL,
  vector BLOB NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(dedup_key, model)
);

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
  origin_path TEXT,
  quarantined_reason TEXT
);

-- table files (on files)
CREATE TABLE files (
  path TEXT PRIMARY KEY,
  mtime REAL NOT NULL
);

-- table gate_sample (on gate_sample)
CREATE TABLE gate_sample (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  sample_key TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL,
  dedup_key TEXT,
  session_id TEXT NOT NULL,
  anchor_uuid TEXT NOT NULL,
  occurred_at TEXT,
  offset_turns INTEGER NOT NULL DEFAULT 0,
  window_json TEXT NOT NULL,
  seed INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
);

-- table refinement (on refinement)
CREATE TABLE refinement (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  prompt_version INTEGER NOT NULL,
  model TEXT NOT NULL,
  pair_index INTEGER NOT NULL,
  action TEXT NOT NULL,
  direction_verbatim TEXT NOT NULL,
  direction TEXT NOT NULL,
  refined_at TEXT NOT NULL,
  UNIQUE(dedup_key, prompt_version, model, pair_index)
);

-- table sampled_session (on sampled_session)
CREATE TABLE sampled_session (
  session_id TEXT PRIMARY KEY,
  sampled_at TEXT NOT NULL
);

-- table triage (on triage)
CREATE TABLE triage (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  role TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  model TEXT NOT NULL,
  category TEXT NOT NULL,
  is_steering INTEGER NOT NULL,
  what_claude_did TEXT NOT NULL,
  confidence REAL NOT NULL,
  rationale TEXT NOT NULL,
  canonical_key TEXT,
  fidelity TEXT NOT NULL CHECK(fidelity IN ('full','summary')),
  judged_at TEXT NOT NULL,
  UNIQUE(dedup_key, role, prompt_version)
);

-- view accepted_steering (on accepted_steering)
CREATE VIEW accepted_steering AS
SELECT
  e.id AS event_id,
  e.dedup_key,
  e.source_kind,
  e.text,
  e.context_json,
  e.payload_json,
  t.category,
  t.what_claude_did,
  e.origin_path
FROM feedback_events e
JOIN latest_judge t ON t.dedup_key = e.dedup_key
WHERE t.is_steering = 1 AND e.quarantined_reason IS NULL;

-- view latest_auditor (on latest_auditor)
CREATE VIEW latest_auditor AS
SELECT * FROM (
  SELECT t.*, ROW_NUMBER() OVER (
    PARTITION BY t.dedup_key ORDER BY t.prompt_version DESC, t.judged_at DESC, t.id DESC
  ) AS rn
  FROM triage t
  WHERE t.role = 'auditor'
) WHERE rn = 1;

-- view latest_judge (on latest_judge)
CREATE VIEW latest_judge AS
SELECT * FROM (
  SELECT t.*, ROW_NUMBER() OVER (
    PARTITION BY t.dedup_key ORDER BY t.prompt_version DESC, t.judged_at DESC, t.id DESC
  ) AS rn
  FROM triage t
  WHERE t.role = 'judge'
) WHERE rn = 1;

-- view latest_refinement (on latest_refinement)
CREATE VIEW latest_refinement AS
WITH gens AS (
  SELECT dedup_key, prompt_version, model, refined_at,
    ROW_NUMBER() OVER (
      PARTITION BY dedup_key ORDER BY prompt_version DESC, refined_at DESC
    ) AS g
  FROM (SELECT DISTINCT dedup_key, prompt_version, model, refined_at FROM refinement)
)
SELECT r.*
FROM refinement r
JOIN gens ON gens.dedup_key = r.dedup_key AND gens.prompt_version = r.prompt_version
         AND gens.model = r.model AND gens.refined_at = r.refined_at AND gens.g = 1;

-- view refined_pairs (on refined_pairs)
CREATE VIEW refined_pairs AS
SELECT
  e.id AS event_id,
  r.dedup_key,
  r.pair_index,
  r.action,
  r.direction_verbatim,
  r.direction,
  e.text AS original_message,
  ap.category,
  e.source_kind,
  e.session_id,
  e.event_uuid,
  e.occurred_at,
  e.origin_path,
  r.prompt_version,
  r.model
FROM latest_refinement r
JOIN feedback_events e ON e.dedup_key = r.dedup_key
JOIN accepted_steering ap ON ap.dedup_key = r.dedup_key
ORDER BY e.id, r.pair_index;

