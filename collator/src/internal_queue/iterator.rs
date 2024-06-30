use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::Arc;

use anyhow::{bail, Result};
use everscale_types::models::{IntAddr, ShardIdent};
use tycho_util::FastHashMap;

use crate::internal_queue::error::QueueError;
use crate::internal_queue::state::state_iterator::{IterRange, MessageWithSource, ShardRange};
use crate::internal_queue::state::states_iterators_manager::StatesIteratorsManager;
use crate::internal_queue::types::{EnqueuedMessage, InternalMessageKey, QueueDiff};

pub trait QueueIterator: Send {
    /// Get next message
    fn next(&mut self, with_new: bool) -> Result<Option<IterItem>>;
    /// Take diff from iterator
    /// Move current position to commited position
    /// Create new transaction
    fn take_diff(&mut self) -> QueueDiff;
    /// Commit processed messages
    /// It's getting last message position for each shard and save
    fn commit(&mut self, messages: Vec<(ShardIdent, InternalMessageKey)>) -> Result<()>;
    /// Add new message to iterator
    fn add_message(&mut self, message: Arc<EnqueuedMessage>) -> Result<()>;
    /// Fill processed upto from iterator
    fn fill_processed_upto(&mut self);
}

pub struct QueueIteratorImpl {
    for_shard: ShardIdent,
    current_position: BTreeMap<ShardIdent, InternalMessageKey>,
    commited_current_position: BTreeMap<ShardIdent, InternalMessageKey>,
    messages_for_current_shard: BinaryHeap<Reverse<Arc<MessageWithSource>>>,
    new_messages: FastHashMap<InternalMessageKey, Arc<EnqueuedMessage>>,
    snapshot_manager: StatesIteratorsManager,
}

impl QueueIteratorImpl {
    pub fn new(
        snapshot_manager: StatesIteratorsManager,
        for_shard: ShardIdent,
    ) -> Result<Self, QueueError> {
        let messages_for_current_shard = BinaryHeap::default();

        Ok(Self {
            for_shard,
            messages_for_current_shard,
            current_position: Default::default(),
            new_messages: Default::default(),
            commited_current_position: Default::default(),
            snapshot_manager,
        })
    }
}

pub struct IterItem {
    pub message_with_source: Arc<MessageWithSource>,
    pub is_new: bool,
}

fn update_shard_range(
    touched_shards: &mut FastHashMap<ShardIdent, ShardRange>,
    shard_id: ShardIdent,
    from: Option<InternalMessageKey>,
    to: Option<InternalMessageKey>,
) {
    touched_shards
        .entry(shard_id)
        .or_insert_with(|| ShardRange { shard_id, from, to });
}

impl QueueIterator for QueueIteratorImpl {
    fn next(&mut self, with_new: bool) -> Result<Option<IterItem>> {
        loop {
            if let Some(next_message) = self.snapshot_manager.next()? {
                let dst = match &next_message.message.info.dst {
                    IntAddr::Std(dst) => dst,
                    IntAddr::Var(_) => {
                        panic!("invalid destination address")
                    }
                };

                if self.for_shard.contains_account(&dst.address)
                    && self.for_shard.workchain() == dst.workchain as i32
                {
                    return Ok(Some(IterItem {
                        message_with_source: next_message.clone(),
                        is_new: false,
                    }));
                } else {
                    self.commited_current_position
                        .entry(next_message.shard_id)
                        .and_modify(|e| {
                            if next_message.message.key() > *e {
                                *e = next_message.message.key().clone();
                            }
                        })
                        .or_insert(next_message.message.key().clone());
                    continue;
                }
            } else {
                break;
            }
        }

        if with_new {
            if let Some(next_message) = self.messages_for_current_shard.pop() {
                let message_key = next_message.0.message.key();

                if self.new_messages.contains_key(&message_key) {
                    return Ok(Some(IterItem {
                        message_with_source: next_message.0.clone(),
                        is_new: true,
                    }));
                } else {
                    bail!(
                        "Message is not in new messages but in current shard messages: {:?}",
                        message_key
                    );
                }
            }
        }

        Ok(None)
    }

    fn take_diff(&mut self) -> QueueDiff {
        tracing::trace!(
            target: crate::tracing_targets::MQ,
            "Taking diff from iterator. New messages count: {}",
            self.new_messages.len());

        let mut diff = QueueDiff::default();

        for (shard_id, lt) in self.commited_current_position.iter() {
            diff.processed_upto.insert(*shard_id, lt.clone());
        }

        let current_shard_processed_upto = self
            .commited_current_position
            .get(&self.for_shard)
            .cloned()
            .unwrap_or_default();

        let amount_before = self.new_messages.len();

        let mut inserted_new_messages = 0;

        tracing::debug!(target: crate::tracing_targets::MQ, "Current shard processed upto: {:?}",current_shard_processed_upto);

        for message in self.new_messages.values() {
            let (dest_workchain, dest_account) = message.destination().unwrap();
            if self.for_shard.contains_account(&dest_account)
                && self.for_shard.workchain() == dest_workchain as i32
            {
                if message.key() > current_shard_processed_upto {
                    diff.messages.insert(message.key(), message.clone());
                    inserted_new_messages += 1;
                }
            } else {
                diff.messages.insert(message.key(), message.clone());
                inserted_new_messages += 1;
            }
        }

        tracing::trace!(
            target: crate::tracing_targets::MQ,
            "Inserted {} messages out of {} to diff",
            inserted_new_messages,
            amount_before);

        self.current_position
            .clone_from(&self.commited_current_position);
        diff
    }

