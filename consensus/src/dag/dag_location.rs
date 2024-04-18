use std::collections::{btree_map, BTreeMap};
use std::future::Future;
use std::ops::RangeInclusive;
use std::sync::{Arc, OnceLock};

use everscale_crypto::ed25519::KeyPair;
use futures_util::FutureExt;

use tycho_util::futures::{JoinTask, Shared};

use crate::models::{DagPoint, Digest, PointId, Round, Signature, UnixTime, ValidPoint};

/// If DAG location exists, it must have non-empty `versions` map;
///
/// Inclusion state is filled if it belongs to the 2 latest dag rounds
/// and will be used for own point production
///
/// Note methods encapsulate mutability to preserve this invariant, a bit less panics
#[derive(Default)]
pub struct DagLocation {
    // one of the points at current location
    // was proven by the next point of a node;
    // even if we marked this point as invalid, consensus may override our decision
    // and we will have to sync
    /* vertex: Option<Digest>, */
    /// We can sign or reject just a single (e.g. first validated) point at the current location;
    /// other (equivocated) points may be received as includes, witnesses or a proven vertex;
    /// we have to include signed points @ r+0 & @ r-1 as dependencies in our point @ r+1.
    /// Not needed for transitive dependencies.
    state: InclusionState,
    /// only one of the point versions at current location
    /// may become proven by the next round point(s) of a node;
    /// even if we marked a proven point as invalid, consensus may ignore our decision
    versions: BTreeMap<Digest, Shared<JoinTask<DagPoint>>>,
}

impl DagLocation {
    pub fn insert_own_point(&mut self, my_point: &DagPoint) {
        let old = self.versions.insert(
            my_point.digest().clone(),
            Shared::new(JoinTask::new(futures_util::future::ready(my_point.clone()))),
        );
        assert!(
            old.is_none(),
            "Coding error: own point is already inserted into DAG location"
        );
        self.state.insert_own_point(my_point);
    }
    pub fn add_dependency<I, F>(&mut self, digest: &Digest, init: I) -> Shared<JoinTask<DagPoint>>
    where
        I: FnOnce() -> F,
        F: Future<Output = DagPoint> + Send + 'static,
    {
        match self.versions.entry(digest.clone()) {
            btree_map::Entry::Occupied(entry) => entry.get().clone(),
            btree_map::Entry::Vacant(entry) => {
                entry.insert(Shared::new(JoinTask::new(init()))).clone()
            }
        }
    }
    pub fn add_validate<I, F>(
        &mut self,
        digest: &Digest,
        init: I,
    ) -> Option<&'_ Shared<JoinTask<DagPoint>>>
    where
        I: FnOnce() -> F,
        F: Future<Output = DagPoint> + Send + 'static,
    {
        // point that is validated depends on other equivocated points futures (if any)
        // in the same location, so order of insertion matches order of futures' completion;
        // to make signature we are interested in the first validated point only
        // (others are at least suspicious and cannot be signed)
        match self.versions.entry(digest.clone()) {
            btree_map::Entry::Occupied(_) => return None,
            btree_map::Entry::Vacant(entry) => {
                let state = self.state.clone();
                let shared = entry.insert(Shared::new(JoinTask::new({
                    // Note: cannot sign during validation,
                    //  because current DAG round may advance concurrently
                    // TODO either leave output as is and reduce locking in 'inclusion state'
                    //  (as single thread consumes them and makes signature),
                    //  or better add global Watch CurrentDagRound (unify with broadcast filter!)
                    //  and sign inside this future (remove futures unordered in signer)
                    init().inspect(move |dag_point| state.init(dag_point))
                })));
                Some(shared)
            }
        }
    }
    pub fn versions(&self) -> &'_ BTreeMap<Digest, Shared<JoinTask<DagPoint>>> {
        &self.versions
    }
    pub fn state(&self) -> &'_ InclusionState {
        &self.state
    }
}

// Todo remove inner locks and introduce global current dag round watch simultaneously, see Signer
#[derive(Default, Clone)]
pub struct InclusionState(Arc<OnceLock<Signable>>);

impl InclusionState {
    /// Must not be used for downloaded dependencies
    pub fn init(&self, first_completed: &DagPoint) {
        _ = self.0.get_or_init(|| {
            let signed = OnceLock::new();
            if Signable::filter(first_completed).is_none() {
                _ = signed.set(Err(()));
            }
            Signable {
                first_completed: first_completed.clone(),
                signed,
            }
        });
    }
    fn insert_own_point(&self, my_point: &DagPoint) {
        let signed = OnceLock::new();
        match Signable::filter(my_point) {
            None => assert!(false, "Coding error: own point is not signable"),
            Some(valid) => {
                _ = signed.set(Ok(Signed {
                    at: valid.point.body.location.round.clone(),
                    with: valid.point.signature.clone(),
                }))
            }
        };
        let result = self.0.set(Signable {
            first_completed: my_point.clone(),
            signed,
        });
        assert!(
            result.is_ok(),
            "Coding error: own point initialized for inclusion twice"
        )
    }
    pub fn is_empty(&self) -> bool {
        self.0.get().is_none()
    }
    pub fn signable(&self) -> Option<&'_ Signable> {
        self.0.get().filter(|signable| !signable.is_completed())
    }
    pub fn signed(&self) -> Option<&'_ Result<Signed, ()>> {
        self.0.get()?.signed.get()
    }
    pub fn signed_point(&self, at: &Round) -> Option<&'_ ValidPoint> {
        let signable = self.0.get()?;
        if &signable.signed.get()?.as_ref().ok()?.at == at {
            signable.first_completed.valid()
        } else {
            None
        }
    }
    /// only for logging
    pub fn init_id(&self) -> Option<PointId> {
        self.0.get().map(|signable| signable.first_completed.id())
    }
}
#[derive(Debug)]
pub struct Signable {
    first_completed: DagPoint,
    // signature cannot be rolled back, the point must be included as next point dependency
    signed: OnceLock<Result<Signed, ()>>,
}

#[derive(Debug)]
pub struct Signed {
    pub at: Round,
    pub with: Signature,
}

impl Signable {
    pub fn sign(
        &self,
        at: &Round,
        key_pair: Option<&KeyPair>, // same round for own point and next round for other's
        time_range: RangeInclusive<UnixTime>,
    ) -> bool {
        let mut this_call_signed = false;
        if let Some((valid, key_pair)) = Self::filter(&self.first_completed).zip(key_pair) {
            if time_range.contains(&valid.point.body.time) {
                _ = self.signed.get_or_init(|| {
                    this_call_signed = true;
                    Ok(Signed {
                        at: at.clone(),
                        with: valid.point.body.sign(key_pair),
                    })
                });
            } else if &valid.point.body.time < time_range.start() {
                self.reject();
            } // else decide later
        } else {
            self.reject();
        }
        this_call_signed
    }
    pub fn reject(&self) {
        _ = self.signed.set(Err(()));
    }
    fn is_completed(&self) -> bool {
        self.signed.get().is_some()
    }
    fn filter(first_completed: &DagPoint) -> Option<&ValidPoint> {
        match first_completed {
            DagPoint::Trusted(valid) => Some(valid),
            _ => None, // including valid Suspicious
        }
    }
}
