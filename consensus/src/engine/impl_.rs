use std::mem;
use std::sync::Arc;

use everscale_crypto::ed25519::KeyPair;
use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use futures_util::{future, FutureExt};
use itertools::Itertools;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::Instrument;
use tycho_network::{Network, OverlayService, PeerId, PeerResolver, PrivateOverlay};
use tycho_util::futures::JoinTask;
use tycho_util::metrics::HistogramGuard;

use crate::dag::{Committer, DagFront, DagRound, InclusionState, Verifier};
use crate::effects::{
    AltFormat, ChainedRoundsContext, Effects, EngineContext, MempoolAdapterStore, MempoolStore,
};
use crate::engine::input_buffer::InputBuffer;
use crate::engine::round_task::RoundTaskReady;
use crate::engine::round_watch::{Consensus, RoundWatch, TopKnownAnchor};
use crate::engine::Genesis;
use crate::intercom::{CollectorSignal, Dispatcher, PeerSchedule, Responder};
use crate::models::{AnchorData, CommitResult, Point, PointInfo, Round, UnixTime};

pub struct Engine {
    dag: DagFront,
    committer_run: JoinHandle<Committer>,
    committed_info_tx: mpsc::UnboundedSender<CommitResult>,
    consensus_round: RoundWatch<Consensus>,
    round_task: RoundTaskReady,
    effects: Effects<ChainedRoundsContext>,
    init_task: Option<JoinTask<()>>,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key_pair: Arc<KeyPair>,
        network: &Network,
        peer_resolver: &PeerResolver,
        overlay_service: &OverlayService,
        mempool_adapter_store: &MempoolAdapterStore,
        input_buffer: InputBuffer,
        committed_info_tx: mpsc::UnboundedSender<CommitResult>,
        top_known_anchor: &RoundWatch<TopKnownAnchor>,
        genesis_round: Option<u32>,
    ) -> Self {
        let (genesis, overlay_id) = Genesis::init(
            Round(genesis_round.unwrap_or_default()),
            UnixTime::from_millis(0),
        );

        let consensus_round = RoundWatch::default();
        let effects = Effects::<ChainedRoundsContext>::new(consensus_round.get());
        let responder = Responder::default();

        let private_overlay = PrivateOverlay::builder(overlay_id)
            .with_peer_resolver(peer_resolver.clone())
            .named("tycho-consensus")
            .build(responder.clone());

        overlay_service.add_private_overlay(&private_overlay);

        let dispatcher = Dispatcher::new(network, &private_overlay);
        let peer_schedule = PeerSchedule::new(key_pair, private_overlay);

        peer_schedule.set_epoch(&[Genesis::id().author], Genesis::round(), false);

        genesis.verify_hash().expect("Failed to verify genesis");
        Verifier::verify(&genesis, &peer_schedule).expect("genesis failed to verify");

        let store = MempoolStore::new(
            mempool_adapter_store,
            consensus_round.receiver(),
            top_known_anchor.receiver(),
        );

        let init_task = JoinTask::new({
            let store = store.clone();
            async move {
                let init_storage_task = tokio::task::spawn_blocking({
                    move || store.init_storage(&overlay_id, &genesis)
                });
                match init_storage_task.await {
                    Ok(()) => {}
                    Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                    Err(e) => panic!("failed to clean db on genesis {e:?}"),
                };
            }
        });

        let round_task = RoundTaskReady::new(
            &dispatcher,
            peer_schedule,
            store,
            &consensus_round,
            top_known_anchor.clone(),
            responder,
            input_buffer,
        );

        tokio::spawn({
            let peer_schedule = round_task.state.peer_schedule.clone();
            async move {
                peer_schedule.run_updater().await;
            }
        });

        let committer_run = tokio::spawn({
            let mut top_known_anchor = top_known_anchor.receiver();
            let mut consensus_round = consensus_round.receiver();
            async move {
                // wait both initialized with non-default value to use latest values
                if genesis_round.is_none() {
                    // wait if not set locally
                    _ = top_known_anchor.next().await;
                }
                _ = consensus_round.next().await;

                Committer::default()
            }
        });

        Self {
            dag: DagFront::default(),
            committer_run,
            committed_info_tx,
            consensus_round,
            round_task,
            effects,
            init_task: Some(init_task),
        }
    }

    pub async fn init(&mut self, current_peers: &[PeerId]) {
        if let Some(init_task) = self.init_task.take() {
            init_task.await;
        };

        // note subscribers are created earlier in constructor
        self.round_task
            .state
            .top_known_anchor
            .set_max(Genesis::round());

        self.round_task
            .state
            .peer_schedule
            .set_epoch(current_peers, Genesis::round().next(), true);
    }

    // restore last two rounds into dag, return the last own point among them to repeat broadcast
    async fn pre_run(&mut self) -> Option<(Point, BoxFuture<'static, InclusionState>)> {
        let last_round = tokio::task::spawn_blocking({
            let store = self.round_task.state.store.clone();
            move || store.latest_round()
        })
        .await
        .expect("load last round from db");

        let first_round = Genesis::round().max(Round(last_round.0.saturating_sub(2)));

        self.effects = Effects::<ChainedRoundsContext>::new(last_round);
        let round_effects = Effects::<EngineContext>::new(&self.effects, last_round);

        self.round_task.state.responder.update(
            &self.round_task.state.broadcast_filter,
            None,
            &self.round_task.state.downloader,
            &self.round_task.state.store,
            &round_effects,
        );

        let info_status = tokio::task::spawn_blocking({
            let store = self.round_task.state.store.clone();
            move || store.load_rounds(first_round, last_round)
        })
        .instrument(round_effects.span().clone())
        .await
        .expect("load last info and status from db");

        let _span_guard = round_effects.span().enter();
        assert!(
            info_status.first().map(|(i, _)| i.round())
                <= info_status.last().map(|(i, _)| i.round()),
            "wrong order of data from db on init"
        );

        let start_info = {
            let maybe_unfinished_round = last_round.prev();
            let local_id = {
                // if local id changed, then we can't reuse that old point
                let guard = self.round_task.state.peer_schedule.atomic();
                guard
                    .local_keys(last_round)
                    .or(guard.local_keys(maybe_unfinished_round))
                    .map(|key_pair| PeerId::from(key_pair.public_key))
            };
            match local_id {
                None => None,
                Some(local_id) => info_status
                    .iter()
                    .rev()
                    .filter(|(info, _)| info.data().author == local_id)
                    .take_while(|(info, _)| info.round() >= maybe_unfinished_round)
                    .max_by_key(|(info, _)| info.round())
                    .map(|(info, _)| info)
                    .cloned(),
            }
        };

        // init dag and committer with current bounds, ready to load related stored data

        let start_round = start_info.as_ref().map_or(last_round, |info| info.round());
        // top known block's anchor is not known yet, will shorten dag length later
        self.dag.init(DagRound::new_bottom(
            Consensus::history_bottom(last_round),
            &self.round_task.state.peer_schedule,
        ));
        // commiter must contain the same rounds as front dag
        let mut buf = (self.dag).fill_to_top(start_round, &self.round_task.state.peer_schedule);
        buf.extend_from_slice(self.dag.as_slice());

        let committer_after_watches =
            mem::replace(&mut self.committer_run, tokio::spawn(future::pending()));
        let committer_init = tokio::spawn({
            let store = self.round_task.state.store.clone();
            let peer_schedule = self.round_task.state.peer_schedule.clone();
            let downloader = self.round_task.state.downloader.clone();
            let top_known_anchor = self.round_task.state.top_known_anchor.clone();
            let consensus_round = self.consensus_round.clone();
            let round_effects = round_effects.clone();

            async move {
                let mut committer = match committer_after_watches.await {
                    Ok(committer) => committer,
                    Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                    Err(e) => panic!("default committer after rounds init: {e:?}"),
                };
                committer.extend_from_ahead(&buf, &peer_schedule);

                // watches contain non-default values as committer future is resolved
                // max(): keep in-mem dag as short as possible
                let bottom = TopKnownAnchor::adapter_history_bottom(top_known_anchor.get())
                    .max(Consensus::history_bottom(consensus_round.get()));

                committer.set_bottom(bottom);

                if bottom < last_round {
                    // init commit data with up-to-date data, if such is stored
                    let info_status = tokio::task::spawn_blocking({
                        let store = store.clone();
                        move || store.load_rounds(bottom, last_round)
                    })
                    .instrument(round_effects.span().clone())
                    .await
                    .expect("load last info and status from db");

                    committer.init_at_start(info_status, &downloader, &store, &round_effects);
                }

                committer
            }
        });
        mem::replace(&mut self.committer_run, committer_init).abort();

        // restore last known front dag round

        let dag_round = self.dag.top();
        let mut point_dag_round = dag_round.clone();

        let includes = FuturesUnordered::new();
        for (info, status) in info_status {
            if info.round() > start_round {
                // if info.round() > start_info.round():
                // * either consensus is on the same start_round(+1) and keeps broadcasting points
                // * or consensus advanced further, and there's no need for start_round+1 includes
                break;
            }
            if point_dag_round.round() != info.round() {
                match dag_round.scan(info.round()) {
                    Some(found) => point_dag_round = found,
                    None => panic!("dag was incorrectly extended: cannot restore point info"),
                };
            };
            let inclusion_state = point_dag_round.restore_exact(
                &info,
                status,
                &self.round_task.state.downloader,
                &self.round_task.state.store,
                &round_effects,
            );
            if info.round() == start_round {
                tracing::info!("add inclusion_state {:?}", info.id().alt());
                includes.push(inclusion_state);
            }
        }

        drop(_span_guard);
        let start_point = match start_info {
            None => None,
            Some(start_info) => tokio::task::spawn_blocking({
                let store = self.round_task.state.store.clone();
                move || store.get_point(start_info.round(), start_info.digest())
            })
            .instrument(round_effects.span().clone())
            .await
            .expect("load last own point from db"),
        };

        let start_point_id = start_point.as_ref().map(|point| point.id());
        tracing::info!(
            parent: round_effects.span(),
            start_round = start_round.0,
            start_point = start_point_id.as_ref().map(|point_id| debug(point_id.alt())),
            "pre-run setup completed",
        );
        // engine or collector may jump to later round if it is determined by broadcast filter
        self.consensus_round.set_max(start_round);
        self.round_task.collector.init(start_round, includes);

        // start_point's inclusion state is already pushed into collector, just a stub is enough
        start_point.map(|a| (a, future::pending().boxed()))
    }

    pub async fn run(mut self) -> ! {
        let mut start_point = self.pre_run().await;

        // may be sparse when engine jumped over large amount of rounds
        // TODO new struct in `dag::commit` mod to:
        //  * keep Vec<Vec<DagRound>> for less allocation compared to a flattened Vec<DagRound>
        //  * discard outdated rounds as soon as possible
        //  * somewhat simplify logic of existing Committer parts by moving it to a new part
        let mut rounds_buffer = Vec::new();
        loop {
            let _round_duration = HistogramGuard::begin("tycho_mempool_engine_round_time");
            let (current_dag_round, round_effects) = {
                // do not repeat the `get()` - it can give non-reproducible result
                let consensus_round = self.consensus_round.get();
                let top_dag_round = self.dag.top().clone();
                assert!(
                    consensus_round >= top_dag_round.round(),
                    "consensus round {} cannot be less than top dag round {}",
                    consensus_round.0,
                    top_dag_round.round().0,
                );
                metrics::gauge!("tycho_mempool_engine_rounds_skipped")
                    .increment((consensus_round.0 as f64) - (top_dag_round.round().0 as f64));

                if consensus_round == top_dag_round.round() {
                    let round_effects =
                        Effects::<EngineContext>::new(&self.effects, consensus_round);
                    (top_dag_round, round_effects)
                } else {
                    self.effects = Effects::<ChainedRoundsContext>::new(consensus_round);
                    let round_effects =
                        Effects::<EngineContext>::new(&self.effects, consensus_round);
                    rounds_buffer.append(
                        &mut self
                            .dag
                            .fill_to_top(consensus_round, &self.round_task.state.peer_schedule),
                    );
                    (self.dag.top().clone(), round_effects)
                }
            };
            metrics::gauge!("tycho_mempool_engine_current_round").set(current_dag_round.round().0);

            rounds_buffer.append(&mut self.dag.fill_to_top(
                current_dag_round.round().next(),
                &self.round_task.state.peer_schedule,
            ));
            let next_dag_round = self.dag.top().clone();

            let (collector_signal_tx, collector_signal_rx) = watch::channel(CollectorSignal::Retry);

            let (own_point_fut, own_point_state) = match start_point.take() {
                Some((point, own_point_state)) => {
                    let point_fut = future::ready(Ok(Some(point)));
                    (point_fut.boxed(), own_point_state)
                }
                None => self.round_task.own_point_task(
                    &collector_signal_rx,
                    &current_dag_round,
                    &round_effects,
                ),
            };

            let peer_schedule = self.round_task.state.peer_schedule.clone();
            let top_known_anchor = self.round_task.state.top_known_anchor.get();

            let round_task_run = self
                .round_task
                .run(
                    own_point_fut,
                    own_point_state,
                    collector_signal_tx,
                    collector_signal_rx,
                    &next_dag_round,
                    &round_effects,
                )
                .until_ready();

            // commit may take longer than a round if it ends with a jump to catch up with consensus
            if self.committer_run.is_finished() {
                let mut committer = match self.committer_run.now_or_never() {
                    Some(Ok(committer)) => committer,
                    Some(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                    Some(Err(e)) => panic!("committer task: {e:?}"),
                    None => unreachable!("committer task is finished and can be taken only once"),
                };

                rounds_buffer.extend_from_slice(self.dag.as_slice());

                let moved_rounds_buf = mem::take(&mut rounds_buffer);

                // max(): keep in-mem dag as short as possible; each commit shortens it even more;
                // this bottom is applied only in case of a gap in mempool history
                let bottom_round = TopKnownAnchor::adapter_history_bottom(top_known_anchor)
                    .max(Consensus::history_bottom(current_dag_round.round()));

                let committed_info_tx = self.committed_info_tx.clone();
                let round_effects = round_effects.clone();

                self.committer_run = tokio::task::spawn_blocking(move || {
                    let _guard = round_effects.span().enter();

                    if !committer.set_bottom(bottom_round) {
                        committed_info_tx
                            .send(CommitResult::NewStartAfterGap(bottom_round)) // not recoverable
                            .expect("Failed to send anchor history info to mpsc channel");
                    };

                    committer.extend_from_ahead(&moved_rounds_buf, &peer_schedule);

                    let committed = committer.commit();

                    round_effects.log_committed(&committed);

                    for data in committed {
                        round_effects.commit_metrics(&data.anchor);
                        committed_info_tx
                            .send(CommitResult::Next(data)) // not recoverable
                            .expect("Failed to send anchor history info to mpsc channel");
                    }

                    committer
                });
            }

            match round_task_run.await {
                Ok((round_task, next_round)) => {
                    self.round_task = round_task;
                    self.consensus_round.set_max(next_round);
                }
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => panic!("mempool engine failed: {e:?}"),
            }
        }
    }
}

impl Effects<EngineContext> {
    fn commit_metrics(&self, anchor: &PointInfo) {
        metrics::counter!("tycho_mempool_commit_anchors").increment(1);
        metrics::gauge!("tycho_mempool_commit_latency_rounds").set(self.depth(anchor.round()));
    }

    fn log_committed(&self, committed: &[AnchorData]) {
        if !committed.is_empty() && tracing::enabled!(tracing::Level::DEBUG) {
            let committed = committed
                .iter()
                .map(|data| {
                    let history = data
                        .history
                        .iter()
                        .map(|point| format!("{:?}", point.id().alt()))
                        .join(", ");
                    format!(
                        "anchor {:?} time {} : [ {history} ]",
                        data.anchor.id().alt(),
                        data.anchor.data().time
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
