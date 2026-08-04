// file: crates/transcodarr-server/src/fleet.rs
// version: 1.0.0
// guid: 2d9f47b1-8c30-45e6-a719-40b8e3c25d76
// last-edited: 2026-08-04
//! Who is connected right now, and how to reach them.
//!
//! The `agent` table says who the fleet *knows*; this says who is *reachable*,
//! and the two answer different questions. A row survives a reboot; a channel
//! does not, and dispatch needs the second one — placing a job on an agent
//! whose stream closed a second ago means the assignment goes nowhere and the
//! slot stays counted.
//!
//! **One connection per agent, newest wins.** The old stream is closed when a
//! new one arrives rather than both being kept. Two live streams to one agent
//! is the setup for handing the same job to the same node twice and counting it
//! once: the agent would answer on whichever it read first, and the server
//! would be tracking the other. Closing the loser makes that unrepresentable
//! rather than merely unlikely.
//!
//! Sends are bounded and non-blocking. A stalled agent that stops reading must
//! not be able to hold up the dispatch loop that is trying to talk to everyone
//! else — so a full queue is reported as a failed send, and the caller treats
//! that as the agent being gone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tonic::Status;

use transcodarr_proto::pb;

/// How many server messages may be queued for one agent before it is treated as
/// unreachable.
///
/// Small on purpose. The queue is not a buffer to smooth over a slow agent, it
/// is a bound on how far behind the truth an agent may fall before the server
/// stops pretending it is there.
const OUTBOUND_DEPTH: usize = 64;

/// A live connection to one agent.
#[derive(Debug, Clone)]
pub struct Connected {
    /// Operator-assigned identity.
    pub agent_id: String,
    /// The epoch this stream authenticated under.
    ///
    /// Held here as well as in the database so a message arriving on this
    /// stream can be judged without a read: a stream cannot outlive the epoch
    /// it opened with, because a new instance opens a new stream.
    pub fencing_epoch: i64,
    /// Jobs the agent last said it was running.
    pub running: Vec<String>,
    outbound: mpsc::Sender<Result<pb::ServerMessage, Status>>,
}

impl Connected {
    /// Try to send, reporting failure rather than waiting.
    ///
    /// A stalled agent must not hold up the loop talking to everyone else, so a
    /// full queue counts as gone. Better to drop an agent that is not reading
    /// than to let it stall the fleet.
    pub fn send(&self, msg: pb::ServerMessage) -> bool {
        self.outbound.try_send(Ok(msg)).is_ok()
    }
}

/// The registry of connected agents.
///
/// Cheap to clone; every clone shares one map.
#[derive(Debug, Clone, Default)]
pub struct AgentTable {
    inner: Arc<Mutex<HashMap<String, Connected>>>,
}

