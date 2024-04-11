use rand::distributions::uniform::{UniformInt, UniformSampler};
use rand::seq::SliceRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use itertools::Itertools;
use tokio::sync::Mutex;
use tycho_network::{OverlayId, PeerId, PublicOverlay};
use crate::overlay_client::settings::NeighboursOptions;

use super::neighbour::{Neighbour, NeighbourOptions};

pub struct NeighbourCollection(pub Arc<Neighbours>);

pub struct Neighbours {
    options: NeighboursOptions,
    entries: Mutex<Vec<Neighbour>>,
    selection_index: Mutex<SelectionIndex>,
    overlay: PublicOverlay,
}

impl Neighbours {
    pub async fn new(
        overlay: PublicOverlay,
        options: NeighboursOptions,
    ) -> Arc<Self> {
        let neighbour_options = NeighbourOptions {
            default_roundtrip_ms: options.default_roundtrip_ms,
        };

        let entries = {
            let entries = overlay.read_entries()
                .choose_multiple(&mut rand::thread_rng(), options.max_neighbours)
                .map(|entry_data| Neighbour::new(entry_data.entry.peer_id, neighbour_options))
                .collect();
            Mutex::new(entries)
        };

        let selection_index = Mutex::new(SelectionIndex::new(options.max_neighbours));

        let result = Self {
            options,
            entries,
            selection_index,
            overlay,
        };

        tracing::info!("Initial update selection call");
        result.update_selection_index().await;
        tracing::info!("Initial update selection finished");

        Arc::new(result)
    }

    pub fn options(&self) -> &NeighboursOptions {
        &self.options
    }

    pub async fn choose(&self) -> Option<Neighbour> {
        self.selection_index
            .lock()
            .await
            .get(&mut rand::thread_rng())
    }



    pub async fn update_selection_index(&self) {
        let mut guard = self.entries.lock().await;
        guard.retain(|x| x.is_reliable());
        let mut lock = self.selection_index.lock().await;
        lock.update(guard.as_slice());
    }

    pub async fn get_sorted_neighbours(&self) ->  Vec<(Neighbour, u32)> {
        let mut index = self.selection_index.lock().await;
        index.indices_with_weights.sort_by(|(ln, lw), (rn, rw) | rw.cmp(lw));
        return Vec::from(index.indices_with_weights.as_slice())
    }

    pub async fn get_active_neighbours_count(&self) -> usize {
        self.entries.lock().await.len()
    }

    // pub async fn get_bad_neighbours_count(&self) -> usize {
    //     let guard = self.entries.lock().await;
    //     guard
    //         .iter()
    //         .filter(|x| !x.is_reliable())
    //         .cloned()
    //         .collect::<Vec<_>>()
    //         .len()
    // }

    pub async fn update(&self, new: Vec<Neighbour>) {
        let mut guard = self.entries.lock().await;
        if guard.len() >= self.options.max_neighbours {
            // or we can alternatively remove the worst node
            drop(guard);
            return
        }

        for n in new {
            if let Some(_) = guard.iter().find(|x| x.peer_id() == n.peer_id()) {
                continue;
            }
            if guard.len() < self.options.max_neighbours {
                guard.push(n)
            } else {
                return;
            }
        }

        // const MINIMAL_NEIGHBOUR_COUNT: usize = 16;
        // let mut guard = self.entries.lock().await;s
        //
        // guard.sort_by(|a, b| a.get_stats().score.cmp(&b.get_stats().score));
        //
        // let mut all_reliable = true;
        //
        // for entry in entries {
        //     if let Some(index) = guard.iter().position(|x| x.peer_id() == entry.peer_id()) {
        //         let nbg = guard.get(index).unwrap();
        //
        //         if !nbg.is_reliable() && guard.len() > MINIMAL_NEIGHBOUR_COUNT {
        //             guard.remove(index);
        //             all_reliable = false;
        //         }
        //     } else {
        //         guard.push(entry.clone());
        //     }
        // }
        //
        // //if everything is reliable then remove the worst node
        // if all_reliable && guard.len() > MINIMAL_NEIGHBOUR_COUNT {
        //     guard.pop();
        // }
        //
        drop(guard);
        self.update_selection_index().await;

    }
}

struct SelectionIndex {
    /// Neighbour indices with cumulative weight.
    indices_with_weights: Vec<(Neighbour, u32)>,
    /// Optional uniform distribution [0; total_weight).
    distribution: Option<UniformInt<u32>>,
}

impl SelectionIndex {
    fn new(capacity: usize) -> Self {
        Self {
            indices_with_weights: Vec::with_capacity(capacity),
            distribution: None,
        }
    }

    fn update(&mut self, neighbours: &[Neighbour]) {
        self.indices_with_weights.clear();
        let mut total_weight = 0;
        for neighbour in neighbours.iter() {
            if let Some(score) = neighbour.compute_selection_score() {
                total_weight += score as u32;
                self.indices_with_weights
                    .push((neighbour.clone(), total_weight));
            }
        }

        self.distribution = if total_weight != 0 {
            Some(UniformInt::new(0, total_weight))
        } else {
            None
        };

        // TODO: fallback to uniform sample from any neighbour
    }

    fn get<R: Rng + ?Sized>(&self, rng: &mut R) -> Option<Neighbour> {
        let chosen_weight = self.distribution.as_ref()?.sample(rng);

        // Find the first item which has a weight higher than the chosen weight.
        let i = self
            .indices_with_weights
            .partition_point(|(_, w)| *w <= chosen_weight);

        self.indices_with_weights
            .get(i)
            .map(|(neighbour, _)| neighbour)
            .cloned()
    }
}

