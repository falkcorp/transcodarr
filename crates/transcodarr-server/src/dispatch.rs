// file: crates/transcodarr-server/src/dispatch.rs
// version: 1.0.0
// guid: a3e6702c-9418-4bd5-87f0-6c25d1e938b7
// last-edited: 2026-08-03
//! Deciding which agent runs which job.
//!
//! Matching is two-stage on purpose, and the split is the reason dispatch stays
//! O(agents) rather than O(queue × agents):
//!
//! - **Eligibility** is computed once per *requirement bucket*, not once per
//!   job. Jobs sharing a `bucket_key` are interchangeable as far as agent
//!   capability goes, so the expensive `satisfies` call runs once for the
//!   bucket and the answer is reused. Observed bucket count for this
//!   environment is around eight, against tens of thousands of queued jobs.
//! - **Admission** is per job, and covers exactly what was deliberately kept
//!   out of the bucket key: free bytes, effective cores, and mount coverage.
//!   Those carry per-file numbers and paths, so folding them into the key would
//!   drive it toward one bucket per job and collapse the whole scheme (flaw
//!   A5).
//!
//! Every refusal is recorded rather than dropped. "Nothing is running and I do
//! not know why" is the question the `dispatch_block` table exists to answer,
//! and it can only answer it if the dispatcher says why each time it declines.

use std::collections::HashMap;

use transcodarr_core::capability::{Capability, Requirement, Requirements, satisfies};
use transcodarr_core::facts::SizeBucket;
use transcodarr_core::job::JobClass;

use crate::capacity::{CapacityLedger, Grant, Refusal};

/// What the dispatcher knows about one agent.
#[derive(Debug, Clone)]
pub struct AgentEntry {
    /// Operator-assigned identity.
    pub id: String,
    /// What it can do.
    pub capability: Capability,
    /// Whether it may install its own output.
    ///
    /// A node that failed the Phase 0 `RenameProbe` may produce output but must
    /// never install it — the WSL2/SMB case, decided before any dispatcher
    /// existed to be confused by it.
    pub commit_eligible: bool,
    /// Whether it is accepting work at all.
    pub accepting: bool,
    /// Free bytes in its work area, as last reported.
    pub workarea_free_bytes: u64,
}

/// A job as the dispatcher sees it.
#[derive(Debug, Clone)]
pub struct QueuedJob {
    /// Job identity.
    pub id: String,
    /// What kind of work.
    pub class: JobClass,
    /// Size band.
    pub size_bucket: SizeBucket,
    /// Everything an agent must satisfy.
    pub requirements: Requirements,
    /// Precomputed key over the categorical requirements only.
    pub bucket_key: String,
    /// Agents that have already rejected or failed this job.
    pub excluded_agents: Vec<String>,
}

/// Why a job did not dispatch this round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// Which job.
    pub job_id: String,
    /// The stage that refused it, as recorded on `dispatch_block`.
    pub stage: &'static str,
    /// Something an operator can act on.
    pub detail: String,
}

/// One job placed on one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// Which job.
    pub job_id: String,
    /// Which agent.
    pub agent_id: String,
}

/// What one dispatch pass decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchRound {
    /// Jobs placed.
    pub assignments: Vec<Assignment>,
    /// Jobs that stayed put, and why.
    pub blocked: Vec<Blocked>,
}

/// Matches jobs to agents.
#[derive(Debug, Default)]
pub struct Dispatcher {
    agents: Vec<AgentEntry>,
    /// bucket_key -> agent_id -> whether that agent satisfies the bucket.
    ///
    /// The cache that makes this O(agents) per bucket rather than per job.
    eligibility: HashMap<String, HashMap<String, bool>>,
}

impl Dispatcher {
    /// An empty dispatcher.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the fleet view.
    ///
    /// Clears the eligibility cache: it is keyed by bucket, not by agent, so a
    /// changed capability would otherwise be answered from a stale entry — and
    /// an agent that just lost its GPU would keep being handed GPU work.
    pub fn set_agents(&mut self, agents: Vec<AgentEntry>) {
        self.agents = agents;
        self.eligibility.clear();
    }

