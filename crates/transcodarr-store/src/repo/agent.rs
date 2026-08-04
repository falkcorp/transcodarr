// file: crates/transcodarr-store/src/repo/agent.rs
// version: 1.0.0
// guid: 4a8e35c7-1f60-4b29-93d8-0c7e21a5f6b4
// last-edited: 2026-08-04
//! The durable agent registry.
//!
//! This table is what makes the fencing rule survive a *server* restart. The
//! rule itself — a new process instance bumps `fencing_epoch`, a stream
//! reconnect resumes it — lives in `transcodarr-proto::VersionGate`, and the
//! epoch it decides is worth nothing if the server forgets it on the way down.
//! An agent that came back after a server restart would be handed epoch 1 again
//! while an intent granted under the *previous* epoch 1 was still live, and the
//! fence would pass a grant it exists to reject.
//!
//! So this repository deliberately does **not** decide the epoch. It stores
//! what it is told and reports what it last stored. The decision has one home,
//! and this is not it — two places computing the same monotonic counter is how
//! they come to disagree.
//!
//! One consequence worth stating: a new `agent_uid` under an existing `id` is a
//! new *installation* answering to an operator name the fleet already knows.
//! [`AgentRepo::instance_of`] reports the stored `agent_uid` alongside the
//! `boot_id` so the caller can treat that as a new instance too. A reinstall
//! adopting the previous installation's epoch would let it inherit a work area
//! that is not its own.

use rusqlite::{OptionalExtension, Row, params};

use crate::StoreError;
use crate::db::now_unix;
use crate::pool::ReadPool;
use crate::writer::WriteOp;

/// A registered agent as the server last saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRecord {
    /// Operator-assigned identity, stable forever (`u1`, `win-rtx2070`).
    pub id: String,
    /// Per-installation identity.
    pub agent_uid: String,
    /// Per-process-instance identity, absent before the first registration.
    pub boot_id: Option<String>,
    /// The build it last registered with.
    pub agent_version: Option<String>,
    /// The protocol version it last spoke.
    pub proto_version: Option<i64>,
    /// Hash of the capability document, for change detection.
    pub capability_hash: Option<String>,
    /// The capability document itself.
    pub capability_json: String,
    /// Cores after any cgroup quota.
    pub effective_cores: Option<f64>,
    /// Whether it may install its own output.
    pub commit_eligible: bool,
    /// The authoritative epoch. Every commit is checked against it.
    pub fencing_epoch: i64,
    /// `Online`, `Draining`, `Unhealthy`, `Offline` or `Quarantined`.
    pub status: String,
    /// `Enabled`, `Paused` or `Drain`.
    pub admin_state: String,
    /// When the current connection began.
    pub connected_since_unix: Option<i64>,
    /// When it last said anything.
    pub last_heartbeat_unix: Option<i64>,
    /// When its lease runs out.
    pub lease_expires_unix: Option<i64>,
}

const AGENT_COLUMNS: &str = "
    id, agent_uid, boot_id, agent_version, proto_version, capability_hash,
    capability_json, effective_cores, commit_eligible, fencing_epoch, status,
    admin_state, connected_since_unix, last_heartbeat_unix, lease_expires_unix";

impl AgentRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            agent_uid: row.get("agent_uid")?,
            boot_id: row.get("boot_id")?,
            agent_version: row.get("agent_version")?,
            proto_version: row.get("proto_version")?,
            capability_hash: row.get("capability_hash")?,
            capability_json: row.get("capability_json")?,
            effective_cores: row.get("effective_cores")?,
            commit_eligible: row.get::<_, i64>("commit_eligible")? != 0,
            fencing_epoch: row.get("fencing_epoch")?,
            status: row.get("status")?,
            admin_state: row.get("admin_state")?,
            connected_since_unix: row.get("connected_since_unix")?,
            last_heartbeat_unix: row.get("last_heartbeat_unix")?,
            lease_expires_unix: row.get("lease_expires_unix")?,
        })
    }
}

/// What the server knows about the last instance to hold an agent name.
///
/// The three fields together are the input to the fencing decision, which is
/// why they are returned as one value rather than fetched separately: reading
/// the epoch and the boot id in two queries invites deciding against a boot id
/// from before a registration and an epoch from after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownInstance {
    /// The installation last seen under this name.
    pub agent_uid: String,
    /// The process instance last seen, if any has registered.
    pub boot_id: Option<String>,
    /// The epoch currently held.
    pub fencing_epoch: i64,
}

