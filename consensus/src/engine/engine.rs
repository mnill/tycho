use std::ops::Neg;
use std::sync::Arc;
use std::time::{Duration, Instant};

use everscale_crypto::ed25519::KeyPair;
use itertools::Itertools;
use tokio::sync::mpsc;
use tycho_network::{DhtClient, OverlayService, PeerId};
use tycho_storage::MempoolStorage;
use tycho_util::metrics::HistogramGuard;

use crate::dag::{Dag, DagRound};
use crate::effects::{AltFormat, ChainedRoundsContext, Effects, EngineContext, MempoolStore};
use crate::engine::input_buffer::InputBuffer;
use crate::engine::round_task::RoundTaskReady;
use crate::engine::MempoolConfig;
use crate::intercom::{Dispatcher, PeerSchedule, Responder};
use crate::models::{Point, UnixTime};
use crate::outer_round::{Collator, Commit, Consensus, OuterRound};

pub struct Engine {
    dag: Dag,
    committed: mpsc::UnboundedSender<(Point, Vec<Point>)>,
    peer_schedule: PeerSchedule,
    store: MempoolStore,
    consensus_round: OuterRound<Consensus>,
    commit_round: OuterRound<Commit>,
    round_task: RoundTaskReady,
    effects: Effects<ChainedRoundsContext>,
}

impl Engine {
    pub fn new(
        key_pair: Arc<KeyPair>,
        dht_client: &DhtClient,
        overlay_service: &OverlayService,
        mempool_storage: &MempoolStorage,
        committed: mpsc::UnboundedSender<(Point, Vec<Point>)>,
        collator_round: &OuterRound<Collator>,
        input_buffer: InputBuffer,
    ) -> Self {
        let consensus_round = OuterRound::default();
        let commit_round = OuterRound::default();
        let effects = Effects::<ChainedRoundsContext>::new(consensus_round.get());

        let responder = Responder::default();
        let (dispatcher, overlay) = Dispatcher::new(dht_client, overlay_service, responder.clone());
        let peer_schedule = PeerSchedule::new(key_pair, overlay);

        let store = MempoolStore::new(
            mempool_storage.clone(),
            consensus_round.receiver(),
            commit_round.receiver(),
            collator_round.receiver(),
        );
        let round_task = RoundTaskReady::new(
            &dispatcher,
            &peer_schedule,
            &store,
            &consensus_round,
            collator_round.clone(),
            responder,
            input_buffer,
        );
        tokio::spawn({
            let peer_schedule = peer_schedule.clone();
            async move {
                peer_schedule.run_updater().await;
            }
        });
        Self {
            dag: Dag::new(),
            committed,
            peer_schedule,
            store,
            consensus_round,
            commit_round,
            round_task,
            effects,
        }
    }

    pub fn init_with_genesis(&mut self, current_peers: &[PeerId]) {
        let genesis = crate::test_utils::genesis();
        let entered_span = tracing::error_span!("init engine with genesis").entered();
        // check only genesis round as it is widely used in point validation.
        // if some nodes use distinct genesis data, their first points will be rejected
        assert_eq!(
            genesis.id(),
            crate::test_utils::genesis_point_id(),
            "genesis point id does not match one from config"
        );
        assert!(
            genesis.is_integrity_ok(),
            "genesis point does not pass integrity check"
        );
        assert!(genesis.is_well_formed(), "genesis point is not well formed");

        {
            let mut guard = self.peer_schedule.write();
            let peer_schedule = self.peer_schedule.clone();

            // finished epoch
            guard.set_next_start(MempoolConfig::GENESIS_ROUND, &peer_schedule);
            guard.set_next_peers(
                &[crate::test_utils::genesis_point_id().author],
                &peer_schedule,
                false,
            );
            guard.rotate(&peer_schedule);

            // current epoch
            guard.set_next_start(MempoolConfig::GENESIS_ROUND.next(), &peer_schedule);
            // start updater only after peers are populated into schedule
            guard.set_next_peers(current_peers, &peer_schedule, true);
            guard.rotate(&peer_schedule);
        }

        let current_dag_round = DagRound::genesis(&genesis, &self.peer_schedule);
        let next_dag_round = current_dag_round.next(&self.peer_schedule);

        let genesis_state =
            current_dag_round.insert_exact_sign(&genesis, next_dag_round.key_pair(), &self.store);

        let next_round = next_dag_round.round();

        self.dag.init(current_dag_round, next_dag_round);

        self.round_task.init(next_round, genesis_state);

        self.consensus_round.set_max(next_round);

        drop(entered_span);
        self.effects = Effects::<ChainedRoundsContext>::new(next_round);
    }

