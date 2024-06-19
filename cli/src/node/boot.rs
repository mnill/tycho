use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use everscale_types::models::BlockId;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tycho_block_util::archive::WithArchiveData;
use tycho_block_util::block::{BlockProofStuff, BlockStuff};
use tycho_block_util::state::ShardStateStuff;
use tycho_core::proto::blockchain::BlockFull;
use tycho_storage::{BlockHandle, BriefBlockInfo, KeyBlocksDirection, KEY_BLOCK_UTIME_STEP};
use tycho_util::futures::JoinTask;
use tycho_util::time::now_sec;

use crate::node::Node;

/// Boot type when the node has not yet started syncing
///
/// Returns last masterchain key block id
pub async fn cold_boot(
    node: &Arc<Node>,
    zerostates: Option<Vec<PathBuf>>,
) -> anyhow::Result<BlockId> {
    tracing::info!("starting cold boot");

    // Find the last known key block (or zerostate)
    // from which we can start downloading other key blocks
    let prev_key_block = prepare_prev_key_block(node, zerostates).await?;

    // Ensure that all key blocks until now (with some offset) are downloaded
    download_key_blocks(node, prev_key_block).await?;

    // Choose the latest key block with persistent state
    let last_key_block = choose_key_block(node)?;

    if last_key_block.id().seqno != 0 {
        // If the last suitable key block is not zerostate, we must download all blocks
        // with their states from shards for that
        download_start_blocks_and_states(node, last_key_block.id()).await?;
    };

    tracing::info!("finished cold boot");
    Ok(*last_key_block.id())
}

async fn prepare_prev_key_block(
    node: &Arc<Node>,
    zerostates: Option<Vec<PathBuf>>,
) -> anyhow::Result<PrevKeyBlock> {
    let block_id = node
        .storage
        .node_state()
        .load_init_mc_block_id()
        .unwrap_or(node.zerostate.as_block_id());

    tracing::info!(block_id = %block_id, "using key block");

    let prev_key_block = match block_id.seqno {
        0 => {
            tracing::info!(%block_id, "using zero state");

            let (handle, state) = match zerostates {
                Some(zerostates) => node.import_zerostates(zerostates).await?,
                None => node.download_zerostates().await?,
            };

            PrevKeyBlock::ZeroState {
                handle: Arc::new(handle),
                state: Arc::new(state),
            }
        }
        _ => {
            tracing::info!(%block_id, "using key block");

            let handle = node
                .storage
                .block_handle_storage()
                .load_handle(&block_id)
                .expect("shouldn't happen");

            let proof = node
                .storage
                .block_storage()
                .load_block_proof(&handle, false)
                .await?;

            PrevKeyBlock::KeyBlock {
                handle: Arc::new(handle),
                proof: Box::new(proof),
            }
        }
    };

    Ok(prev_key_block)
}

