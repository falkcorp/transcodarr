-- file: crates/transcodarr-store/migrations/0001_initial.sql
-- version: 1.0.0
-- guid: 3b8c14e9-6a07-4d52-9f13-c5e80a274b6d
-- last-edited: 2026-08-03
--
-- Initial schema. Table and column names come from the naming contract in
-- docs/design/synthesis-decisions.md and must not drift from it.
--
-- STRICT tables throughout: SQLite's default type affinity will happily store
-- the string 'Running' in an INTEGER column, and a job state machine that can
-- be corrupted by a typo is not a state machine.

-- `schema_migration` is NOT created here. The migrator must be able to query it
-- before it can know which migrations to run, so it bootstraps that one table
-- itself. Declaring it again here would fail on a fresh database -- which is
-- exactly what it did the first time.

-- Space is budgeted against a server-assigned pool identity, never st_dev or a
-- mount path: two agents see the same pool at different paths and different
-- device numbers, and budgeting per-mount triple-counts the same free space.
CREATE TABLE storage_pool (
  id                  TEXT PRIMARY KEY,
  name                TEXT NOT NULL,
  dataset             TEXT NOT NULL,
  kind                TEXT NOT NULL,
  reserve_bytes       INTEGER NOT NULL DEFAULT 0,
  snapshot_policy_ok  INTEGER NOT NULL DEFAULT 0 CHECK (snapshot_policy_ok IN (0,1)),
  last_check_unix     INTEGER
) STRICT;

-- ZFS accounting, sampled. In-place replacement reclaims nothing while a
-- snapshot still references the old blocks, so reclaim is measured from these
-- rows rather than from the difference in file sizes.
CREATE TABLE pool_reclaim_sample (
  id                      INTEGER PRIMARY KEY AUTOINCREMENT,
  pool_id                 TEXT NOT NULL REFERENCES storage_pool(id) ON DELETE CASCADE,
  at_unix                 INTEGER NOT NULL,
  used_bytes              INTEGER NOT NULL,
  usedbysnapshots_bytes   INTEGER NOT NULL,
  available_bytes         INTEGER NOT NULL,
  referenced_bytes        INTEGER NOT NULL
) STRICT;

CREATE TABLE library (
  id                 TEXT PRIMARY KEY,
  name               TEXT NOT NULL,
  root_path          TEXT NOT NULL,
  dataset            TEXT,
  pool_id            TEXT REFERENCES storage_pool(id),
  work_dir           TEXT NOT NULL,
  trash_dir          TEXT NOT NULL,
  exclude_globs_json TEXT NOT NULL DEFAULT '[]',
  enabled            INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  scan_cron          TEXT,
  scan_parallelism   INTEGER NOT NULL DEFAULT 4,
  priority           INTEGER NOT NULL DEFAULT 0,
  -- A file still being written has a recent mtime. Enqueueing it races the
  -- writer, so discovery ignores anything younger than this.
  min_mtime_age_s    INTEGER NOT NULL DEFAULT 300,
  created_unix       INTEGER NOT NULL,
  updated_unix       INTEGER NOT NULL
) STRICT;