impl AgentTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connection, displacing any older one for the same agent.
    ///
    /// Returns the sender the stream should hand to tonic. The displaced
    /// connection's channel is dropped, which ends its stream: an agent that
    /// somehow held two would otherwise be a job dispatched twice and counted
    /// once.
    pub fn connect(
        &self,
        agent_id: &str,
        fencing_epoch: i64,
    ) -> mpsc::Receiver<Result<pb::ServerMessage, Status>> {
        let (tx, rx) = mpsc::channel(OUTBOUND_DEPTH);
        let mut map = self.inner.lock().expect("agent table poisoned");
        if let Some(old) = map.insert(
            agent_id.to_string(),
            Connected {
                agent_id: agent_id.to_string(),
                fencing_epoch,
                running: Vec::new(),
                outbound: tx,
            },
        ) {
            tracing::warn!(
                agent = %agent_id,
                old_epoch = old.fencing_epoch,
                new_epoch = fencing_epoch,
                "a second connection displaced an existing one"
            );
        }
        rx
    }

    /// Drop a connection, but only if it is still the one that opened it.
    ///
    /// The guard matters. A slow teardown of a displaced stream would otherwise
    /// remove the *replacement* on its way out, silently disconnecting an agent
    /// that had just successfully reconnected.
    pub fn disconnect(&self, agent_id: &str, fencing_epoch: i64) {
        let mut map = self.inner.lock().expect("agent table poisoned");
        if map
            .get(agent_id)
            .is_some_and(|c| c.fencing_epoch == fencing_epoch)
        {
            map.remove(agent_id);
        }
    }

    /// Record what an agent says it is running.
    pub fn set_running(&self, agent_id: &str, running: Vec<String>) {
        let mut map = self.inner.lock().expect("agent table poisoned");
        if let Some(c) = map.get_mut(agent_id) {
            c.running = running;
        }
    }

    /// One connected agent.
    pub fn get(&self, agent_id: &str) -> Option<Connected> {
        self.inner
            .lock()
            .expect("agent table poisoned")
            .get(agent_id)
            .cloned()
    }

    /// Every connected agent.
    pub fn connected(&self) -> Vec<Connected> {
        let mut all: Vec<_> = self
            .inner
            .lock()
            .expect("agent table poisoned")
            .values()
            .cloned()
            .collect();
        all.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        all
    }

    /// How many agents are reachable.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("agent table poisoned").len()
    }

    /// Whether nobody is connected.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Send to one agent, reporting whether it got as far as the queue.
    pub fn send(&self, agent_id: &str, msg: pb::ServerMessage) -> bool {
        self.get(agent_id).is_some_and(|c| c.send(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revoke(job: &str) -> pb::ServerMessage {
        pb::ServerMessage {
            body: Some(pb::server_message::Body::Revoke(pb::Revoke {
                job_id: job.into(),
                reason: "test".into(),
            })),
        }
    }

    #[tokio::test]
    async fn a_connected_agent_receives_what_is_sent_to_it() {
        let table = AgentTable::new();
        let mut rx = table.connect("u1", 1);
        assert!(table.send("u1", revoke("j1")));
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn sending_to_an_agent_that_is_not_connected_fails_rather_than_waits() {
        let table = AgentTable::new();
        assert!(!table.send("nobody", revoke("j1")));
    }

    /// Two live streams to one agent is the setup for handing the same job to
    /// the same node twice and counting it once.
    #[tokio::test]
    async fn a_second_connection_displaces_the_first() {
        let table = AgentTable::new();
        let mut first = table.connect("u1", 1);
        let mut second = table.connect("u1", 2);

        assert_eq!(table.len(), 1);
        assert!(table.send("u1", revoke("j1")));

        // The replacement got it, and the displaced stream is closed rather
        // than merely idle.
        assert!(second.recv().await.is_some());
        assert!(
            first.recv().await.is_none(),
            "the displaced stream must end, not linger"
        );
    }

    /// A slow teardown of a displaced stream must not remove the replacement on
    /// its way out, or an agent that just reconnected is silently dropped.
    #[tokio::test]
    async fn a_late_disconnect_from_a_displaced_stream_does_not_evict_the_new_one() {
        let table = AgentTable::new();
        let _first = table.connect("u1", 1);
        let _second = table.connect("u1", 2);

        table.disconnect("u1", 1); // the old stream finally notices it is done

        assert_eq!(table.len(), 1, "the replacement must survive");
        assert_eq!(table.get("u1").unwrap().fencing_epoch, 2);
    }

    #[tokio::test]
    async fn a_disconnect_from_the_current_stream_removes_it() {
        let table = AgentTable::new();
        let _rx = table.connect("u1", 3);
        table.disconnect("u1", 3);
        assert!(table.is_empty());
    }

    /// A stalled agent must not be able to hold up the loop talking to everyone
    /// else, so a full queue counts as the agent being gone.
    #[tokio::test]
    async fn a_full_queue_reports_failure_instead_of_blocking() {
        let table = AgentTable::new();
        let _rx = table.connect("u1", 1); // never read from

        let mut sent = 0;
        for i in 0..(OUTBOUND_DEPTH * 2) {
            if table.send("u1", revoke(&format!("j{i}"))) {
                sent += 1;
            }
        }
        assert_eq!(sent, OUTBOUND_DEPTH, "the queue is bounded, and sends fail");
    }

    #[tokio::test]
    async fn the_running_set_is_recorded_per_agent() {
        let table = AgentTable::new();
        let _rx = table.connect("u1", 1);
        table.set_running("u1", vec!["j1".into(), "j2".into()]);
        assert_eq!(table.get("u1").unwrap().running, vec!["j1", "j2"]);
    }
}
