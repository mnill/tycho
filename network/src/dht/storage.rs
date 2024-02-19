use std::cell::RefCell;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use moka::sync::{Cache, CacheBuilder};
use moka::Expiry;
use tl_proto::TlWrite;
use tycho_util::time::now_sec;

use crate::proto::dht::{OverlayValue, OverlayValueRef, PeerValueRef, ValueRef};

type DhtCache<S> = Cache<StorageKeyId, StoredValue, S>;
type DhtCacheBuilder<S> = CacheBuilder<StorageKeyId, StoredValue, DhtCache<S>>;

pub trait OverlayValueMerger: Send + Sync + 'static {
    fn check_value(&self, new: &OverlayValueRef<'_>) -> Result<(), StorageError>;
    fn merge_value(&self, new: &OverlayValueRef<'_>, stored: &mut OverlayValue) -> bool;
}

impl OverlayValueMerger for () {
    fn check_value(&self, _new: &OverlayValueRef<'_>) -> Result<(), StorageError> {
        Err(StorageError::InvalidKey)
    }
    fn merge_value(&self, _new: &OverlayValueRef<'_>, _stored: &mut OverlayValue) -> bool {
        false
    }
}

pub(crate) struct StorageBuilder {
    cache_builder: DhtCacheBuilder<std::hash::RandomState>,
    overlay_value_merger: Weak<dyn OverlayValueMerger>,
    max_ttl: Duration,
}

impl Default for StorageBuilder {
    fn default() -> Self {
        Self {
            cache_builder: Default::default(),
            overlay_value_merger: Weak::<()>::new(),
            max_ttl: Duration::from_secs(3600),
        }
    }
}

impl StorageBuilder {
    pub fn build(self) -> Storage {
        fn weigher(_key: &StorageKeyId, value: &StoredValue) -> u32 {
            std::mem::size_of::<StorageKeyId>() as u32
                + std::mem::size_of::<StoredValue>() as u32
                + value.data.len() as u32
        }

        Storage {
            cache: self
                .cache_builder
                .time_to_live(self.max_ttl)
                .weigher(weigher)
                .expire_after(ValueExpiry)
                .build_with_hasher(ahash::RandomState::default()),
            overlay_value_merger: self.overlay_value_merger,
            max_ttl_sec: self.max_ttl.as_secs().try_into().unwrap_or(u32::MAX),
        }
    }

    pub fn with_overlay_value_merger(mut self, merger: &Arc<dyn OverlayValueMerger>) -> Self {
        self.overlay_value_merger = Arc::downgrade(merger);
        self
    }

    pub fn with_max_capacity(mut self, max_capacity: u64) -> Self {
        self.cache_builder = self.cache_builder.max_capacity(max_capacity);
        self
    }

    pub fn with_max_ttl(mut self, ttl: Duration) -> Self {
        self.max_ttl = ttl;
        self
    }

    pub fn with_max_idle(mut self, duration: Duration) -> Self {
        self.cache_builder = self.cache_builder.time_to_idle(duration);
        self
    }
}

pub(crate) struct Storage {
    cache: DhtCache<ahash::RandomState>,
    overlay_value_merger: Weak<dyn OverlayValueMerger>,
    max_ttl_sec: u32,
}

impl Storage {
    pub fn builder() -> StorageBuilder {
        StorageBuilder::default()
    }

    pub fn get(&self, key: &[u8; 32]) -> Option<Bytes> {
        let stored_value = self.cache.get(key)?;
        (stored_value.expires_at > now_sec()).then_some(stored_value.data)
    }

    pub fn insert(&self, value: &ValueRef<'_>) -> Result<bool, StorageError> {
        match value.expires_at().checked_sub(now_sec()) {
            Some(0) | None => return Err(StorageError::ValueExpired),
            Some(remaining_ttl) if remaining_ttl > self.max_ttl_sec => {
                return Err(StorageError::UnsupportedTtl)
            }
            _ => {}
        }

        match value {
            ValueRef::Peer(value) => self.insert_signed_value(value),
            ValueRef::Overlay(value) => self.insert_overlay_value(value),
        }
    }

    fn insert_signed_value(&self, value: &PeerValueRef<'_>) -> Result<bool, StorageError> {
        let Some(public_key) = value.key.peer_id.as_public_key() else {
            return Err(StorageError::InvalidSignature);
        };

        if !matches!(
            <&[u8; 64]>::try_from(value.signature.as_ref()),
            Ok(signature) if public_key.verify(value, signature)
        ) {
            return Err(StorageError::InvalidSignature);
        }

        Ok(self
            .cache
            .entry(tl_proto::hash(&value.key))
            .or_insert_with_if(
                || StoredValue::new(value, value.expires_at),
                |prev| prev.expires_at < value.expires_at,
            )
            .is_fresh())
    }

    fn insert_overlay_value(&self, value: &OverlayValueRef<'_>) -> Result<bool, StorageError> {
        let Some(merger) = self.overlay_value_merger.upgrade() else {
            return Ok(false);
        };

        merger.check_value(value)?;

        enum OverlayValueCow<'a, 'b> {
            Borrowed(&'a OverlayValueRef<'b>),
            Owned(OverlayValue),
        }

        impl OverlayValueCow<'_, '_> {
            fn make_stored_value(&self) -> StoredValue {
                match self {
                    Self::Borrowed(value) => StoredValue::new(*value, value.expires_at),
                    Self::Owned(value) => StoredValue::new(value, value.expires_at),
                }
            }
        }

        let new_value = RefCell::new(OverlayValueCow::Borrowed(value));

        Ok(self
            .cache
            .entry(tl_proto::hash(&value.key))
            .or_insert_with_if(
                || {
                    let value = new_value.borrow();
                    value.make_stored_value()
                },
                |prev| {
                    let Ok(mut prev) = tl_proto::deserialize::<OverlayValue>(&prev.data) else {
                        // Invalid values are always replaced with new values
                        return true;
                    };

                    if merger.merge_value(value, &mut prev) {
                        *new_value.borrow_mut() = OverlayValueCow::Owned(prev);
                        true
                    } else {
                        false
                    }
                },
            )
            .is_fresh())
    }
}

#[derive(Clone)]
struct StoredValue {
    expires_at: u32,
    data: Bytes,
}

impl StoredValue {
    fn new<T: TlWrite<Repr = tl_proto::Boxed>>(value: &T, expires_at: u32) -> Self {
        let mut data = BytesMut::with_capacity(value.max_size_hint());
        value.write_to(&mut data);

        StoredValue {
            expires_at,
            data: data.freeze(),
        }
    }
}

struct ValueExpiry;

impl Expiry<StorageKeyId, StoredValue> for ValueExpiry {
    fn expire_after_create(
        &self,
        _key: &StorageKeyId,
        value: &StoredValue,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(ttl_since_now(value.expires_at))
    }

    fn expire_after_update(
        &self,
        _key: &StorageKeyId,
        value: &StoredValue,
        _updated_at: std::time::Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(ttl_since_now(value.expires_at))
    }
}

fn ttl_since_now(expires_at: u32) -> Duration {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap();

    Duration::from_secs(expires_at as u64).saturating_sub(now)
}

pub type StorageKeyId = [u8; 32];

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("value expired")]
    ValueExpired,
    #[error("unsupported ttl")]
    UnsupportedTtl,
    #[error("invalid key")]
    InvalidKey,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("value too big")]
    ValueTooBig,
}