-- Immutable-ish file facts plus evaluation bookkeeping. Wide on purpose: the
-- Evaluator re-runs over these columns without touching a byte of media, which
-- is what makes re-deciding ~49.6k files cheap.
CREATE TABLE file (
  id                    INTEGER PRIMARY KEY AUTOINCREMENT,
  library_id            TEXT NOT NULL REFERENCES library(id) ON DELETE CASCADE,
  canonical_path        TEXT NOT NULL,
  path_hash             TEXT NOT NULL,
  size_bytes            INTEGER NOT NULL,
  mtime_unix            INTEGER NOT NULL,
  mtime_ns              INTEGER NOT NULL DEFAULT 0,
  -- Identity is (dev, inode), not path: a moved file is the same file, and a
  -- hardlinked one must not be processed twice.
  inode                 INTEGER,
  dev                   INTEGER,
  nlink                 INTEGER NOT NULL DEFAULT 1,
  size_bucket           TEXT,
  content_sig           TEXT,
  container             TEXT,
  duration_s            REAL,
  bitrate_bps           INTEGER,
  video_codec           TEXT,
  video_profile         TEXT,
  video_bit_depth       INTEGER,
  video_pix_fmt         TEXT,
  video_width           INTEGER,
  video_height          INTEGER,
  color_transfer        TEXT,
  color_primaries       TEXT,
  is_hdr                INTEGER NOT NULL DEFAULT 0 CHECK (is_hdr IN (0,1)),
  is_dovi               INTEGER NOT NULL DEFAULT 0 CHECK (is_dovi IN (0,1)),
  dovi_profile          INTEGER,
  has_object_audio      INTEGER NOT NULL DEFAULT 0 CHECK (has_object_audio IN (0,1)),
  audio_codecs          TEXT,
  audio_track_count     INTEGER NOT NULL DEFAULT 0,
  audio_bytes           INTEGER,
  subtitle_track_count  INTEGER NOT NULL DEFAULT 0,
  probe_json            TEXT,
  probe_at_unix         INTEGER,
  probe_tool_version    TEXT,
  state                 TEXT NOT NULL DEFAULT 'Discovered'
                          CHECK (state IN ('Discovered','Probing','Probed','ProbeFailed',
                                           'Evaluated','Processed','Quarantined','Missing')),
  decision              TEXT,
  decision_reason       TEXT,
  eval_rules_version    TEXT,
  eval_at_unix          INTEGER,
  -- Guards against a policy that keeps re-deciding the same file forever.
  same_decision_streak  INTEGER NOT NULL DEFAULT 0,
  quarantine_reason     TEXT,
  original_size_bytes   INTEGER,
  bytes_reclaimed       INTEGER NOT NULL DEFAULT 0,
  last_job_id           TEXT,
  first_seen_unix       INTEGER NOT NULL,
  last_seen_unix        INTEGER NOT NULL,
  scan_generation       INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE UNIQUE INDEX idx_file_path ON file(library_id, path_hash);
CREATE INDEX idx_file_identity ON file(dev, inode) WHERE inode IS NOT NULL;
CREATE INDEX idx_file_state ON file(state);
-- The Evaluator's working set: probed files whose decision predates the
-- current rules version. Batched 1000 at a time over this index.
CREATE INDEX idx_file_needs_eval ON file(library_id, eval_rules_version)
  WHERE state IN ('Probed','Evaluated');

CREATE TABLE file_stream (
  file_id          INTEGER NOT NULL REFERENCES file(id) ON DELETE CASCADE,
  stream_index     INTEGER NOT NULL,
  kind             TEXT NOT NULL,
  codec            TEXT NOT NULL,
  profile          TEXT,
  channels         INTEGER,
  channel_layout   TEXT,
  bit_depth        INTEGER,
  sample_rate      INTEGER,
  bit_rate_bps     INTEGER,
  frame_count      INTEGER,
  language         TEXT,
  title            TEXT,
  is_default       INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0,1)),
  is_forced        INTEGER NOT NULL DEFAULT 0 CHECK (is_forced IN (0,1)),
  disposition_json TEXT,
  PRIMARY KEY (file_id, stream_index)
) STRICT;

-- Markers are per decision class, not per file (flaw A3). A video no-gain must
-- not prevent a later Opus -> EAC3 audio pass on the same file.
CREATE TABLE file_skip_marker (
  file_id        INTEGER NOT NULL REFERENCES file(id) ON DELETE CASCADE,
  decision_class TEXT NOT NULL,
  rules_version  TEXT NOT NULL,
  reason         TEXT NOT NULL,
  at_unix        INTEGER NOT NULL,
  PRIMARY KEY (file_id, decision_class)
) STRICT;