    /// Whether an agent satisfies a bucket's requirements, cached.
    fn agent_satisfies(&mut self, bucket_key: &str, agent_idx: usize, reqs: &Requirements) -> bool {
        let agent_id = self.agents[agent_idx].id.clone();
        if let Some(hit) = self
            .eligibility
            .get(bucket_key)
            .and_then(|m| m.get(&agent_id))
        {
            return *hit;
        }
        // Only the categorical requirements decide bucket eligibility; the
        // per-file ones are admission checks and are filtered out here so a
        // cached answer stays valid across every job in the bucket.
        let categorical = Requirements(
            reqs.0
                .iter()
                .filter(|r| r.is_categorical())
                .cloned()
                .collect(),
        );
        let ok = satisfies(&self.agents[agent_idx].capability, &categorical).is_ok();
        self.eligibility
            .entry(bucket_key.to_string())
            .or_default()
            .insert(agent_id, ok);
        ok
    }

    /// Per-job checks deliberately excluded from the bucket key.
    ///
    /// Free bytes, effective cores and mount coverage carry per-file numbers
    /// and paths. Folding them into the key would drive it toward one bucket
    /// per job and collapse the precomputed eligibility to nothing (flaw A5),
    /// so they are checked here, once, against the agent actually chosen.
    fn admits(agent: &AgentEntry, job: &QueuedJob) -> Result<(), String> {
        for req in &job.requirements.0 {
            match req {
                Requirement::MinFreeBytes(need) => {
                    if agent.workarea_free_bytes < *need {
                        return Err(format!(
                            "work area has {} GiB free, job needs {} GiB",
                            agent.workarea_free_bytes / (1024 * 1024 * 1024),
                            need / (1024 * 1024 * 1024)
                        ));
                    }
                }
                Requirement::MinEffectiveCores(need) => {
                    let have = agent.capability.effective_cores;
                    if have < *need {
                        return Err(format!(
                            "agent has {have:.1} effective cores, job needs {need:.1}"
                        ));
                    }
                }
                Requirement::MountCovers(prefix) => {
                    let covered = agent
                        .capability
                        .mounts
                        .iter()
                        .any(|m| prefix.starts_with(&m.canonical_prefix) && m.writable);
                    if !covered {
                        return Err(format!("no writable mount covers {prefix}"));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Place as many queued jobs as capacity and capability allow.
    ///
    /// Jobs are considered in the order given — the caller supplies them
    /// already sorted by priority — and every refusal is recorded, because
    /// "nothing is running and I do not know why" is the question this data
    /// exists to answer.
    pub fn dispatch(&mut self, jobs: &[QueuedJob], ledger: &mut CapacityLedger) -> DispatchRound {
        let mut round = DispatchRound::default();

        for job in jobs {
            let mut best: Option<usize> = None;
            let mut last_refusal: Option<String> = None;
            let mut saw_capable = false;

            for idx in 0..self.agents.len() {
                {
                    let a = &self.agents[idx];
                    if !a.accepting {
                        continue;
                    }
                    // A node that failed the RenameProbe may produce output but
                    // must never install it. Until a produce-only path exists,
                    // it is not a candidate at all -- handing it work it cannot
                    // finish is worse than leaving it idle.
                    if !a.commit_eligible {
                        continue;
                    }
                    if job.excluded_agents.iter().any(|e| e == &a.id) {
                        continue;
                    }
                }

                if !self.agent_satisfies(&job.bucket_key, idx, &job.requirements) {
                    continue;
                }
                saw_capable = true;

                let agent = &self.agents[idx];
                if let Err(why) = Self::admits(agent, job) {
                    last_refusal = Some(why);
                    continue;
                }

                match ledger.try_acquire(
                    &agent.id,
                    &job.id,
                    Grant {
                        class: job.class,
                        size_bucket: job.size_bucket,
                    },
                ) {
                    Ok(()) => {
                        best = Some(idx);
                        break;
                    }
                    Err(r) => last_refusal = Some(refusal_detail(r, &agent.id)),
                }
            }

            match best {
                Some(idx) => round.assignments.push(Assignment {
                    job_id: job.id.clone(),
                    agent_id: self.agents[idx].id.clone(),
                }),
                None => {
                    // The distinction an operator needs: "no agent can ever run
                    // this" is a capability problem to fix, "every capable
                    // agent is busy" is a queue that will drain on its own.
                    let (stage, detail) = if !saw_capable {
                        (
                            "capability",
                            format!(
                                "no enabled, commit-eligible agent satisfies {}",
                                describe(&job.requirements)
                            ),
                        )
                    } else {
                        (
                            "capacity",
                            last_refusal
                                .unwrap_or_else(|| "every capable agent is full".to_string()),
                        )
                    };
                    round.blocked.push(Blocked {
                        job_id: job.id.clone(),
                        stage,
                        detail,
                    });
                }
            }
        }
        round
    }
}

fn refusal_detail(r: Refusal, agent_id: &str) -> String {
    match r {
        Refusal::TotalSlots => format!("{agent_id} is at its total slot limit"),
        Refusal::ClassSlots => format!("{agent_id} is at its limit for this class"),
        Refusal::LargeSlots => format!("{agent_id} is at its large-file limit"),
        Refusal::AlreadyHeld => "this job already holds a slot".to_string(),
        Refusal::UnknownAgent => format!("{agent_id} is not in the ledger"),
    }
}

/// Render requirements for an operator.
///
/// Carrying the *reason* is the point. "No agent available" sends someone
/// hunting; "requires hevc_nvenc, no agent advertises it" is actionable.
fn describe(reqs: &Requirements) -> String {
    let parts: Vec<String> = reqs
        .0
        .iter()
        .filter(|r| r.is_categorical())
        .map(|r| format!("{r:?}"))
        .collect();
    if parts.is_empty() {
        "no categorical requirements".to_string()
    } else {
        parts.join(" + ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capacity::AgentLimits;
    use transcodarr_core::capability::{
        AgentClass, Capability, ContainerId, DecoderCapability, DecoderKind, DecoderStatus,
        DecoderTriple, Mount, Platform, bucket_key,
    };
    use transcodarr_core::plan::{BitDepth, EncoderId};

    fn capability(class: AgentClass, encoders: &[EncoderId], cores: f64) -> Capability {
        Capability {
            platform: Some(Platform::Linux),
            classes: vec![class],
            encoders: encoders.to_vec(),
            muxers: vec![ContainerId::Matroska],
            decoders: vec![DecoderCapability {
                triple: DecoderTriple {
                    codec: "h264".into(),
                    profile: "High".into(),
                    bit_depth: BitDepth::Eight,
                    kind: DecoderKind::Nvdec,
                },
                status: DecoderStatus::VerifiedOk,
                evidence: String::new(),
            }],
            effective_cores: cores,
            mounts: vec![Mount {
                canonical_prefix: "/mnt/bigdata".into(),
                local_path: "/mnt/bigdata".into(),
                writable: true,
            }],
            ..Capability::default()
        }
    }

    fn agent(id: &str, cap: Capability, free: u64) -> AgentEntry {
        AgentEntry {
            id: id.into(),
            capability: cap,
            commit_eligible: true,
            accepting: true,
            workarea_free_bytes: free,
        }
    }

    fn job(id: &str, reqs: Vec<Requirement>, bucket: SizeBucket) -> QueuedJob {
        let requirements = Requirements(reqs);
        QueuedJob {
            id: id.into(),
            class: JobClass::Audio,
            size_bucket: bucket,
            bucket_key: bucket_key(&requirements),
            requirements,
            excluded_agents: vec![],
        }
    }

    fn audio_reqs() -> Vec<Requirement> {
        vec![
            Requirement::AgentClass(AgentClass::Cpu),
            Requirement::Encoder(EncoderId::Eac3),
            Requirement::Muxer(ContainerId::Matroska),
        ]
    }

    fn ledger(slots: u32, large: u32) -> CapacityLedger {
        let mut l = CapacityLedger::new();
        l.set_limits("u1", AgentLimits::flat(slots, large));
        l
    }

    #[test]
    fn a_capable_agent_with_room_gets_the_job() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
            1 << 40,
        )]);
        let mut l = ledger(4, 1);

        let round = d.dispatch(&[job("j1", audio_reqs(), SizeBucket::Small)], &mut l);
        assert_eq!(round.assignments.len(), 1);
        assert_eq!(round.assignments[0].agent_id, "u1");
        assert!(round.blocked.is_empty());
        assert_eq!(l.in_flight("u1"), 1);
    }

    /// "No agent can ever run this" is a capability problem to fix; "every
    /// capable agent is busy" is a queue that drains on its own. An operator
    /// needs to be told which.
    #[test]
    fn an_unsatisfiable_job_is_blocked_on_capability_not_capacity() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
            1 << 40,
        )]);
        let mut l = ledger(4, 1);