async fn download_key_blocks(
    node: &Arc<Node>,
    mut prev_key_block: PrevKeyBlock,
) -> anyhow::Result<()> {
    const BLOCKS_PER_BATCH: u32 = 10;
    const PARALLEL_REQUESTS: usize = 10;

    let (ids_tx, mut ids_rx) = mpsc::unbounded_channel();
    let (tasks_tx, mut tasks_rx) = mpsc::unbounded_channel();

    tokio::spawn({
        let blockchain_rpc_client = node.blockchain_rpc_client.clone();

        async move {
            while let Some(block_id) = tasks_rx.recv().await {
                // TODO: add retry count to interrupt infinite loop
                'inner: loop {
                    tracing::debug!(%block_id, "start downloading next key blocks");

                    let res = blockchain_rpc_client
                        .get_next_key_block_ids(&block_id, BLOCKS_PER_BATCH)
                        .await;

                    match res {
                        Ok(res) => {
                            let (handle, data) = res.split();
                            handle.accept();

                            if ids_tx.send(data.block_ids).is_err() {
                                tracing::debug!(%block_id, "stop downloading next key blocks");
                                return;
                            }

                            break 'inner;
                        }
                        Err(e) => {
                            tracing::warn!(%block_id, "failed to download key block ids: {e:?}");
                        }
                    }
                }
            }
        }
    });

    // Start getting next key blocks
    tasks_tx.send(*prev_key_block.handle().id())?;

    while let Some(ids) = ids_rx.recv().await {
        let stream = futures_util::stream::iter(ids)
            .map(|block_id| {
                let storage = node.storage.clone();
                let blockchain_rpc_client = node.blockchain_rpc_client.clone();

                JoinTask::new(async move {
                    let block_storage = storage.block_storage();
                    let block_handle_storage = storage.block_handle_storage();

                    // Check whether block proof is already stored locally
                    if let Some(handle) = block_handle_storage.load_handle(&block_id) {
                        if let Ok(proof) = block_storage.load_block_proof(&handle, false).await {
                            return WithArchiveData::loaded(proof);
                        }
                    }

                    // TODO: add retry count to interrupt infinite loop
                    loop {
                        let res = blockchain_rpc_client
                            .get_key_block_proof(&block_id)
                            .await;

                        match res {
                            Ok(res) => {
                                let (handle, data) = res.split();

                                match BlockProofStuff::deserialize(block_id, &data.data, false) {
                                    Ok(proof) => {
                                        handle.accept();
                                        return proof.with_archive_data(&data.data);
                                    },
                                    Err(e) => {
                                        tracing::error!(%block_id, "failed to deserialize block proof: {e}");
                                        handle.reject();
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(%block_id, "failed to download block proof: {e:?}");
                            }
                        }
                    }
                })
            })
            .buffered(PARALLEL_REQUESTS);

        let mut proofs = stream.collect::<Vec<_>>().await;
        proofs.sort_by_key(|x| *x.id());

        // Save previous key block to restart downloading in case of error
        let fallback_key_block = prev_key_block.clone();

        let proofs_len = proofs.len();
        for (index, proof) in proofs.into_iter().enumerate() {
            // Verify block proof
            match prev_key_block.check_next_proof(&proof.data) {
                Ok(info) => {
                    // Save block proof
                    let block_id = proof.id();

                    let handle = node
                        .storage
                        .block_storage()
                        .store_block_proof(&proof, info.with_mc_seq_no(block_id.seqno).into())
                        .await?
                        .handle;

                    let block_utime = handle.meta().gen_utime();
                    let prev_utime = prev_key_block.handle().meta().gen_utime();

                    // Update init_mc_block_id
                    if tycho_block_util::state::is_persistent_state(block_utime, prev_utime) {
                        node.storage
                            .node_state()
                            .store_init_mc_block_id(handle.id());
                    }

                    // Trigger task to getting next key blocks
                    if index == proofs_len.saturating_sub(1) {
                        tasks_tx.send(*proof.data.id())?;
                    }

                    // Update prev_key_block
                    prev_key_block = PrevKeyBlock::KeyBlock {
                        handle: Arc::new(handle),
                        proof: Box::new(proof.data),
                    };
                }
                Err(e) => {
                    tracing::warn!("got invalid key block proof: {e:?}");

                    // Restart downloading proofs
                    tasks_tx.send(*fallback_key_block.handle().id())?;
                    prev_key_block = fallback_key_block;

                    break;
                }
            }
        }

        let now_utime = now_sec();
        let last_utime = prev_key_block.handle().meta().gen_utime();

        tracing::debug!(
            now_utime,
            last_utime,
            last_known_block_id = %prev_key_block.handle().id(),
        );

        // Prevent infinite key blocks loading
        if last_utime + 2 * KEY_BLOCK_UTIME_STEP > now_utime {
            break;
        }
    }

    Ok(())
}

/// Select the latest suitable key block with persistent state
fn choose_key_block(node: &Node) -> anyhow::Result<BlockHandle> {
    let block_handle_storage = node.storage.block_handle_storage();

    let mut key_blocks = block_handle_storage
        .key_blocks_iterator(KeyBlocksDirection::Backward)
        .map(|block_id| {
            block_handle_storage
                .load_handle(&block_id)
                .context("Key block handle not found")
        })
        .peekable();

    // Iterate all key blocks in reverse order (from the latest to the oldest)
    while let Some(handle) = key_blocks.next().transpose()? {
        let handle_utime = handle.meta().gen_utime();
        let prev_utime = match key_blocks.peek() {
            Some(Ok(prev_block)) => prev_block.meta().gen_utime(),
            Some(Err(e)) => {
                tracing::warn!("failed to load previous key block: {e:?}");
                return Err(BootError::FailedToLoadKeyBlock.into());
            }
            None => 0,
        };

        let is_persistent = prev_utime == 0
            || tycho_block_util::state::is_persistent_state(handle_utime, prev_utime);

        tracing::debug!(
            seq_no = handle.id().seqno,
            is_persistent,
            "new key block candidate",
        );

        // Skip not persistent
        if !is_persistent {
            tracing::debug!("ignoring state: not persistent");
            continue;
        }

        // Skip too new key blocks
        if handle_utime + INTITAL_SYNC_TIME_SECONDS > now_sec() {
            tracing::debug!("ignoring state: too new");
            continue;
        }

        // Use first suitable key block
        tracing::info!(block_id = %handle.id(), "found best key block handle");
        return Ok(handle);
    }

    Err(BootError::PersistentShardStateNotFound.into())
}

async fn download_start_blocks_and_states(
    node: &Arc<Node>,
    mc_block_id: &BlockId,
) -> anyhow::Result<()> {
    // Download and save masterchain block and state
    let (_, init_mc_block) = download_block_with_state(node, *mc_block_id, *mc_block_id).await?;

    tracing::info!(
        block_id = %init_mc_block.id(),
        "downloaded init mc block state"
    );

    // Download and save blocks and states from other shards
    for (_, block_id) in init_mc_block.shard_blocks()? {
        download_block_with_state(node, *mc_block_id, block_id).await?;
    }

    Ok(())
}

async fn download_block_with_state(
    node: &Arc<Node>,
    mc_block_id: BlockId,
    block_id: BlockId,
) -> anyhow::Result<(BlockHandle, BlockStuff)> {
    let block_storage = node.storage.block_storage();
    let block_handle_storage = node.storage.block_handle_storage();

    let mc_seqno = mc_block_id.seqno;

    let handle = block_handle_storage
        .load_handle(&block_id)
        .filter(|handle| handle.meta().has_data());

    // Download block data and proof
    let (block, handle) = match handle {
        Some(handle) => (block_storage.load_block_data(&handle).await?, handle),
        None => {
            let blockchain_rpc_client = &node.blockchain_rpc_client;

            // TODO: add retry count to interrupt infinite loop
            let (block, proof, meta_data) = loop {
                let res = blockchain_rpc_client.get_block_full(&block_id).await;

                match res {
                    Ok(res) => {
                        let (handle, block_full) = res.split();

                        match block_full {
                            BlockFull::Found {
                                block_id,
                                block: block_data,
                                proof: proof_data,
                                is_link,
                            } => {
                                let block = match BlockStuff::deserialize_checked(
                                    block_id,
                                    &block_data,
                                ) {
                                    Ok(block) => WithArchiveData::new(block, block_data),
                                    Err(e) => {
                                        tracing::error!(%block_id, "failed to deserialize block: {e}");
                                        handle.reject();
                                        continue;
                                    }
                                };

                                let proof = match BlockProofStuff::deserialize(
                                    block_id,
                                    &proof_data,
                                    is_link,
                                ) {
                                    Ok(proof) => WithArchiveData::new(proof, proof_data),
                                    Err(e) => {
                                        tracing::error!(%block_id, "failed to deserialize block proof: {e}");
                                        handle.reject();
                                        continue;
                                    }
                                };

                                match proof.pre_check_block_proof() {
                                    Ok((_, block_info)) => {
                                        let meta_data = BriefBlockInfo::from(&block_info)
                                            .with_mc_seq_no(mc_seqno);

                                        break (block, proof, meta_data);
                                    }
                                    Err(e) => {
                                        tracing::error!("received invalid block: {e:?}");
                                    }
                                }
                            }
                            BlockFull::Empty => {
                                tracing::warn!(%block_id, "block not found");
                                handle.reject();
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%block_id, "failed to download block: {e:?}");
                    }
                }
            };

            tracing::info!(%block_id, "downloaded block data");

            let mut handle = block_storage
                .store_block_data(&block, &block.archive_data, meta_data)
                .await?
                .handle;

            if !handle.meta().has_proof() {
                handle = block_storage
                    .store_block_proof(&proof, handle.into())
                    .await?
                    .handle;
            }

            (block.data, handle)
        }
    };

    // Download block state
    if !handle.meta().has_state() {
        let state_update = block.block().load_state_update()?;

        tracing::info!(block_id = %handle.id(), "downloading state");
        let (_, shard_state) = node.load_or_download_state(&block_id).await?;
        tracing::info!(block_id = %handle.id(), "downloaded state");

        let state_hash = *shard_state.root_cell().repr_hash();
        if state_update.new_hash != state_hash {
            return Err(BootError::ShardStateHashMismatch.into());
        }
    }

    Ok((handle, block))
}

#[derive(Clone)]
enum PrevKeyBlock {
    ZeroState {
        handle: Arc<BlockHandle>,
        state: Arc<ShardStateStuff>,
    },
    KeyBlock {
        handle: Arc<BlockHandle>,
        proof: Box<BlockProofStuff>,
    },
}

impl PrevKeyBlock {
    fn handle(&self) -> &Arc<BlockHandle> {
        match self {
            Self::KeyBlock { handle, .. } | Self::ZeroState { handle, .. } => handle,
        }
    }

    fn check_next_proof(&self, next_proof: &BlockProofStuff) -> anyhow::Result<BriefBlockInfo> {
        let (virt_block, virt_block_info) = next_proof
            .pre_check_block_proof()
            .context("Failed to pre check block proof")?;

        let res = BriefBlockInfo::from(&virt_block_info);

        match self {
            // Check block proof with zero state
            PrevKeyBlock::ZeroState { state, .. } => {
                tycho_block_util::block::check_with_master_state(
                    next_proof,
                    state,
                    &virt_block,
                    &virt_block_info,
                )
            }
            // Check block proof with previous key block
            PrevKeyBlock::KeyBlock { proof, .. } => {
                tycho_block_util::block::check_with_prev_key_block_proof(
                    next_proof,
                    proof,
                    &virt_block,
                    &virt_block_info,
                )
            }
        }
        .map(move |_| res)
    }
}

const INTITAL_SYNC_TIME_SECONDS: u32 = 300;

#[derive(thiserror::Error, Debug)]
enum BootError {
    #[error("Failed to load key block")]
    FailedToLoadKeyBlock,
    #[error("Persistent shard state not found")]
    PersistentShardStateNotFound,
    #[error("Downloaded shard state hash mismatch")]
    ShardStateHashMismatch,
}