CREATE TABLE job (
  id                       TEXT PRIMARY KEY,
  file_id                  INTEGER NOT NULL REFERENCES file(id) ON DELETE CASCADE,
  library_id               TEXT NOT NULL REFERENCES library(id) ON DELETE CASCADE,
  class                    TEXT NOT NULL
                             CHECK (class IN ('Audio','VideoGpu','VideoCpu','Probe','Verify')),
  size_bucket              TEXT NOT NULL CHECK (size_bucket IN ('Small','Medium','Large')),
  state                    TEXT NOT NULL
                             CHECK (state IN ('Pending','Blocked','Eligible','Assigned','Running',
                                              'Verifying','Committing','Retrying','Succeeded',
                                              'Failed','Cancelled','DeadLettered','NeedsOperator')),
  priority                 INTEGER NOT NULL DEFAULT 0,
  order_key                INTEGER NOT NULL DEFAULT 0,
  requirements_json        TEXT NOT NULL,
  -- Paths and byte thresholds are deliberately excluded from the bucket key
  -- (flaw A5); they become per-job admission checks instead, or the key space
  -- explodes and precomputed eligibility becomes useless.
  requirements_bucket_key  TEXT NOT NULL,
  plan_json                TEXT,
  expected_content_sig     TEXT NOT NULL,
  rules_version            TEXT NOT NULL,
  attempt                  INTEGER NOT NULL DEFAULT 0,
  max_attempts             INTEGER NOT NULL DEFAULT 3,
  agent_id                 TEXT REFERENCES agent(id),
  fencing_epoch            INTEGER NOT NULL DEFAULT 0,
  lease_expires_unix       INTEGER,
  not_before_unix          INTEGER,
  excluded_agents_json     TEXT NOT NULL DEFAULT '[]',
  parent_job_id            TEXT REFERENCES job(id),
  input_bytes              INTEGER,
  output_bytes             INTEGER,
  terminal_reason          TEXT,
  created_unix             INTEGER NOT NULL,
  updated_unix             INTEGER NOT NULL,
  started_unix             INTEGER,
  finished_unix            INTEGER
) STRICT;

-- At most one open job per file, enforced by the database rather than by
-- dispatcher discipline. This is what makes double-dispatch structurally
-- impossible instead of merely unlikely.
CREATE UNIQUE INDEX idx_job_open_per_file ON job(file_id)
  WHERE state NOT IN ('Succeeded','Failed','Cancelled','DeadLettered','NeedsOperator');
CREATE INDEX idx_job_ready ON job(state, class, size_bucket, priority DESC, order_key);
CREATE INDEX idx_job_agent ON job(agent_id) WHERE agent_id IS NOT NULL;

-- Append-only transition ledger. Never updated, never deleted for
-- DeadLettered jobs -- it is the record of what actually happened.
CREATE TABLE job_event (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id      TEXT NOT NULL REFERENCES job(id) ON DELETE CASCADE,
  at_unix_ms  INTEGER NOT NULL,
  from_state  TEXT,
  to_state    TEXT NOT NULL,
  agent_id    TEXT,
  attempt     INTEGER NOT NULL DEFAULT 0,
  reason_code TEXT,
  detail      TEXT
) STRICT;

CREATE INDEX idx_job_event_job ON job_event(job_id, at_unix_ms);

CREATE TABLE job_attempt (
  job_id                TEXT NOT NULL REFERENCES job(id) ON DELETE CASCADE,
  attempt               INTEGER NOT NULL,
  agent_id              TEXT,
  agent_uid             TEXT,
  fencing_epoch         INTEGER NOT NULL DEFAULT 0,
  -- Persisted BEFORE exec, so a failure can always be reproduced by pasting
  -- this argv into a shell on the agent.
  argv_json             TEXT,
  decode_strategy_json  TEXT,
  started_unix_ms       INTEGER,
  finished_unix_ms      INTEGER,
  exit_code             INTEGER,
  signal                INTEGER,
  failure_class         TEXT,
  failure_code          TEXT,
  stderr_tail           TEXT,
  progress_summary_json TEXT,
  validation_json       TEXT,
  output_probe_json     TEXT,
  input_bytes           INTEGER,
  output_bytes          INTEGER,
  PRIMARY KEY (job_id, attempt)
) STRICT;