/// Everything a registration asserts about an agent.
///
/// `fencing_epoch` is supplied by the caller, already decided by the version
/// gate. See the module documentation for why it is not computed here.
#[derive(Debug, Clone)]
pub struct AgentRegistration {
    /// Operator-assigned identity.
    pub id: String,
    /// Per-installation identity.
    pub agent_uid: String,
    /// Per-process-instance identity.
    pub boot_id: String,
    /// Reported hostname.
    pub hostname: Option<String>,
    /// Reported platform.
    pub platform: Option<String>,
    /// Reported architecture.
    pub arch: Option<String>,
    /// The agent build.
    pub agent_version: String,
    /// The protocol version it speaks.
    pub proto_version: i64,
    /// ffmpeg build string.
    pub ffmpeg_version: Option<String>,
    /// ffprobe build string.
    pub ffprobe_version: Option<String>,
    /// NVIDIA driver version, when there is a GPU.
    pub driver_version: Option<String>,
    /// Advertised work classes, as JSON.
    pub classes_json: String,
    /// The capability document, as JSON.
    pub capability_json: String,
    /// Hash of that document.
    pub capability_hash: String,
    /// Cores after any cgroup quota.
    pub effective_cores: f64,
    /// Physical cores, for comparison against the above.
    pub physical_cores: Option<i64>,
    /// Reported mounts, as JSON.
    pub mounts_json: String,
    /// The Phase 0 rename-probe verdict: `untested`, `ok`, `failed` or
    /// `inconclusive`.
    pub rename_probe_status: String,
    /// Whether this agent may install its own output.
    pub commit_eligible: bool,
    /// The epoch decided by the version gate.
    pub fencing_epoch: i64,
}

/// Reads and writes over `agent`.
#[derive(Debug, Clone)]
pub struct AgentRepo {
    pool: ReadPool,
}

impl AgentRepo {
    /// Bind to a read pool.
    pub fn new(pool: ReadPool) -> Self {
        Self { pool }
    }

