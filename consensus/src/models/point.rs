use std::collections::BTreeMap;
use std::fmt::{Debug, Display, Formatter};
use std::ops::{Add, Sub};
use std::sync::Arc;

use bytes::Bytes;
use everscale_crypto::ed25519::KeyPair;
use serde::{Deserialize, Serialize};
use sha2::{Digest as Sha2Digest, Sha256};
use tycho_network::PeerId;

use crate::engine::MempoolConfig;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Digest([u8; 32]);

impl Display for Digest {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let len = f.precision().unwrap_or(32);
        for byte in self.0.iter().take(len) {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Debug for Digest {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Digest(")?;
        std::fmt::Display::fmt(self, f)?;
        f.write_str(")")
    }
}

impl Digest {
    fn new(point_body: &PointBody) -> Self {
        let body = bincode::serialize(&point_body).expect("shouldn't happen");
        let mut hasher = Sha256::new();
        hasher.update(body.as_slice());
        Self(hasher.finalize().into())
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Signature(Bytes);

impl Display for Signature {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let len = f.precision().unwrap_or(64);
        for byte in self.0.iter().take(len) {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
impl Debug for Signature {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Signature(")?;
        std::fmt::Display::fmt(self, f)?;
        f.write_str(")")
    }
}

impl Signature {
    pub fn new(local_keypair: &KeyPair, digest: &Digest) -> Self {
        let sig = local_keypair.sign_raw(digest.0.as_slice());
        Self(Bytes::from(sig.to_vec()))
    }

    pub fn verifies(&self, signer: &PeerId, digest: &Digest) -> bool {
        let sig_raw: Result<[u8; 64], _> = self.0.to_vec().try_into();
        sig_raw
            .ok()
            .zip(signer.as_public_key())
            .map_or(false, |(sig_raw, pub_key)| {
                pub_key.verify_raw(digest.0.as_slice(), &sig_raw)
            })
    }
}

#[derive(Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Round(pub u32);

impl Round {
    /// stub that cannot be used even by genesis round
    pub const BOTTOM: Self = Self(0);
    pub fn prev(&self) -> Self {
        self.0
            .checked_sub(1)
            .map(Round)
            .expect("DAG round number underflow, fix dag initial configuration")
    }
    pub fn next(&self) -> Self {
        self.0
            .checked_add(1)
            .map(Round)
            .expect("DAG round number overflow, inner type exhausted")
    }
}

#[derive(Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct UnixTime(u64);

impl UnixTime {
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }
    pub fn now() -> Self {
        Self(
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("current time since unix epoch")
                    .as_millis(),
            )
            .expect("current Unix time in millis as u64"),
        )
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl Add for UnixTime {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl Sub for UnixTime {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl Display for UnixTime {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Copy, Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Location {
    pub round: Round,
    pub author: PeerId,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct PointId {
    pub location: Location,
    pub digest: Digest,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PrevPoint {
    // until weak links are supported,
    // any node may proof its vertex@r-1 with its point@r+0 only
    // pub round: Round,
    pub digest: Digest,
    /// `>= 2F` neighbours, order does not matter;
    /// point author is excluded: everyone must use the proven point to validate its proof
    // Note: bincode may be non-stable on (de)serializing HashMap due to different local order
    pub evidence: BTreeMap<PeerId, Signature>,
    // TODO if we use TL, then every node can sign hash of a point's body (not all body bytes)
    //  so we can include that hash into PrevPoint
    //  to check signatures inside BroadcastFilter::verify() without waiting for DAG
    //  (if that will be fast enough to respond without overlay query timeout)
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub enum Through {
    Witness(PeerId),
    Includes(PeerId),
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub enum Link {
    ToSelf,
    Direct(Through),
    Indirect { to: PointId, path: Through },
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PointBody {
    pub location: Location, // let it be @ r+0
    pub time: UnixTime,
    pub payload: Vec<Bytes>,
    /// by the same author
    pub proof: Option<PrevPoint>,
    /// `>= 2F+1` points @ r-1,
    /// signed by author @ r-1 with some additional points just mentioned;
    /// mandatory includes author's own vertex iff proof is given.
    /// Repeatable order on every node is needed for commit; map is used during validation
    pub includes: BTreeMap<PeerId, Digest>,
    /// `>= 0` points @ r-2, signed by author @ r-1
    /// Repeatable order on every node needed for commit; map is used during validation
    pub witness: BTreeMap<PeerId, Digest>,
    /// last included by author; defines author's last committed anchor
    pub anchor_trigger: Link,
    /// last included by author; maintains anchor chain linked without explicit DAG traverse
    pub anchor_proof: Link,
    /// time of previous anchor candidate
    pub anchor_time: UnixTime,
}

/// Just a field accessor
#[derive(Clone, Copy)]
pub enum LinkField {
    Trigger,
    Proof,
}

// Todo: Arc<Point{...}> => Point(Arc<...{...}>)
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Point {
    pub body: PointBody,
    // hash of the point's body (includes author peer id)
    pub digest: Digest,
    // author's signature for the digest
    pub signature: Signature,
}

impl Point {
    pub fn new(local_keypair: &KeyPair, point_body: PointBody) -> Arc<Self> {
        assert_eq!(
            point_body.location.author,
            PeerId::from(local_keypair.public_key),
            "produced point author must match local key pair"
        );
        let digest = Digest::new(&point_body);
        Arc::new(Point {
            body: point_body,
            signature: Signature::new(local_keypair, &digest),
            digest,
        })
    }

    pub fn id(&self) -> PointId {
        PointId {
            location: self.body.location.clone(),
            digest: self.digest.clone(),
        }
    }

    pub fn prev_id(&self) -> Option<PointId> {
        let Some(digest) = self.body.proof.as_ref().map(|p| &p.digest) else {
            return None;
        };
        Some(PointId {
            location: Location {
                round: self.body.location.round.prev(),
                author: self.body.location.author,
            },
            digest: digest.clone(),
        })
    }

    /// Failed integrity means the point may be created by someone else.
    /// blame every dependent point author and the sender of this point,
    /// do not use the author from point's body
    pub fn is_integrity_ok(&self) -> bool {
        self.signature
            .verifies(&self.body.location.author, &self.digest)
            && self.digest == Digest::new(&self.body)
    }

    /// blame author and every dependent point's author
    /// must be checked right after integrity, before any manipulations with the point
    pub fn is_well_formed(&self) -> bool {
        // any genesis is suitable, round number may be taken from configs
        let author = &self.body.location.author;
        let is_time_ok = self.body.time >= self.body.anchor_time;
        let is_special_ok = match self.body.location.round {
            MempoolConfig::GENESIS_ROUND => {
                self.body.includes.is_empty()
                    && self.body.witness.is_empty()
                    && self.body.payload.is_empty()
                    && self.body.proof.is_none()
                    && self.body.anchor_proof == Link::ToSelf
                    && self.body.anchor_trigger == Link::ToSelf
            }
            round if round > MempoolConfig::GENESIS_ROUND => {
                // no witness is possible at the round right after genesis;
                // the other way: we may panic on round.prev().prev() while extracting link's round
                (round > MempoolConfig::GENESIS_ROUND.next() || self.body.witness.is_empty())
                // leader must maintain its chain of proofs,
                // while others must link to previous points (checked at the end of this method);
                // its decided later (using dag round data) whether current point belongs to leader
                && !(self.body.anchor_proof == Link::ToSelf && self.body.proof.is_none())
                && !(self.body.anchor_trigger == Link::ToSelf && self.body.proof.is_none())
            }
            _ => false,
        };
        is_time_ok && is_special_ok
            // proof is listed in includes - to count for 2/3+1, verify and commit dependencies
            && self.body.proof.as_ref().map(|p| &p.digest) == self.body.includes.get(&author)
            // in contrast, evidence must contain only signatures of others
            && self.body.proof.as_ref().map_or(true, |p| !p.evidence.contains_key(author))
            && self.is_link_well_formed(LinkField::Proof)
            && self.is_link_well_formed(LinkField::Trigger)
            && match (self.anchor_round(LinkField::Proof), self.anchor_round(LinkField::Trigger)) {
                (x, MempoolConfig::GENESIS_ROUND) => x >= MempoolConfig::GENESIS_ROUND,
                (MempoolConfig::GENESIS_ROUND, y) => y >= MempoolConfig::GENESIS_ROUND,
                // equality is impossible due to commit waves do not start every round;
                // anchor trigger may belong to a later round than proof and vice versa;
                // no indirect links over genesis tombstone
                (x, y) => x != y && x > MempoolConfig::GENESIS_ROUND && y > MempoolConfig::GENESIS_ROUND,
            }
    }

    fn is_link_well_formed(&self, link_field: LinkField) -> bool {
        match self.anchor_link(link_field) {
            Link::ToSelf => true,
            Link::Direct(Through::Includes(peer)) => self.body.includes.contains_key(peer),
            Link::Direct(Through::Witness(peer)) => self.body.witness.contains_key(peer),
            Link::Indirect {
                path: Through::Includes(peer),
                to,
            } => {
                self.body.includes.contains_key(peer)
                    && to.location.round.next() < self.body.location.round
            }
            Link::Indirect {
                path: Through::Witness(peer),
                to,
            } => {
                self.body.witness.contains_key(peer)
                    && to.location.round.next().next() < self.body.location.round
            }
        }
    }

    pub fn anchor_link(&self, link_field: LinkField) -> &'_ Link {
        match link_field {
            LinkField::Trigger => &self.body.anchor_trigger,
            LinkField::Proof => &self.body.anchor_proof,
        }
    }

    pub fn anchor_round(&self, link_field: LinkField) -> Round {
        match self.anchor_link(link_field) {
            Link::ToSelf => self.body.location.round,
            Link::Direct(Through::Includes(_)) => self.body.location.round.prev(),
            Link::Direct(Through::Witness(_)) => self.body.location.round.prev().prev(),
            Link::Indirect { to, .. } => to.location.round,
        }
    }

    /// the final destination of an anchor link
    pub fn anchor_id(&self, link_field: LinkField) -> PointId {
        match self.anchor_link(link_field) {
            Link::Indirect { to, .. } => to.clone(),
            _direct => self.anchor_link_id(link_field),
        }
    }

    /// next point in path from `&self` to the anchor
    pub fn anchor_link_id(&self, link_field: LinkField) -> PointId {
        let (peer, is_in_includes) = match self.anchor_link(link_field) {
            Link::ToSelf => return self.id(),
            Link::Direct(Through::Includes(peer)) => (peer, true),
            Link::Direct(Through::Witness(peer)) => (peer, false),
            Link::Indirect {
                path: Through::Includes(peer),
                ..
            } => (peer, true),
            Link::Indirect {
                path: Through::Witness(peer),
                ..
            } => (peer, false),
        };

        let (map, round) = if is_in_includes {
            (&self.body.includes, self.body.location.round.prev())
        } else {
            (&self.body.witness, self.body.location.round.prev().prev())
        };
        PointId {
            location: Location {
                round,
                author: *peer,
            },
            digest: map
                .get(peer)
                .expect("Coding error: usage of ill-formed point")
                .clone(),
        }
    }
}