    fn commit(&mut self, messages: Vec<(ShardIdent, InternalMessageKey)>) -> Result<()> {
        tracing::debug!(
            target: crate::tracing_targets::MQ,
            "Committing messages to the iterator. Messages count: {}",
            messages.len());

        for message in messages {
            if let Some(current_key) = self.commited_current_position.get_mut(&message.0) {
                if message.1 > *current_key {
                    current_key.clone_from(&message.1);
                }
            } else {
                self.commited_current_position.insert(message.0, message.1);
            }
        }
        Ok(())
    }

    fn add_message(&mut self, message: Arc<EnqueuedMessage>) -> Result<()> {
        self.new_messages.insert(message.key(), message.clone());
        let (dest_workchain, dest_account) = message.destination()?;
        if self.for_shard.contains_account(&dest_account)
            && self.for_shard.workchain() == dest_workchain as i32
        {
            let message_with_source = MessageWithSource::new(self.for_shard, message.clone());
            self.messages_for_current_shard
                .push(Reverse(Arc::new(message_with_source)));
        };
        Ok(())
    }

    fn fill_processed_upto(&mut self) {
        let read_uptos = self.snapshot_manager.get_iter_upto();
        for read_upto in read_uptos.iter() {
            if let None = self.commited_current_position.get_mut(read_upto.0) {
                self.commited_current_position
                    .insert(*read_upto.0, read_upto.1.clone());
            }
        }
    }
}

fn find_common_ancestor(shard1: ShardIdent, shard2: ShardIdent) -> Option<ShardIdent> {
    if shard1.is_ancestor_of(&shard2) {
        Some(shard1)
    } else if shard2.is_ancestor_of(&shard1) {
        Some(shard2)
    } else {
        None
    }
}

pub struct QueueIteratorExt;

impl QueueIteratorExt {
    pub fn collect_ranges(
        shards_from: FastHashMap<ShardIdent, InternalMessageKey>,
        shards_to: FastHashMap<ShardIdent, InternalMessageKey>,
    ) -> FastHashMap<ShardIdent, ShardRange> {
        let mut shards_with_ranges = FastHashMap::default();
        for from in shards_from {
            for to in &shards_to {
                let iter_range_from = IterRange {
                    shard_id: from.0,
                    key: from.1.clone(),
                };
                let iter_range_to = IterRange {
                    shard_id: *to.0,
                    key: to.1.clone(),
                };
                Self::traverse_and_collect_ranges(
                    &mut shards_with_ranges,
                    &iter_range_from,
                    &iter_range_to,
                );
            }
        }

        shards_with_ranges
    }

    pub fn traverse_and_collect_ranges(
        touched_shards: &mut FastHashMap<ShardIdent, ShardRange>,
        from_range: &IterRange,
        to_range: &IterRange,
    ) {
        if from_range.shard_id == to_range.shard_id
            || from_range.shard_id.intersects(&to_range.shard_id)
        {
            update_shard_range(
                touched_shards,
                from_range.shard_id,
                Some(from_range.key.clone()),
                Some(to_range.key.clone()),
            );
        } else if from_range.shard_id.is_parent_of(&to_range.shard_id)
            || from_range.shard_id.is_child_of(&to_range.shard_id)
        {
            update_shard_range(
                touched_shards,
                from_range.shard_id,
                Some(from_range.key.clone()),
                None,
            );
            update_shard_range(
                touched_shards,
                to_range.shard_id,
                None,
                Some(to_range.key.clone()),
            );
        }

        if let Some(common_ancestor) = find_common_ancestor(from_range.shard_id, to_range.shard_id)
        {
            update_shard_range(
                touched_shards,
                from_range.shard_id,
                Some(from_range.key.clone()),
                None,
            );
            update_shard_range(
                touched_shards,
                to_range.shard_id,
                None,
                Some(to_range.key.clone()),
            );

            let mut current_shard = if from_range.shard_id.is_ancestor_of(&to_range.shard_id) {
                to_range.shard_id
            } else {
                from_range.shard_id
            };

            while current_shard != common_ancestor {
                if let Some(parent_shard) = current_shard.merge() {
                    update_shard_range(touched_shards, parent_shard, None, None);
                    current_shard = parent_shard;
                } else {
                    break;
                }
            }
        }
    }
}