-- The durable ledger covering the replace window. Without it, a lost JobResult
-- after a successful replace causes the next attempt to re-encode a file that
-- has already been replaced.
CREATE TABLE commit_intent (
  id                   TEXT PRIMARY KEY,
  job_id               TEXT NOT NULL REFERENCES job(id) ON DELETE CASCADE,
  attempt              INTEGER NOT NULL,
  agent_id             TEXT NOT NULL,
  agent_uid            TEXT NOT NULL,
  boot_id              TEXT,
  fencing_epoch        INTEGER NOT NULL,
  pool_id              TEXT REFERENCES storage_pool(id),
  source_path          TEXT NOT NULL,
  source_dev           INTEGER,
  source_inode         INTEGER,
  temp_path            TEXT NOT NULL,
  final_path           TEXT NOT NULL,
  trash_path           TEXT,
  phase                TEXT NOT NULL DEFAULT 'Granted'
                         CHECK (phase IN ('Granted','Retired','Installed')),
  state                TEXT NOT NULL DEFAULT 'live'
                         CHECK (state IN ('live','resolved')),
  expected_content_sig TEXT NOT NULL,
  created_unix_ms      INTEGER NOT NULL,
  updated_unix_ms      INTEGER NOT NULL,
  resolved_unix_ms     INTEGER,
  resolution           TEXT
) STRICT;

-- Two agents can never be mid-replace on the same final path. Structural, not
-- advisory: the insert simply fails.
CREATE UNIQUE INDEX idx_commit_intent_live ON commit_intent(final_path)
  WHERE state = 'live';
CREATE INDEX idx_commit_intent_job ON commit_intent(job_id);

CREATE TABLE agent (
  id                   TEXT PRIMARY KEY,
  agent_uid            TEXT NOT NULL,
  boot_id              TEXT,
  hostname             TEXT,
  platform             TEXT,
  arch                 TEXT,
  agent_version        TEXT,
  proto_version        INTEGER,
  ffmpeg_version       TEXT,
  ffprobe_version      TEXT,
  driver_version       TEXT,
  classes_json         TEXT NOT NULL DEFAULT '[]',
  capability_json      TEXT NOT NULL DEFAULT '{}',
  capability_hash      TEXT,
  effective_cores      REAL,
  physical_cores       INTEGER,
  mounts_json          TEXT NOT NULL DEFAULT '[]',
  -- Set by the Phase 0 RenameProbe. A node that cannot rename over an open
  -- destination may produce output but must never install it.
  rename_probe_status  TEXT NOT NULL DEFAULT 'untested'
                         CHECK (rename_probe_status IN ('untested','ok','failed','inconclusive')),
  commit_eligible      INTEGER NOT NULL DEFAULT 0 CHECK (commit_eligible IN (0,1)),
  fencing_epoch        INTEGER NOT NULL DEFAULT 0,
  status               TEXT NOT NULL DEFAULT 'Offline'
                         CHECK (status IN ('Online','Draining','Unhealthy','Offline','Quarantined')),
  admin_state          TEXT NOT NULL DEFAULT 'Enabled'
                         CHECK (admin_state IN ('Enabled','Paused','Drain')),
  quarantine_reason    TEXT,
  connected_since_unix INTEGER,
  last_register_unix   INTEGER,
  last_heartbeat_unix  INTEGER,
  lease_expires_unix   INTEGER
) STRICT;

CREATE TABLE agent_mount (
  agent_id            TEXT NOT NULL REFERENCES agent(id) ON DELETE CASCADE,
  canonical_prefix    TEXT NOT NULL,
  local_path          TEXT NOT NULL,
  pool_id             TEXT REFERENCES storage_pool(id),
  fstype              TEXT,
  free_bytes          INTEGER,
  total_bytes         INTEGER,
  writable            INTEGER NOT NULL DEFAULT 0 CHECK (writable IN (0,1)),
  -- Resolved per (agent, library) against the real final path, not as one
  -- global boolean (flaw D12).
  workarea_same_device INTEGER NOT NULL DEFAULT 0 CHECK (workarea_same_device IN (0,1)),
  observed_unix       INTEGER NOT NULL,
  PRIMARY KEY (agent_id, canonical_prefix)
) STRICT;