    pub async fn run(mut self) -> ! {
        loop {
            let _round_duration = HistogramGuard::begin(EngineContext::ROUND_DURATION);
            let (prev_round_ok, current_dag_round, round_effects) = {
                // do not repeat the `get()` - it can give non-reproducible result
                let consensus_round = self.consensus_round.get();
                let top_dag_round = self.dag.top();
                assert!(
                    consensus_round >= top_dag_round.round(),
                    "consensus round {} cannot be less than top dag round {}",
                    consensus_round.0,
                    top_dag_round.round().0,
                );
                metrics::counter!(EngineContext::ROUNDS_SKIP)
                    .increment((consensus_round.0 - top_dag_round.round().0) as _); // safe

                // `true` if we collected enough dependencies and (optionally) signatures,
                // so `next_dag_round` from the previous loop is the current now
                let prev_round_ok = consensus_round == top_dag_round.round();
                if prev_round_ok {
                    // Round was not advanced by `BroadcastFilter`, and `Collector` gathered enough
                    // broadcasts during previous round. So if `Broadcaster` was run at previous
                    // round (controlled by `Collector`), then it gathered enough signatures.
                    // If our newly created current round repeats such success, then we've moved
                    // consensus with other 2F nodes during previous round
                    // (at this moment we are not sure to be free from network lag).
                    // Then any reliable point received _up to this moment_ should belong to a round
                    // not greater than new `next_dag_round`, and any future round cannot exist.

                    let round_effects =
                        Effects::<EngineContext>::new(&self.effects, consensus_round);
                    (prev_round_ok, top_dag_round, round_effects)
                } else {
                    self.effects = Effects::<ChainedRoundsContext>::new(consensus_round);
                    let round_effects =
                        Effects::<EngineContext>::new(&self.effects, consensus_round);
                    let current_dag_round =
                        self.dag
                            .fill_to_top(consensus_round, &self.peer_schedule, &round_effects);
                    (prev_round_ok, current_dag_round, round_effects)
                }
            };
            metrics::gauge!(EngineContext::CURRENT_ROUND).set(current_dag_round.round().0);

            let next_dag_round = self.dag.fill_to_top(
                current_dag_round.round().next(),
                &self.peer_schedule,
                &round_effects,
            );

            let round_task_run = self
                .round_task
                .run(
                    prev_round_ok,
                    &current_dag_round,
                    &next_dag_round,
                    &round_effects,
                )
                .until_ready();

            let commit_run = tokio::task::spawn_blocking({
                let mut dag = self.dag;
                let next_dag_round = next_dag_round.clone();
                let committed_tx = self.committed.clone();
                let commit_round = self.commit_round.clone();
                let round_effects = round_effects.clone();
                move || {
                    let task_start = Instant::now();
                    let _guard = round_effects.span().enter();

                    let committed = dag.commit(next_dag_round);

                    round_effects.commit_metrics(&committed);
                    round_effects.log_committed(&committed);

                    if let Some(last_anchor_round) =
                        committed.last().map(|(ancor, _)| ancor.round())
                    {
                        commit_round.set_max(last_anchor_round);
                        for points in committed {
                            committed_tx
                                .send(points) // not recoverable
                                .expect("Failed to send anchor commit message tp mpsc channel");
                        }
                        metrics::histogram!(EngineContext::COMMIT_DURATION)
                            .record(task_start.elapsed());
                    }
                    dag
                }
            });

            match tokio::try_join!(round_task_run, commit_run) {
                Ok(((round_task, next_round), dag)) => {
                    self.round_task = round_task;
                    self.consensus_round.set_max(next_round);
                    self.dag = dag;
                }
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => panic!("mempool engine failed: {e:?}"),
            }
        }
    }
}

impl EngineContext {
    const CURRENT_ROUND: &'static str = "tycho_mempool_engine_current_round";
    const ROUNDS_SKIP: &'static str = "tycho_mempool_engine_rounds_skipped";
    const ROUND_DURATION: &'static str = "tycho_mempool_engine_round_time";
    const COMMIT_DURATION: &'static str = "tycho_mempool_engine_commit_time";
}

impl Effects<EngineContext> {
    fn commit_metrics(&self, committed: &[(Point, Vec<Point>)]) {
        metrics::counter!("tycho_mempool_commit_anchors").increment(committed.len() as _);

        if let Some((first_anchor, _)) = committed.first() {
            metrics::gauge!("tycho_mempool_commit_latency_rounds")
                .set(self.depth(first_anchor.round()));
        }
        if let Some((last_anchor, _)) = committed.last() {
            let now = UnixTime::now().as_u64();
            let anchor_time = last_anchor.data().time.as_u64();
            let latency = if now >= anchor_time {
                Duration::from_millis(now - anchor_time).as_secs_f64()
            } else {
                Duration::from_millis(anchor_time - now).as_secs_f64().neg()
            };
            metrics::histogram!("tycho_mempool_commit_anchor_latency_time").record(latency);
        }
    }

    fn log_committed(&self, committed: &[(Point, Vec<Point>)]) {
        if !committed.is_empty() && tracing::enabled!(tracing::Level::DEBUG) {
            let committed = committed
                .iter()
                .map(|(anchor, history)| {
                    let history = history
                        .iter()
                        .map(|point| format!("{:?}", point.id().alt()))
                        .join(", ");
                    format!(
                        "anchor {:?} time {} : [ {history} ]",
                        anchor.id().alt(),
                        anchor.data().time
                    )
                })
                .join("  ;  ");
            tracing::debug!(
                parent: self.span(),
                "committed {committed}"
            );
        }
    }
}