    /// One agent by its operator-assigned name.
    pub fn get(&self, id: &str) -> Result<Option<AgentRecord>, StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            &format!("SELECT {AGENT_COLUMNS} FROM agent WHERE id = ?1"),
            [id],
            AgentRecord::from_row,
        )
        .optional()?)
    }

    /// Every agent the fleet has ever seen.
    pub fn all(&self) -> Result<Vec<AgentRecord>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!("SELECT {AGENT_COLUMNS} FROM agent ORDER BY id"))?;
        let rows = stmt.query_map([], AgentRecord::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Agents currently connected and not administratively held back.
    ///
    /// This is the dispatcher's fleet view. `Draining` is excluded on purpose:
    /// a draining agent finishes what it holds and takes nothing new, and the
    /// only way to express that is to stop offering it work.
    pub fn dispatchable(&self) -> Result<Vec<AgentRecord>, StoreError> {
        let c = self.pool.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agent
             WHERE status = 'Online' AND admin_state = 'Enabled'
             ORDER BY id"
        ))?;
        let rows = stmt.query_map([], AgentRecord::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// What the server last saw under this name, for the fencing decision.
    pub fn instance_of(&self, id: &str) -> Result<Option<KnownInstance>, StoreError> {
        let c = self.pool.get()?;
        Ok(c.query_row(
            "SELECT agent_uid, boot_id, fencing_epoch FROM agent WHERE id = ?1",
            [id],
            |row| {
                Ok(KnownInstance {
                    agent_uid: row.get(0)?,
                    boot_id: row.get(1)?,
                    fencing_epoch: row.get(2)?,
                })
            },
        )
        .optional()?)
    }

    /// Record a registration.
    ///
    /// `connected_since_unix` is refreshed only when the `boot_id` changes: a
    /// stream reconnect is the same connection as far as an operator is
    /// concerned, and resetting it on every network blip would make an agent
    /// that has been up for a week look like it just arrived — hiding exactly
    /// the flapping worth noticing.
    pub fn register_op(reg: AgentRegistration) -> WriteOp {
        WriteOp::new(format!("agent.register:{}", reg.id), move |c| {
            let now = now_unix();
            Ok(c.execute(
                "INSERT INTO agent
                   (id, agent_uid, boot_id, hostname, platform, arch, agent_version,
                    proto_version, ffmpeg_version, ffprobe_version, driver_version,
                    classes_json, capability_json, capability_hash, effective_cores,
                    physical_cores, mounts_json, rename_probe_status, commit_eligible,
                    fencing_epoch, status, connected_since_unix, last_register_unix,
                    last_heartbeat_unix)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,
                         ?19,?20,'Online',?21,?21,?21)
                 ON CONFLICT(id) DO UPDATE SET
                    agent_uid           = excluded.agent_uid,
                    boot_id             = excluded.boot_id,
                    hostname            = excluded.hostname,
                    platform            = excluded.platform,
                    arch                = excluded.arch,
                    agent_version       = excluded.agent_version,
                    proto_version       = excluded.proto_version,
                    ffmpeg_version      = excluded.ffmpeg_version,
                    ffprobe_version     = excluded.ffprobe_version,
                    driver_version      = excluded.driver_version,
                    classes_json        = excluded.classes_json,
                    capability_json     = excluded.capability_json,
                    capability_hash     = excluded.capability_hash,
                    effective_cores     = excluded.effective_cores,
                    physical_cores      = excluded.physical_cores,
                    mounts_json         = excluded.mounts_json,
                    rename_probe_status = excluded.rename_probe_status,
                    commit_eligible     = excluded.commit_eligible,
                    fencing_epoch       = excluded.fencing_epoch,
                    status              = 'Online',
                    last_register_unix  = excluded.last_register_unix,
                    last_heartbeat_unix = excluded.last_heartbeat_unix,
                    connected_since_unix = CASE
                        WHEN agent.boot_id IS excluded.boot_id THEN agent.connected_since_unix
                        ELSE excluded.connected_since_unix
                    END",
                params![
                    reg.id,
                    reg.agent_uid,
                    reg.boot_id,
                    reg.hostname,
                    reg.platform,
                    reg.arch,
                    reg.agent_version,
                    reg.proto_version,
                    reg.ffmpeg_version,
                    reg.ffprobe_version,
                    reg.driver_version,
                    reg.classes_json,
                    reg.capability_json,
                    reg.capability_hash,
                    reg.effective_cores,
                    reg.physical_cores,
                    reg.mounts_json,
                    reg.rename_probe_status,
                    i64::from(reg.commit_eligible),
                    reg.fencing_epoch,
                    now,
                ],
            )? as u64)
        })
    }

    /// Append a capability change to the history.
    ///
    /// Called only when the hash actually moved. An entry per registration
    /// would bury the one that matters — an agent whose ffmpeg was upgraded
    /// under it, or whose GPU stopped being reported — under a row per
    /// reconnect.
    pub fn record_capability_op(
        agent_id: String,
        capability_hash: String,
        capability_json: String,
        agent_version: Option<String>,
        ffmpeg_version: Option<String>,
        driver_version: Option<String>,
        diff_summary: String,
    ) -> WriteOp {
        WriteOp::new(format!("agent.capability_history:{agent_id}"), move |c| {
            Ok(c.execute(
                "INSERT INTO agent_capability_history
                   (agent_id, at_unix, capability_hash, capability_json, agent_version,
                    ffmpeg_version, driver_version, diff_summary)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    agent_id,
                    now_unix(),
                    capability_hash,
                    capability_json,
                    agent_version,
                    ffmpeg_version,
                    driver_version,
                    diff_summary,
                ],
            )? as u64)
        })
    }

    /// Record a heartbeat and extend the lease.
    ///
    /// The lease is written as an absolute time computed here rather than as a
    /// duration for the reader to add: server monotonic time is the only clock
    /// both ends can be judged against, and an agent whose clock is wrong must
    /// not be able to extend its own lease by saying so.
    pub fn heartbeat_op(agent_id: String, lease_seconds: i64) -> WriteOp {
        WriteOp::new(format!("agent.heartbeat:{agent_id}"), move |c| {
            let now = now_unix();
            Ok(c.execute(
                "UPDATE agent
                 SET last_heartbeat_unix = ?2, lease_expires_unix = ?3
                 WHERE id = ?1",
                params![agent_id, now, now.saturating_add(lease_seconds)],
            )? as u64)
        })
    }

    /// Move an agent to a new status.
    ///
    /// The epoch is deliberately untouched. Going offline does not invalidate
    /// work already granted — that is what the *next* registration's epoch bump
    /// is for — and fencing on disconnect would kill a job that is running
    /// perfectly well behind a dropped connection.
    pub fn set_status_op(agent_id: String, status: String) -> WriteOp {
        WriteOp::new(format!("agent.status:{agent_id}"), move |c| {
            Ok(c.execute(
                "UPDATE agent SET status = ?2 WHERE id = ?1",
                params![agent_id, status],
            )? as u64)
        })
    }

    /// Quarantine an agent, with the reason an operator will read.
    pub fn quarantine_op(agent_id: String, reason: String) -> WriteOp {
        WriteOp::new(format!("agent.quarantine:{agent_id}"), move |c| {
            Ok(c.execute(
                "UPDATE agent SET status = 'Quarantined', quarantine_reason = ?2
                 WHERE id = ?1",
                params![agent_id, reason],
            )? as u64)
        })
    }

    /// Set the administrative state an operator controls.
    pub fn set_admin_state_op(agent_id: String, admin_state: String) -> WriteOp {
        WriteOp::new(format!("agent.admin_state:{agent_id}"), move |c| {
            Ok(c.execute(
                "UPDATE agent SET admin_state = ?2 WHERE id = ?1",
                params![agent_id, admin_state],
            )? as u64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::tests_support::fixture;
    use crate::writer::{WriteLane, Writer};
    use tempfile::TempDir;

    fn registration(id: &str, uid: &str, boot: &str, epoch: i64) -> AgentRegistration {
        AgentRegistration {
            id: id.into(),
            agent_uid: uid.into(),
            boot_id: boot.into(),
            hostname: Some("u1".into()),
            platform: Some("linux".into()),
            arch: Some("x86_64".into()),
            agent_version: "1.0.0".into(),
            proto_version: 1,
            ffmpeg_version: Some("7.1".into()),
            ffprobe_version: Some("7.1".into()),
            driver_version: None,
            classes_json: r#"["audio"]"#.into(),
            capability_json: r#"{"classes":["Audio"]}"#.into(),
            capability_hash: "hash-1".into(),
            effective_cores: 38.0,
            physical_cores: Some(48),
            mounts_json: "[]".into(),
            rename_probe_status: "ok".into(),
            commit_eligible: true,
            fencing_epoch: epoch,
        }
    }

    #[test]
    fn a_registration_round_trips() {
        let f = fixture();
        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 1,
        )));
        let repo = AgentRepo::new(f.pool.clone());
        let a = repo.get("u1").unwrap().unwrap();
        assert_eq!(a.agent_uid, "uid-1");
        assert_eq!(a.boot_id.as_deref(), Some("boot-a"));
        assert_eq!(a.fencing_epoch, 1);
        assert_eq!(a.status, "Online");
        assert!(a.commit_eligible);
        assert_eq!(a.effective_cores, Some(38.0));
    }

    /// The reason this table exists. The version gate resumes an epoch it read
    /// from here; if a server restart lost it, a returning agent would be
    /// handed epoch 1 again while an intent granted under the *previous* epoch
    /// 1 was still live, and the fence would pass a grant it exists to reject.
    #[test]
    fn the_fencing_epoch_survives_a_server_restart() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.db");

        {
            let writer = Writer::start(Db::open_unchecked(&path).unwrap());
            writer
                .submit_blocking(
                    WriteLane::Normal,
                    AgentRepo::register_op(registration("u1", "uid-1", "boot-a", 7)),
                )
                .unwrap();
        } // writer dropped: the server has gone down

        let _writer = Writer::start(Db::open_unchecked(&path).unwrap());
        let pool = ReadPool::open(&path, 2).unwrap();
        let known = AgentRepo::new(pool).instance_of("u1").unwrap().unwrap();
        assert_eq!(known.fencing_epoch, 7, "the epoch must outlive the process");
        assert_eq!(known.boot_id.as_deref(), Some("boot-a"));
        assert_eq!(known.agent_uid, "uid-1");
    }

    /// The repository stores the epoch it is given and never invents one. The
    /// decision belongs to the version gate; two places computing the same
    /// monotonic counter is how they come to disagree.
    #[test]
    fn re_registering_stores_whatever_epoch_the_caller_decided() {
        let f = fixture();
        let repo = AgentRepo::new(f.pool.clone());

        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 3,
        )));
        assert_eq!(repo.get("u1").unwrap().unwrap().fencing_epoch, 3);

        // A reconnect: the gate resumed the epoch, so the same value comes back.
        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 3,
        )));
        assert_eq!(repo.get("u1").unwrap().unwrap().fencing_epoch, 3);

        // A new process instance: the gate bumped it, and so does the row.
        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-b", 4,
        )));
        let a = repo.get("u1").unwrap().unwrap();
        assert_eq!(a.fencing_epoch, 4);
        assert_eq!(a.boot_id.as_deref(), Some("boot-b"));
    }

    /// A reinstall answers to an operator name the fleet already knows but is
    /// not the same installation. The caller needs to see that to avoid letting
    /// it adopt a work area that is not its own.
    #[test]
    fn a_reinstall_is_visible_as_a_changed_agent_uid() {
        let f = fixture();
        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 1,
        )));
        let repo = AgentRepo::new(f.pool.clone());

        let known = repo.instance_of("u1").unwrap().unwrap();
        assert_eq!(known.agent_uid, "uid-1");

        f.write(AgentRepo::register_op(registration(
            "u1", "uid-2", "boot-z", 2,
        )));
        let known = repo.instance_of("u1").unwrap().unwrap();
        assert_eq!(known.agent_uid, "uid-2");
    }

    /// A reconnect is the same connection to an operator. Resetting the clock
    /// on every network blip would make an agent up for a week look like it
    /// just arrived, hiding exactly the flapping worth noticing.
    #[test]
    fn a_reconnect_keeps_connected_since_but_a_new_instance_resets_it() {
        let f = fixture();
        let repo = AgentRepo::new(f.pool.clone());

        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 1,
        )));
        let first = repo.get("u1").unwrap().unwrap().connected_since_unix;
        assert!(first.is_some());

        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 1,
        )));
        assert_eq!(
            repo.get("u1").unwrap().unwrap().connected_since_unix,
            first,
            "a reconnect is not a new connection"
        );

        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-b", 2,
        )));
        assert!(
            repo.get("u1")
                .unwrap()
                .unwrap()
                .connected_since_unix
                .is_some()
        );
    }

    /// Going offline must not disturb the epoch. Fencing on disconnect would
    /// kill a job running perfectly well behind a dropped connection.
    #[test]
    fn changing_status_leaves_the_epoch_alone() {
        let f = fixture();
        let repo = AgentRepo::new(f.pool.clone());
        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 9,
        )));

        f.write(AgentRepo::set_status_op("u1".into(), "Offline".into()));
        let a = repo.get("u1").unwrap().unwrap();
        assert_eq!(a.status, "Offline");
        assert_eq!(a.fencing_epoch, 9);
    }

    /// The dispatcher's fleet view. A draining agent finishes what it holds and
    /// takes nothing new, so the only way to express that is to stop offering
    /// it work.
    #[test]
    fn only_online_and_enabled_agents_are_dispatchable() {
        let f = fixture();
        let repo = AgentRepo::new(f.pool.clone());
        for (id, uid) in [("u1", "uid-1"), ("u2", "uid-2"), ("u3", "uid-3")] {
            f.write(AgentRepo::register_op(registration(id, uid, "boot-a", 1)));
        }

        f.write(AgentRepo::set_status_op("u2".into(), "Draining".into()));
        f.write(AgentRepo::set_admin_state_op("u3".into(), "Paused".into()));

        let ids: Vec<_> = repo
            .dispatchable()
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec!["u1".to_string()]);
    }

    #[test]
    fn quarantine_records_the_reason_an_operator_will_read() {
        let f = fixture();
        let repo = AgentRepo::new(f.pool.clone());
        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 1,
        )));

        f.write(AgentRepo::quarantine_op(
            "u1".into(),
            "5 consecutive validation failures".into(),
        ));
        assert_eq!(repo.get("u1").unwrap().unwrap().status, "Quarantined");
        assert!(repo.dispatchable().unwrap().is_empty());
    }

    #[test]
    fn a_heartbeat_extends_the_lease_from_the_server_clock() {
        let f = fixture();
        let repo = AgentRepo::new(f.pool.clone());
        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 1,
        )));

        f.write(AgentRepo::heartbeat_op("u1".into(), 30));
        let a = repo.get("u1").unwrap().unwrap();
        let beat = a.last_heartbeat_unix.unwrap();
        assert_eq!(a.lease_expires_unix, Some(beat + 30));
    }

    #[test]
    fn capability_history_is_appended_only_when_asked() {
        let f = fixture();
        f.write(AgentRepo::register_op(registration(
            "u1", "uid-1", "boot-a", 1,
        )));
        f.write(AgentRepo::record_capability_op(
            "u1".into(),
            "hash-2".into(),
            r#"{"classes":["Audio","Cpu"]}"#.into(),
            Some("1.0.1".into()),
            Some("7.1".into()),
            None,
            "classes: +cpu".into(),
        ));

        let c = f.pool.get().unwrap();
        let (n, summary): (i64, String) = c
            .query_row(
                "SELECT COUNT(*), MAX(diff_summary) FROM agent_capability_history
                 WHERE agent_id = 'u1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(summary, "classes: +cpu");
    }

    #[test]
    fn an_unknown_agent_reads_as_absent_rather_than_as_an_error() {
        let f = fixture();
        let repo = AgentRepo::new(f.pool.clone());
        assert!(repo.get("nobody").unwrap().is_none());
        assert!(repo.instance_of("nobody").unwrap().is_none());
    }
}