CREATE TABLE agent_capability_history (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_id        TEXT NOT NULL REFERENCES agent(id) ON DELETE CASCADE,
  at_unix         INTEGER NOT NULL,
  capability_hash TEXT NOT NULL,
  capability_json TEXT NOT NULL,
  agent_version   TEXT,
  ffmpeg_version  TEXT,
  driver_version  TEXT,
  diff_summary    TEXT
) STRICT;

-- Learned negatives, e.g. "this agent claims NVDEC AV1 but it does not work".
-- Expirable and operator-clearable so a transient fault cannot permanently
-- narrow the fleet (flaw C8).
CREATE TABLE agent_capability_override (
  agent_id     TEXT NOT NULL REFERENCES agent(id) ON DELETE CASCADE,
  requirement  TEXT NOT NULL,
  allowed      INTEGER NOT NULL CHECK (allowed IN (0,1)),
  reason       TEXT,
  evidence     TEXT,
  at_unix      INTEGER NOT NULL,
  expires_unix INTEGER,
  PRIMARY KEY (agent_id, requirement)
) STRICT;

-- Why each queued job did not dispatch last round. Without this, "nothing is
-- running and I don't know why" is an unanswerable question.
CREATE TABLE dispatch_block (
  job_id         TEXT PRIMARY KEY REFERENCES job(id) ON DELETE CASCADE,
  at_unix        INTEGER NOT NULL,
  blocking_stage TEXT NOT NULL,
  detail_json    TEXT
) STRICT;

CREATE TABLE config_revision (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  at_unix       INTEGER NOT NULL,
  actor         TEXT,
  source        TEXT,
  rules_version TEXT,
  toml          TEXT NOT NULL,
  note          TEXT
) STRICT;

CREATE TABLE schedule_window (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  name           TEXT NOT NULL,
  enabled        INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  days_mask      INTEGER NOT NULL DEFAULT 127,
  start_minute   INTEGER NOT NULL,
  end_minute     INTEGER NOT NULL,
  priority       INTEGER NOT NULL DEFAULT 0,
  overrides_json TEXT NOT NULL DEFAULT '{}'
) STRICT;

-- Manual overrides carry a mandatory expiry: a temporary limit change that
-- outlives the operator's memory of making it is a permanent one.
CREATE TABLE schedule_override (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  scope        TEXT NOT NULL,
  class        TEXT,
  slots        INTEGER NOT NULL,
  expires_unix INTEGER NOT NULL,
  actor        TEXT,
  reason       TEXT,
  created_unix INTEGER NOT NULL
) STRICT;

CREATE TABLE scan_run (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  library_id      TEXT NOT NULL REFERENCES library(id) ON DELETE CASCADE,
  mode            TEXT NOT NULL,
  started_unix    INTEGER NOT NULL,
  finished_unix   INTEGER,
  status          TEXT NOT NULL DEFAULT 'running',
  scan_generation INTEGER NOT NULL,
  files_seen      INTEGER NOT NULL DEFAULT 0,
  files_new       INTEGER NOT NULL DEFAULT 0,
  files_changed   INTEGER NOT NULL DEFAULT 0,
  files_missing   INTEGER NOT NULL DEFAULT 0,
  probe_errors    INTEGER NOT NULL DEFAULT 0,
  aborted_reason  TEXT,
  duration_ms     INTEGER
) STRICT;

-- Originals are retained rather than deleted, so a bad decision is
-- recoverable. Reaped against pool pressure with a minimum grace floor.
CREATE TABLE trash_entry (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  file_id          INTEGER REFERENCES file(id) ON DELETE SET NULL,
  job_id           TEXT REFERENCES job(id) ON DELETE SET NULL,
  original_path    TEXT NOT NULL,
  trash_path       TEXT NOT NULL,
  pool_id          TEXT REFERENCES storage_pool(id),
  size_bytes       INTEGER NOT NULL,
  at_unix          INTEGER NOT NULL,
  purge_after_unix INTEGER NOT NULL,
  restored_unix    INTEGER
) STRICT;

CREATE INDEX idx_trash_purge ON trash_entry(purge_after_unix) WHERE restored_unix IS NULL;