        let gpu = vec![
            Requirement::AgentClass(AgentClass::Gpu),
            Requirement::Encoder(EncoderId::HevcNvenc),
        ];
        let round = d.dispatch(&[job("j1", gpu, SizeBucket::Small)], &mut l);
        assert!(round.assignments.is_empty());
        assert_eq!(round.blocked[0].stage, "capability");
        assert!(
            round.blocked[0].detail.contains("HevcNvenc"),
            "the reason must name what is missing: {}",
            round.blocked[0].detail
        );
    }

    #[test]
    fn a_full_agent_blocks_on_capacity_with_a_readable_reason() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
            1 << 40,
        )]);
        let mut l = ledger(1, 1);

        let jobs = vec![
            job("j1", audio_reqs(), SizeBucket::Small),
            job("j2", audio_reqs(), SizeBucket::Small),
        ];
        let round = d.dispatch(&jobs, &mut l);
        assert_eq!(round.assignments.len(), 1);
        assert_eq!(round.blocked.len(), 1);
        assert_eq!(round.blocked[0].stage, "capacity");
        assert!(round.blocked[0].detail.contains("total slot limit"));
    }

    /// A node that failed the RenameProbe may produce output but must never
    /// install it. Handing it work it cannot finish is worse than idling it.
    #[test]
    fn a_commit_ineligible_agent_is_never_chosen() {
        let mut d = Dispatcher::new();
        let mut a = agent(
            "win",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 8.0),
            1 << 40,
        );
        a.commit_eligible = false;
        d.set_agents(vec![a]);
        let mut l = CapacityLedger::new();
        l.set_limits("win", AgentLimits::flat(4, 1));

        let round = d.dispatch(&[job("j1", audio_reqs(), SizeBucket::Small)], &mut l);
        assert!(round.assignments.is_empty());
        assert_eq!(round.blocked[0].stage, "capability");
    }

    #[test]
    fn a_draining_agent_is_not_chosen() {
        let mut d = Dispatcher::new();
        let mut a = agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
            1 << 40,
        );
        a.accepting = false;
        d.set_agents(vec![a]);
        let mut l = ledger(4, 1);

        let round = d.dispatch(&[job("j1", audio_reqs(), SizeBucket::Small)], &mut l);
        assert!(round.assignments.is_empty());
    }

    /// The per-job admission checks -- the ones deliberately kept out of the
    /// bucket key, because they carry per-file numbers and paths.
    #[test]
    fn a_job_needing_more_space_than_the_agent_has_is_refused_on_admission() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
            1_000_000_000, // 1 GB free
        )]);
        let mut l = ledger(4, 1);

        let mut reqs = audio_reqs();
        reqs.push(Requirement::MinFreeBytes(80 * 1024 * 1024 * 1024));
        let round = d.dispatch(&[job("big", reqs, SizeBucket::Large)], &mut l);

        assert!(round.assignments.is_empty());
        assert_eq!(round.blocked[0].stage, "capacity");
        assert!(
            round.blocked[0].detail.contains("free"),
            "detail: {}",
            round.blocked[0].detail
        );
        assert_eq!(l.in_flight("u1"), 0, "a refused job holds nothing");
    }

    #[test]
    fn a_job_needing_more_cores_than_the_agent_has_is_refused() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 4.0),
            1 << 40,
        )]);
        let mut l = ledger(4, 1);

        let mut reqs = audio_reqs();
        reqs.push(Requirement::MinEffectiveCores(13.0));
        let round = d.dispatch(&[job("j1", reqs, SizeBucket::Small)], &mut l);
        assert!(round.assignments.is_empty());
        assert!(round.blocked[0].detail.contains("effective cores"));
    }

    #[test]
    fn a_job_whose_path_is_not_mounted_is_refused() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
            1 << 40,
        )]);
        let mut l = ledger(4, 1);

        let mut reqs = audio_reqs();
        reqs.push(Requirement::MountCovers("/mnt/elsewhere/tv".into()));
        let round = d.dispatch(&[job("j1", reqs, SizeBucket::Small)], &mut l);
        assert!(round.assignments.is_empty());
        assert!(round.blocked[0].detail.contains("no writable mount"));
    }

    /// An agent that already rejected a job must not be handed it again --
    /// that is the requeue bounce the exclusion list exists to stop.
    #[test]
    fn an_excluded_agent_is_skipped() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
            1 << 40,
        )]);
        let mut l = ledger(4, 1);

        let mut j = job("j1", audio_reqs(), SizeBucket::Small);
        j.excluded_agents = vec!["u1".into()];
        let round = d.dispatch(&[j], &mut l);
        assert!(round.assignments.is_empty());
    }

    /// The large-file cap must bind through the dispatcher, not only in the
    /// ledger: the pool is latency-bound and large files starve everything else.
    #[test]
    fn the_large_file_cap_binds_during_dispatch() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
            1 << 40,
        )]);
        let mut l = ledger(8, 1);

        let jobs = vec![
            job("big1", audio_reqs(), SizeBucket::Large),
            job("big2", audio_reqs(), SizeBucket::Large),
            job("small", audio_reqs(), SizeBucket::Small),
        ];
        let round = d.dispatch(&jobs, &mut l);

        let placed: Vec<&str> = round
            .assignments
            .iter()
            .map(|a| a.job_id.as_str())
            .collect();
        assert!(placed.contains(&"big1"));
        assert!(placed.contains(&"small"), "small work must still flow");
        assert!(!placed.contains(&"big2"));
        assert_eq!(round.blocked.len(), 1);
        assert!(round.blocked[0].detail.contains("large-file"));
    }

    /// Jobs sharing a bucket key reuse one eligibility answer. If a changed
    /// fleet did not clear the cache, an agent that just lost its GPU would
    /// keep being handed GPU work.
    #[test]
    fn changing_the_fleet_invalidates_cached_eligibility() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
            1 << 40,
        )]);
        let mut l = ledger(4, 1);
        assert_eq!(
            d.dispatch(&[job("j1", audio_reqs(), SizeBucket::Small)], &mut l)
                .assignments
                .len(),
            1
        );

        // The agent loses the encoder.
        d.set_agents(vec![agent(
            "u1",
            capability(AgentClass::Cpu, &[], 48.0),
            1 << 40,
        )]);
        let round = d.dispatch(&[job("j2", audio_reqs(), SizeBucket::Small)], &mut l);
        assert!(
            round.assignments.is_empty(),
            "a stale cache would place this"
        );
        assert_eq!(round.blocked[0].stage, "capability");
    }

    /// Two agents: the first that is capable and has room takes it, and the
    /// second is used once the first fills.
    #[test]
    fn work_spreads_to_a_second_agent_once_the_first_is_full() {
        let mut d = Dispatcher::new();
        d.set_agents(vec![
            agent(
                "u1",
                capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
                1 << 40,
            ),
            agent(
                "u2",
                capability(AgentClass::Cpu, &[EncoderId::Eac3], 48.0),
                1 << 40,
            ),
        ]);
        let mut l = CapacityLedger::new();
        l.set_limits("u1", AgentLimits::flat(1, 1));
        l.set_limits("u2", AgentLimits::flat(1, 1));

        let jobs = vec![
            job("j1", audio_reqs(), SizeBucket::Small),
            job("j2", audio_reqs(), SizeBucket::Small),
            job("j3", audio_reqs(), SizeBucket::Small),
        ];
        let round = d.dispatch(&jobs, &mut l);
        assert_eq!(round.assignments.len(), 2);
        assert_eq!(round.assignments[0].agent_id, "u1");
        assert_eq!(round.assignments[1].agent_id, "u2");
        assert_eq!(round.blocked.len(), 1);
    }

    /// With no agents at all, every job is blocked on capability and nothing
    /// panics -- the empty-fleet case is a normal startup state.
    #[test]
    fn an_empty_fleet_blocks_everything_without_panicking() {
        let mut d = Dispatcher::new();
        let mut l = CapacityLedger::new();
        let round = d.dispatch(&[job("j1", audio_reqs(), SizeBucket::Small)], &mut l);
        assert!(round.assignments.is_empty());
        assert_eq!(round.blocked[0].stage, "capability");
    }
}
