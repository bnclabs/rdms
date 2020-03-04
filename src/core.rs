//! Module `core` define and implement core types and traits for [rdms].
//!
//! List of types implementing CommitIterator
//! =========================================
//!
//! * [CommitWrapper][scans::CommitWrapper], a wrapper type to convert any
//!   iterator into a [CommitIterator].
//! * [std::vec::IntoIter], iterator from std-lib for a vector of entries.
//! * [Llrb], memory index using left-leaning-red-black tree.
//! * [Mvcc], memory index using multi-version-concurrency-control for LLRB.
//! * [Robt], disk index using full-packed, immutable btree.
//!

use std::{
    borrow::Borrow,
    convert::TryInto,
    ffi, fmt, fs,
    hash::Hash,
    marker,
    mem::ManuallyDrop,
    ops::{Bound, RangeBounds},
    result,
    sync::atomic::{AtomicBool, Ordering::SeqCst},
};

use crate::{error::Error, util, vlog};
#[allow(unused_imports)]
use crate::{
    llrb::Llrb,
    mvcc::Mvcc,
    rdms::{self, Rdms},
    robt::Robt,
    scans,
    wal::Wal,
};

/// Type alias for all results returned by [rdms] methods.
pub type Result<T> = result::Result<T, Error>;

/// Type alias to trait-objects iterating over an index.
pub type IndexIter<'a, K, V> = Box<dyn Iterator<Item = Result<Entry<K, V>>> + 'a>;

/// Type alias to trait-objects iterating, piece-wise, over [Index].
pub type ScanIter<'a, K, V> = Box<dyn Iterator<Item = Result<ScanEntry<K, V>>> + 'a>;

/// A convenience trait to group thread-safe trait conditions.
pub trait ThreadSafe: 'static + Send {}

// TODO: should cutoff have a force variant to force compaction ?
/// Cutoff enumerated parameter to [compact][Index::compact] method. Refer
/// to [rdms] library documentation for more information on compaction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cutoff {
    /// Mono compaction is for non-lsm compaction.
    Mono,
    /// Tombstone-compaction, refer to [rdms] for more detail.
    Tombstone(Bound<u64>),
    /// Lsm-compaction, refer to [rdms] for more detail.
    Lsm(Bound<u64>),
}

impl Cutoff {
    pub fn new_mono() -> Cutoff {
        Cutoff::Mono
    }

    pub fn new_tombstone(b: Bound<u64>) -> Cutoff {
        Cutoff::Tombstone(b)
    }

    pub fn new_tombstone_empty() -> Cutoff {
        Cutoff::Lsm(Bound::Excluded(std::u64::MIN))
    }

    pub fn new_lsm(b: Bound<u64>) -> Cutoff {
        Cutoff::Lsm(b)
    }

    pub fn new_lsm_empty() -> Cutoff {
        Cutoff::Lsm(Bound::Excluded(std::u64::MIN))
    }
    pub fn to_bound(&self) -> Bound<u64> {
        match self {
            Cutoff::Mono => Bound::Excluded(std::u64::MIN),
            Cutoff::Lsm(b) => b.clone(),
            Cutoff::Tombstone(b) => b.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Cutoff::Mono => false,
            Cutoff::Lsm(Bound::Excluded(n)) => *n == std::u64::MIN,
            Cutoff::Tombstone(Bound::Excluded(n)) => *n == std::u64::MIN,
            _ => false,
        }
    }
}

/// Trait for diffable values.
///
/// Version control is a unique feature built into [rdms]. And this is possible
/// by values implementing this trait. Note that this version control follows
/// centralised behaviour, as apposed to distributed behaviour, for which we
/// need three-way-merge trait. Now more on how it works:
///
/// If,
/// ```notest
/// P = old value; C = new value; D = difference between P and C
/// ```
///
/// Then,
/// ```notest
/// D = C - P (diff operation)
/// P = C - D (merge operation, to get old value)
/// ```
pub trait Diff: Sized + From<<Self as Diff>::D> {
    type D: Clone + From<Self> + Into<Self> + Footprint;

    /// Return the delta between two consecutive versions of a value.
    /// `Delta = New - Old`.
    fn diff(&self, old: &Self) -> Self::D;

    /// Merge delta with newer version to return older version of the value.
    /// `Old = New - Delta`.
    fn merge(&self, delta: &Self::D) -> Self;
}

/// Trait to be implemented by index-types, key-types and, value-types.
///
/// This trait is required to compute the memory or disk foot-print
/// for index-types, key-types and value-types.
///
/// **Note: This can be an approximate measure.**
///
pub trait Footprint {
    /// Return the approximate size of the underlying type, when
    /// stored in memory or serialized on disk.
    ///
    /// NOTE: `isize` is used instead of `usize` because of delta computation.
    fn footprint(&self) -> Result<isize>;
}

/// Trait define methods to integrate index with [Wal] (Write-Ahead-Log).
///
/// All the methods defined by this trait will be dispatched when
/// reloading an index from on-disk Write-Ahead-Log.
pub trait Replay<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Replay set operation from wal-file onto index.
    fn set_index(&mut self, key: K, value: V, index: u64) -> Result<()>;

    /// Replay set-cas operation from wal-file onto index.
    fn set_cas_index(&mut self, key: K, value: V, cas: u64, index: u64) -> Result<()>;

    /// Replay delete operation from wal-file onto index.
    fn delete_index(&mut self, key: K, index: u64) -> Result<()>;
}

/// Trait define methods to integrate index with Wal (Write-Ahead-Log).
///
/// After writing into the [Wal], write operation shall be applied on
/// the [Index] [write-handle][Index::W].
pub trait WalWriter<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Set {key, value} in index. Return older entry if present.
    ///
    /// *LSM mode*: Add a new version for the key, perserving the old value.
    fn set_index(&mut self, key: K, value: V, index: u64) -> Result<Option<Entry<K, V>>>;

    /// Set {key, value} in index if an older entry exists with the
    /// same `cas` value. To create a fresh entry, pass `cas` as ZERO.
    /// Return older entry if present.
    ///
    /// *LSM mode*: Add a new version for the key, perserving the old value.
    fn set_cas_index(
        &mut self,
        key: K,
        value: V,
        cas: u64,
        index: u64,
    ) -> Result<Option<Entry<K, V>>>;

    /// Delete key from index. Return old entry if present.
    ///
    /// *LSM mode*: Mark the entry as deleted along with seqno at which it
    /// deleted
    ///
    /// NOTE: K should be borrowable as &Q and Q must be convertable to
    /// owned K. This is require in lsm mode, where owned K must be
    /// inserted into the tree.
    fn delete_index<Q>(&mut self, key: &Q, index: u64) -> Result<Option<Entry<K, V>>>
    where
        K: Borrow<Q>,
        Q: ToOwned<Owned = K> + Ord + ?Sized;
}

/// Trait to create new memory based index instances using pre-defined set of
/// configuration.
pub trait WriteIndexFactory<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    type I: Index<K, V> + Footprint;

    /// Create a new index instance with predefined configuration,
    /// Typically this index will be used to index new set of entries.
    fn new(&self, name: &str) -> Result<Self::I>;

    /// Index type identification purpose.
    fn to_type(&self) -> String;
}

/// Trait to create new disk based index instances using pre-defined set
/// of configuration.
pub trait DiskIndexFactory<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    type I: Clone + Index<K, V> + CommitIterator<K, V> + Footprint;

    /// Create a new index instance with predefined configuration.
    /// Typically this index will be used to commit newer snapshots
    /// onto disk.
    fn new(&self, dir: &ffi::OsStr, name: &str) -> Result<Self::I>;

    /// Open an existing index instance with predefined configuration.
    fn open(&self, dir: &ffi::OsStr, name: &str) -> Result<Self::I>;

    /// Index type for identification purpose.
    fn to_type(&self) -> String;
}

/// Trait to commit a batch of pre-sorted entries into target index.
///
/// Main purpose of this trait is to give target index, into which
/// the source iterator must be commited, an ability to generate the
/// actual iterator(s) the way it suits itself. In other words, target
/// index might call any of the method to generate the required iterator(s).
///
/// On the other hand, it may not be possible for the target index to
/// know the `within` sequence-no range to filter out entries and its
/// versions, for which we use [CommitIter]
pub trait CommitIterator<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Return a handle for full table iteration. Caller can hold this handle
    /// for a long time, hence implementors should make sure to handle
    /// unwanted side-effects.
    fn scan<G>(&mut self, within: G) -> Result<IndexIter<K, V>>
    where
        G: Clone + RangeBounds<u64>;

    /// Return a list of equally balanced handles to iterate on
    /// range-partitioned entries.
    fn scans<G>(&mut self, n_shards: usize, within: G) -> Result<Vec<IndexIter<K, V>>>
    where
        G: Clone + RangeBounds<u64>;

    /// Same as [scans][CommitIterator::scans] but range partition is
    /// decided by the `ranges` argument. And unlike the `shards` argument,
    /// `ranges` argument is treated with precision, number of iterators
    /// returned shall exactly match _range.len()_.
    fn range_scans<N, G>(&mut self, ranges: Vec<N>, within: G) -> Result<Vec<IndexIter<K, V>>>
    where
        G: Clone + RangeBounds<u64>,
        N: Clone + RangeBounds<K>;
}

/// Trait implemented by all types of rdms-indexes.
///
/// Note that not all index types shall implement all the methods
/// defined by this trait.
///
pub trait Index<K, V>: Sized
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Writer handle into this index to ingest, concurrently, key-value pairs.
    type W: Writer<K, V>;

    /// Reader handle into this index to concurrently access with other readers
    /// and writers.
    type R: Reader<K, V> + CommitIterator<K, V>;

    /// Return the name of the index.
    fn to_name(&self) -> Result<String>;

    /// Return application metadata, that was previously commited into index.
    fn to_metadata(&self) -> Result<Vec<u8>>;

    /// Return the current seqno tracked by this index.
    fn to_seqno(&self) -> Result<u64>;

    /// Application can set the start sequence number for this index.
    fn set_seqno(&mut self, seqno: u64) -> Result<()>;

    /// Create a new read handle, for multi-threading. Note that not all
    /// indexes allow concurrent readers. Refer to index API for more details.
    fn to_reader(&mut self) -> Result<Self::R>;

    /// Create a new write handle, for multi-threading. Note that not all
    /// indexes allow concurrent writers. Refer to index API for more details.
    fn to_writer(&mut self) -> Result<Self::W>;

    /// Commit entries from iterator into the index. Though it takes mutable
    /// reference, there can be concurrent compact() call. It is upto the
    /// implementing type to synchronize the concurrent commit() and compact()
    /// calls.
    fn commit<C, F>(&mut self, scanner: CommitIter<K, V, C>, mf: F) -> Result<()>
    where
        C: CommitIterator<K, V>,
        F: Fn(Vec<u8>) -> Vec<u8>;

    /// Compact index to reduce index-footprint. Though it takes mutable
    /// reference, there can be concurrent commit() call. It is upto the
    /// implementing type to synchronize the concurrent commit() and
    /// compact() calls. All entries whose mutation versions are below the
    /// `cutoff` bound can be purged permenantly.
    ///
    /// Return number of items in index.
    fn compact<F>(&mut self, cutoff: Cutoff, metacb: F) -> Result<usize>
    where
        F: Fn(Vec<u8>) -> Vec<u8>;

    /// End of index life-cycle. Persisted data (in disk) shall not be
    /// cleared. Refer [purge][Index::purge] for that.
    fn close(self) -> Result<()>;

    /// End of index life-cycle. Also clears persisted data (in disk).
    fn purge(self) -> Result<()>;
}

/// Trait to self-validate index's internal state.
pub trait Validate<T: fmt::Display> {
    /// Call this to make sure all is well. Note that this can be
    /// a costly call. Returned value can be serialized into string
    /// format and logged, printed, etc..
    fn validate(&mut self) -> Result<T>;
}

/// Trait to manage keys in a bitmapped Bloom-filter.
pub trait Bloom: Sized {
    /// Create an empty bit-map.
    fn create() -> Self;

    /// Return the number of items in the bitmap.
    fn len(&self) -> Result<usize>;

    /// Add key into the index.
    fn add_key<Q: ?Sized + Hash>(&mut self, element: &Q);

    /// Add key into the index.
    fn add_digest32(&mut self, digest: u32);

    /// Check whether key in persent, there can be false positives but
    /// no false negatives.
    fn contains<Q: ?Sized + Hash>(&self, element: &Q) -> bool;

    /// Serialize the bit-map to binary array.
    fn to_vec(&self) -> Vec<u8>;

    /// Deserialize the binary array to bit-map.
    fn from_vec(buf: &[u8]) -> Result<Self>;

    /// Merge two bitmaps.
    fn or(&self, other: &Self) -> Result<Self>;
}

/// Trait define read operations for rdms-index.
pub trait Reader<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Get `key` from index. Returned entry may not have all its
    /// previous versions, if it is costly to fetch from disk.
    fn get<Q>(&mut self, key: &Q) -> Result<Entry<K, V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + Hash;

    /// Iterate over all entries in this index. Returned entry may not
    /// have all its previous versions, if it is costly to fetch from disk.
    fn iter(&mut self) -> Result<IndexIter<K, V>>;

    /// Iterate from lower bound to upper bound. Returned entry may not
    /// have all its previous versions, if it is costly to fetch from disk.
    fn range<'a, R, Q>(&'a mut self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + Clone + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized;

    /// Iterate from upper bound to lower bound. Returned entry may not
    /// have all its previous versions, if it is costly to fetch from disk.
    fn reverse<'a, R, Q>(&'a mut self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + Clone + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized;

    /// Get `key` from index. Returned entry shall have all its
    /// previous versions, can be a costly call.
    fn get_with_versions<Q>(&mut self, key: &Q) -> Result<Entry<K, V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + Hash;

    /// Iterate over all entries in this index. Returned entry shall
    /// have all its previous versions, can be a costly call.
    fn iter_with_versions(&mut self) -> Result<IndexIter<K, V>>;

    /// Iterate from lower bound to upper bound. Returned entry shall
    /// have all its previous versions, can be a costly call.
    fn range_with_versions<'a, R, Q>(&'a mut self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + Clone + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized;

    /// Iterate from upper bound to lower bound. Returned entry shall
    /// have all its previous versions, can be a costly call.
    fn reverse_with_versions<'a, R, Q>(&'a mut self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + Clone + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized;
}

/// Trait define write operations for rdms-index.
pub trait Writer<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Set {key, value} in index. Return older entry if present.
    /// If operation was invalid or NOOP, returned seqno shall be ZERO.
    ///
    /// *LSM mode*: Add a new version for the key, perserving the old value.
    fn set(&mut self, k: K, v: V) -> Result<Option<Entry<K, V>>>;

    /// Set {key, value} in index if an older entry exists with the
    /// same `cas` value. To create a fresh entry, pass `cas` as ZERO.
    /// Return the older entry if present. If operation was invalid or
    /// NOOP, returned seqno shall be ZERO.
    ///
    /// *LSM mode*: Add a new version for the key, perserving the old value.
    fn set_cas(&mut self, k: K, v: V, cas: u64) -> Result<Option<Entry<K, V>>>;

    /// Delete key from index. Return the mutation and entry if present.
    /// If operation was invalid or NOOP, returned seqno shall be ZERO.
    ///
    /// *LSM mode*: Mark the entry as deleted along with seqno at which it
    /// deleted
    ///
    /// NOTE: K should be borrowable as &Q and Q must be convertable to
    /// owned K. This is require in lsm mode, where owned K must be
    /// inserted into the tree.
    fn delete<Q>(&mut self, key: &Q) -> Result<Option<Entry<K, V>>>
    where
        K: Borrow<Q>,
        Q: ToOwned<Owned = K> + Ord + ?Sized;
}

/// Trait to serialize key and value types.
pub trait Serialize: Sized {
    /// Convert this value into binary equivalent. Encoded bytes shall
    /// be appended to the input-buffer `buf`. Return bytes encoded.
    fn encode(&self, buf: &mut Vec<u8>) -> Result<usize>;

    /// Reverse process of encode, given the binary equivalent `buf`,
    /// construct `self`.
    fn decode(&mut self, buf: &[u8]) -> Result<usize>;
}

/// Trait typically implemented by mem-only indexes, to construct a stable
/// full-table scan.
///
/// Indexes implementing this trait is expected to return an iterator over
/// all its entries. Some of the necessary conditions are:
///
/// * Iteration should be stable even if there is background mutation.
/// * Iteration should not block background mutation, it might block for a
///   short while though.
pub trait PiecewiseScan<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Return an iterator over entries that meet following properties
    /// * Only entries greater than range.start_bound().
    /// * Only entries whose modified seqno is within seqno-range.
    ///
    /// This method is typically implemented by memory-only indexes. Also,
    /// returned entry may not have all its previous versions, if it is
    /// costly to fetch from disk.
    fn pw_scan<G>(&mut self, from: Bound<K>, within: G) -> Result<ScanIter<K, V>>
    where
        G: Clone + RangeBounds<u64>;
}

/// Trait to serialize an implementing type to JSON encoded string.
///
/// Typically used for web-interfaces.
pub trait ToJson {
    /// Call this method to get the JSON encoded string.
    fn to_json(&self) -> String;
}

/// Delta maintains the older version of value, with necessary fields for
/// log-structured-merge.
#[derive(Clone)]
pub(crate) enum InnerDelta<V>
where
    V: Clone + Diff,
{
    U { delta: vlog::Delta<V>, seqno: u64 },
    D { seqno: u64 },
}

#[derive(Clone)]
pub(crate) struct Delta<V>
where
    V: Clone + Diff,
{
    data: InnerDelta<V>,
}

// Delta construction methods.
impl<V> Delta<V>
where
    V: Clone + Diff,
{
    pub(crate) fn new_upsert(delta: vlog::Delta<V>, seqno: u64) -> Delta<V> {
        Delta {
            data: InnerDelta::U { delta, seqno },
        }
    }

    pub(crate) fn new_delete(seqno: u64) -> Delta<V> {
        Delta {
            data: InnerDelta::D { seqno },
        }
    }
}

impl<V> Footprint for Delta<V>
where
    V: Clone + Diff,
{
    fn footprint(&self) -> Result<isize> {
        use std::mem::size_of;

        let fp: isize = convert_at!(size_of::<Delta<V>>())?;
        Ok(fp
            + match &self.data {
                InnerDelta::U { delta, .. } => delta.footprint()?,
                InnerDelta::D { .. } => 0,
            })
    }
}

impl<V> AsRef<InnerDelta<V>> for Delta<V>
where
    V: Clone + Diff,
{
    fn as_ref(&self) -> &InnerDelta<V> {
        &self.data
    }
}

/// Delta accessor methods
impl<V> Delta<V>
where
    V: Clone + Diff,
{
    /// Return the underlying _difference_ value for this delta.
    #[allow(dead_code)] // TODO: remove if not required.
    pub(crate) fn to_diff(&self) -> Option<<V as Diff>::D> {
        match &self.data {
            InnerDelta::D { .. } => None,
            InnerDelta::U { delta, .. } => delta.to_native_delta(),
        }
    }

    /// Return the underlying _difference_ value for this delta.
    #[allow(dead_code)] // TODO: remove if not required.
    pub(crate) fn into_diff(self) -> Option<<V as Diff>::D> {
        match self.data {
            InnerDelta::D { .. } => None,
            InnerDelta::U { delta, .. } => delta.into_native_delta(),
        }
    }

    /// Return the seqno at which this delta was modified,
    /// which includes Create and Delete operations.
    /// To differentiate between Create and Delete operations
    /// use born_seqno() and dead_seqno() methods respectively.
    pub(crate) fn to_seqno(&self) -> u64 {
        match &self.data {
            InnerDelta::U { seqno, .. } => *seqno,
            InnerDelta::D { seqno } => *seqno,
        }
    }

    /// Return the seqno and the state of modification. `true` means
    /// this version was a create/update, and `false` means
    /// this version was deleted.
    #[allow(dead_code)] // TODO: remove if not required.
    pub(crate) fn to_seqno_state(&self) -> (bool, u64) {
        match &self.data {
            InnerDelta::U { seqno, .. } => (true, *seqno),
            InnerDelta::D { seqno } => (false, *seqno),
        }
    }

    #[allow(dead_code)] // TODO: remove this once rdms is weaved-up.
    pub(crate) fn into_upserted(self) -> Option<(vlog::Delta<V>, u64)> {
        match self.data {
            InnerDelta::U { delta, seqno } => Some((delta, seqno)),
            InnerDelta::D { .. } => None,
        }
    }

    #[allow(dead_code)] // TODO: remove this once rdms is weaved-up.
    pub(crate) fn into_deleted(self) -> Option<u64> {
        match self.data {
            InnerDelta::D { seqno } => Some(seqno),
            InnerDelta::U { .. } => None,
        }
    }

    pub(crate) fn is_reference(&self) -> bool {
        match self.data {
            InnerDelta::U {
                delta: vlog::Delta::Reference { .. },
                ..
            } => true,
            _ => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_deleted(&self) -> bool {
        match self.data {
            InnerDelta::D { .. } => true,
            InnerDelta::U { .. } => false,
        }
    }
}

pub(crate) enum Value<V> {
    U {
        value: ManuallyDrop<Box<vlog::Value<V>>>,
        is_reclaim: AtomicBool,
        seqno: u64,
    },
    D {
        seqno: u64,
    },
}

impl<V> Clone for Value<V>
where
    V: Clone,
{
    fn clone(&self) -> Value<V> {
        match self {
            Value::U {
                value,
                is_reclaim,
                seqno,
            } => Value::U {
                value: value.clone(),
                is_reclaim: AtomicBool::new(is_reclaim.load(SeqCst)),
                seqno: *seqno,
            },
            Value::D { seqno } => Value::D { seqno: *seqno },
        }
    }
}

impl<V> Drop for Value<V> {
    fn drop(&mut self) {
        // if is_reclaim is false, then it is a mvcc-clone. so don't touch
        // the value.
        match self {
            Value::U {
                value, is_reclaim, ..
            } if is_reclaim.load(SeqCst) => unsafe { ManuallyDrop::drop(value) },
            _ => (),
        }
    }
}

// Value construction methods
impl<V> Value<V>
where
    V: Clone,
{
    pub(crate) fn new_upsert(v: Box<vlog::Value<V>>, seqno: u64) -> Value<V> {
        Value::U {
            value: ManuallyDrop::new(v),
            is_reclaim: AtomicBool::new(true),
            seqno,
        }
    }

    pub(crate) fn new_upsert_value(value: V, seqno: u64) -> Value<V> {
        Value::U {
            value: ManuallyDrop::new(Box::new(vlog::Value::new_native(value))),
            is_reclaim: AtomicBool::new(true),
            seqno,
        }
    }

    pub(crate) fn new_delete(seqno: u64) -> Value<V> {
        Value::D { seqno }
    }

    pub(crate) fn mvcc_clone(&self, copyval: bool) -> Value<V> {
        match self {
            Value::U {
                value,
                seqno,
                is_reclaim,
            } if !copyval => {
                is_reclaim.store(false, SeqCst);
                let v = value.as_ref() as *const vlog::Value<V>;
                let value = unsafe { Box::from_raw(v as *mut vlog::Value<V>) };
                Value::U {
                    value: ManuallyDrop::new(value),
                    is_reclaim: AtomicBool::new(true),
                    seqno: *seqno,
                }
            }
            val => val.clone(),
        }
    }
}

// Value accessor methods
impl<V> Value<V>
where
    V: Clone,
{
    pub(crate) fn to_native_value(&self) -> Option<V> {
        match &self {
            Value::U { value, .. } => value.to_native_value(),
            Value::D { .. } => None,
        }
    }

    pub(crate) fn to_seqno(&self) -> u64 {
        match self {
            Value::U { seqno, .. } => *seqno,
            Value::D { seqno } => *seqno,
        }
    }

    pub(crate) fn is_deleted(&self) -> bool {
        match self {
            Value::U { .. } => false,
            Value::D { .. } => true,
        }
    }

    pub(crate) fn is_reference(&self) -> bool {
        match self {
            Value::U { value, .. } => value.is_reference(),
            _ => false,
        }
    }
}

impl<V> Footprint for Value<V>
where
    V: Footprint,
{
    fn footprint(&self) -> Result<isize> {
        use std::mem::size_of;

        Ok(match self {
            Value::U { value, .. } => {
                let size: isize = convert_at!(size_of::<V>())?;
                size + value.footprint()?
            }
            Value::D { .. } => 0,
        })
    }
}

/// Entry is the covering structure for a {Key, value} pair
/// indexed by rdms.
///
/// It is a user facing structure, also used in stitching together
/// different components of [Rdms].
#[derive(Clone)]
pub struct Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    key: K,
    value: Value<V>,
    deltas: Vec<Delta<V>>,
}

impl<K, V> Borrow<K> for Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    fn borrow(&self) -> &K {
        self.as_key()
    }
}

impl<K, V> Footprint for Entry<K, V>
where
    K: Clone + Ord + Footprint,
    V: Clone + Diff + Footprint,
{
    /// Return the previous versions of this entry as Deltas.
    fn footprint(&self) -> Result<isize> {
        let mut fp = self.key.footprint()?;
        if !self.is_deleted() {
            fp += self.value.footprint()?;
        }
        for delta in self.deltas.iter() {
            fp += delta.footprint()?;
        }
        Ok(fp)
    }
}

// Entry construction methods.
impl<K, V> Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Key's memory footprint cannot exceed this limit. _1GB_.
    pub const KEY_SIZE_LIMIT: usize = 1024 * 1024 * 1024;
    /// Value's memory footprint cannot exceed this limit. _1TB_.
    pub const VALUE_SIZE_LIMIT: usize = 1024 * 1024 * 1024 * 1024;
    /// Value diff's memory footprint cannot exceed this limit. _1TB_.
    pub const DIFF_SIZE_LIMIT: usize = 1024 * 1024 * 1024 * 1024;

    pub(crate) fn new(key: K, value: Value<V>) -> Entry<K, V> {
        Entry {
            key,
            value,
            deltas: vec![],
        }
    }

    pub(crate) fn mvcc_clone(&self, copyval: bool) -> Entry<K, V> {
        Entry {
            key: self.key.clone(),
            value: self.value.mvcc_clone(copyval),
            deltas: self.deltas.clone(),
        }
    }

    pub(crate) fn set_deltas(&mut self, deltas: Vec<Delta<V>>) {
        self.deltas = deltas;
    }
}

// Entry accessor methods.
impl<K, V> Entry<K, V>
where
    K: Clone + Ord + Footprint,
    V: Clone + Diff + Footprint,
{
    // Corresponds to CREATE and UPDATE operations also the latest version,
    // for this entry. In non-lsm mode this is equivalent to over-writing
    // previous value.
    //
    // `nentry` is new_entry to be CREATE/UPDATE into index.
    //
    // TODO: may be we can just pass the Value, instead of `nentry` ?
    pub(crate) fn prepend_version(&mut self, nentry: Self, lsm: bool) -> Result<isize> {
        if lsm {
            self.prepend_version_lsm(nentry)
        } else {
            self.prepend_version_nolsm(nentry)
        }
    }

    // `nentry` is new_entry to be CREATE/UPDATE into index.
    fn prepend_version_nolsm(&mut self, nentry: Self) -> Result<isize> {
        let size = self.value.footprint()?;
        self.value = nentry.value.clone();
        Ok(self.value.footprint()? - size)
    }

    // `nentry` is new_entry to be CREATE/UPDATE into index.
    fn prepend_version_lsm(&mut self, nentry: Self) -> Result<isize> {
        let delta = match &self.value {
            Value::D { seqno } => Ok(Delta::new_delete(*seqno)),
            Value::U { value, seqno, .. } if !value.is_reference() => {
                // compute delta
                match &nentry.value {
                    Value::D { .. } => {
                        let diff: <V as Diff>::D = {
                            let v = value.to_native_value().unwrap();
                            From::from(v)
                        };
                        {
                            let v = vlog::Delta::new_native(diff);
                            Ok(Delta::new_upsert(v, *seqno))
                        }
                    }
                    Value::U { value: nvalue, .. } => {
                        let dff = nvalue
                            .to_native_value()
                            .unwrap()
                            .diff(&value.to_native_value().unwrap());
                        {
                            let v = vlog::Delta::new_native(dff);
                            Ok(Delta::new_upsert(v, *seqno))
                        }
                    }
                }
            }
            Value::U { .. } => {
                let msg = format!("Entry.prepend_version_lsm()");
                Err(Error::UnReachable(msg))
            }
        }?;

        let size = {
            let size = nentry.value.footprint()? + delta.footprint()?;
            size - self.value.footprint()?
        };

        self.deltas.insert(0, delta);
        self.prepend_version_nolsm(nentry)?;

        Ok(size)
    }

    // DELETE operation, only in lsm-mode or sticky mode.
    pub(crate) fn delete(&mut self, seqno: u64) -> Result<isize> {
        let size = self.footprint()?;

        match &self.value {
            Value::D { seqno } => {
                // insert a delete delta
                self.deltas.insert(0, Delta::new_delete(*seqno));
                Ok(())
            }
            Value::U { value, seqno, .. } if !value.is_reference() => {
                let delta = {
                    let d: <V as Diff>::D = From::from(value.to_native_value().unwrap());
                    vlog::Delta::new_native(d)
                };
                self.deltas.insert(0, Delta::new_upsert(delta, *seqno));
                Ok(())
            }
            Value::U { .. } => {
                let msg = format!("Entry.delete()");
                Err(Error::UnReachable(msg))
            }
        }?;

        self.value = Value::new_delete(seqno);
        Ok(self.footprint()? - size)
    }
}

impl<K, V> Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    // purge all versions whose seqno <= or < `cutoff`.
    pub(crate) fn purge(mut self, cutoff: Cutoff) -> Option<Entry<K, V>> {
        let n = self.to_seqno();

        let cutoff = match cutoff {
            Cutoff::Mono if self.is_deleted() => return None,
            Cutoff::Mono => {
                self.set_deltas(vec![]);
                return Some(self);
            }
            Cutoff::Lsm(cutoff) => cutoff,
            Cutoff::Tombstone(cutoff) if self.is_deleted() => match cutoff {
                Bound::Included(cutoff) if n <= cutoff => return None,
                Bound::Excluded(cutoff) if n < cutoff => return None,
                Bound::Unbounded => return None,
                _ => return Some(self),
            },
            Cutoff::Tombstone(_) => return Some(self),
        };

        // If all versions of this entry are before cutoff, then purge entry
        match cutoff {
            Bound::Included(0) => return Some(self),
            Bound::Excluded(0) => return Some(self),
            Bound::Included(cutoff) if n <= cutoff => return None,
            Bound::Excluded(cutoff) if n < cutoff => return None,
            Bound::Unbounded => return None,
            _ => (),
        }
        // Otherwise, purge only those versions that are before cutoff
        self.deltas = self
            .deltas
            .drain(..)
            .take_while(|d| {
                let seqno = d.to_seqno();
                match cutoff {
                    Bound::Included(cutoff) if seqno > cutoff => true,
                    Bound::Excluded(cutoff) if seqno >= cutoff => true,
                    _ => false,
                }
            })
            .collect();
        Some(self)
    }
}

impl<K, V> Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Pick all versions whose seqno is within the specified range.
    /// Note that, by rdms-design only memory-indexes ingesting new
    /// mutations are subjected to this filter function.
    pub fn filter_within(
        &self,
        start: Bound<u64>, // filter from
        end: Bound<u64>,   // filter till
    ) -> Option<Entry<K, V>> {
        // skip versions newer than requested range.
        let entry = self.skip_till(start.clone(), end)?;
        // purge versions older than request range.
        match start {
            Bound::Included(x) => {
                let cutoff = Cutoff::new_lsm(Bound::Excluded(x));
                entry.purge(cutoff)
            }
            Bound::Excluded(x) => {
                let cutoff = Cutoff::new_lsm(Bound::Included(x));
                entry.purge(cutoff)
            }
            Bound::Unbounded => Some(entry),
        }
    }

    fn skip_till(&self, ob: Bound<u64>, nb: Bound<u64>) -> Option<Entry<K, V>> {
        // skip entire entry if it is before the specified range.
        let n = self.to_seqno();
        match ob {
            Bound::Included(o_seqno) if n < o_seqno => return None,
            Bound::Excluded(o_seqno) if n <= o_seqno => return None,
            _ => (),
        }
        // skip the entire entry if it is after the specified range.
        let o = self.deltas.last().map_or(n, |d| d.to_seqno());
        match nb {
            Bound::Included(nb) if o > nb => return None,
            Bound::Excluded(nb) if o >= nb => return None,
            Bound::Included(nb) if n <= nb => return Some(self.clone()),
            Bound::Excluded(nb) if n < nb => return Some(self.clone()),
            Bound::Unbounded => return Some(self.clone()),
            _ => (),
        };

        // println!("skip_till {} {} {:?}", o, n, nb);
        // partial skip.
        let mut entry = self.clone();
        let mut iter = entry.deltas.drain(..);
        while let Some(delta) = iter.next() {
            let (value, _) = next_value(entry.value.to_native_value(), delta.data);
            entry.value = value;
            let seqno = entry.value.to_seqno();
            let done = match nb {
                Bound::Included(n_seqno) if seqno <= n_seqno => true,
                Bound::Excluded(n_seqno) if seqno < n_seqno => true,
                _ => false,
            };
            // println!("skip_till loop {} {:?} {} ", seqno, nb, done);
            if done {
                // collect the remaining deltas and return
                entry.deltas = iter.collect();
                // println!("skip_till fin {}", entry.deltas.len());
                return Some(entry);
            }
        }

        unreachable!()
    }

    /// Return an iterator for all existing versions for this entry.
    pub fn versions(&self) -> VersionIter<K, V> {
        VersionIter {
            key: self.key.clone(),
            entry: Some(Entry {
                key: self.key.clone(),
                value: self.value.clone(),
                deltas: Default::default(),
            }),
            curval: None,
            deltas: Some(self.to_deltas().into_iter()),
        }
    }
}

impl<K, V> Entry<K, V>
where
    K: Clone + Ord + Footprint,
    V: Clone + Diff + Footprint,
{
    /// Merge two version chain for same key. This can happen between
    /// two entries from two index, where one of them is a newer snapshot
    /// of the same index. In any case it is expected that all versions of
    /// one entry shall be greater than all versions of the other entry.
    pub fn xmerge(self, entry: Entry<K, V>) -> Result<Entry<K, V>> {
        // `a` is newer than `b`, and all versions in a and b are mutually
        // exclusive in seqno ordering.
        let (a, mut b) = if self.to_seqno() > entry.to_seqno() {
            (self, entry)
        } else if entry.to_seqno() > self.to_seqno() {
            (entry, self)
        } else {
            panic!("xmerge {} == {}", entry.to_seqno(), self.to_seqno())
        };

        if cfg!(debug_assertions) {
            a.validate_xmerge(&b)?;
        }

        for ne in a.versions().collect::<Vec<Entry<K, V>>>().into_iter().rev() {
            // println!("xmerge {} {}", ne.to_seqno(), ne.is_deleted());
            b.prepend_version(ne, true /* lsm */)?;
        }
        Ok(b)
    }

    // `self` is newer than `entry`
    fn validate_xmerge(&self, entr: &Entry<K, V>) -> Result<()> {
        // validate ordering
        let mut seqnos = vec![self.to_seqno()];
        self.deltas.iter().for_each(|d| seqnos.push(d.to_seqno()));
        seqnos.push(entr.to_seqno());
        entr.deltas.iter().for_each(|d| seqnos.push(d.to_seqno()));
        let fail = seqnos[0..seqnos.len() - 1]
            .into_iter()
            .zip(seqnos[1..].into_iter())
            .any(|(a, b)| a <= b);

        if fail {
            //println!(
            //    "validate_xmerge {:?} {} {:?} {} {:?}",
            //    seqnos,
            //    self.to_seqno(),
            //    self.deltas
            //        .iter()
            //        .map(|d| d.to_seqno())
            //        .collect::<Vec<u64>>(),
            //    entr.to_seqno(),
            //    entr.deltas
            //        .iter()
            //        .map(|d| d.to_seqno())
            //        .collect::<Vec<u64>>(),
            //);
            Err(Error::UnExpectedFail(format!("Entry.validate_xmerge()")))
        } else {
            Ok(())
        }
    }
}

impl<K, V> Entry<K, V>
where
    K: Clone + Ord,
    V: Default + Clone + Diff + Serialize,
    <V as Diff>::D: Default + Serialize,
{
    pub(crate) fn fetch_value(&mut self, fd: &mut fs::File) -> Result<()> {
        Ok(match &self.value {
            Value::U { value, seqno, .. } => match value.to_reference() {
                Some((fpos, len, _seqno)) => {
                    self.value =
                        Value::new_upsert(Box::new(vlog::fetch_value(fpos, len, fd)?), *seqno);
                }
                _ => (),
            },
            _ => (),
        })
    }

    pub(crate) fn fetch_deltas(&mut self, fd: &mut fs::File) -> Result<()> {
        for delta in self.deltas.iter_mut() {
            match delta.data {
                InnerDelta::U {
                    delta: vlog::Delta::Reference { fpos, length, .. },
                    seqno,
                } => {
                    *delta = Delta::new_upsert(vlog::fetch_delta(fpos, length, fd)?, seqno);
                }
                _ => (),
            }
        }
        Ok(())
    }
}

// Entry accessor methods
impl<K, V> Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Return a reference to key.
    #[inline]
    pub fn as_key(&self) -> &K {
        &self.key
    }

    /// Return owned key vlalue.
    #[inline]
    pub fn to_key(&self) -> K {
        self.key.clone()
    }

    #[inline]
    pub(crate) fn as_deltas(&self) -> &Vec<Delta<V>> {
        &self.deltas
    }

    pub(crate) fn to_delta_count(&self) -> usize {
        self.deltas.len()
    }

    pub(crate) fn as_value(&self) -> &Value<V> {
        &self.value
    }

    /// Return the previous versions of this entry as Deltas.
    #[inline]
    pub(crate) fn to_deltas(&self) -> Vec<Delta<V>> {
        self.deltas.clone()
    }

    /// Return value. If entry is marked as deleted, return None.
    pub fn to_native_value(&self) -> Option<V> {
        self.value.to_native_value()
    }

    /// Return the latest seqno that created/updated/deleted this entry.
    #[inline]
    pub fn to_seqno(&self) -> u64 {
        match self.value {
            Value::U { seqno, .. } => seqno,
            Value::D { seqno, .. } => seqno,
        }
    }

    /// Return the seqno and the state of modification. `true` means
    /// latest value was a create/update, and `false` means latest value
    /// was deleted.
    #[inline]
    pub fn to_seqno_state(&self) -> (bool, u64) {
        match self.value {
            Value::U { seqno, .. } => (true, seqno),
            Value::D { seqno, .. } => (false, seqno),
        }
    }

    /// Return whether this entry is in deleted state, applicable onle
    /// in lsm mode.
    pub fn is_deleted(&self) -> bool {
        self.value.is_deleted()
    }
}

/// Iterate from newest to oldest _available_ version for this entry.
pub struct VersionIter<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    key: K,
    entry: Option<Entry<K, V>>,
    curval: Option<V>,
    deltas: Option<std::vec::IntoIter<Delta<V>>>,
}

impl<K, V> Iterator for VersionIter<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    type Item = Entry<K, V>;

    fn next(&mut self) -> Option<Self::Item> {
        // first iteration
        if let Some(entry) = self.entry.take() {
            if entry.value.is_reference() {
                self.deltas.take();
                return None;
            } else {
                self.curval = entry.to_native_value();
                return Some(entry);
            }
        }
        // remaining iterations
        let delta = {
            match &mut self.deltas {
                Some(deltas) => match deltas.next() {
                    None => {
                        return None;
                    }
                    Some(delta) if delta.is_reference() => {
                        self.deltas.take();
                        return None;
                    }
                    Some(delta) => delta,
                },
                None => return None,
            }
        };
        let (value, curval) = next_value(self.curval.take(), delta.data);
        self.curval = curval;
        Some(Entry::new(self.key.clone(), value))
    }
}

fn next_value<V>(value: Option<V>, delta: InnerDelta<V>) -> (Value<V>, Option<V>)
where
    V: Clone + Diff,
{
    match (value, delta) {
        (None, InnerDelta::D { seqno }) => {
            // consequitive delete
            (Value::new_delete(seqno), None)
        }
        (Some(_), InnerDelta::D { seqno }) => {
            // this entry is deleted.
            (Value::new_delete(seqno), None)
        }
        (None, InnerDelta::U { delta, seqno }) => {
            // previous entry was a delete.
            let nv: V = From::from(delta.into_native_delta().unwrap());
            let value = Value::new_upsert(Box::new(vlog::Value::new_native(nv.clone())), seqno);
            (value, Some(nv))
        }
        (Some(curval), InnerDelta::U { delta, seqno }) => {
            // this and previous entry are create/update.
            let nv = curval.merge(&delta.into_native_delta().unwrap());
            let value = Value::new_upsert(Box::new(vlog::Value::new_native(nv.clone())), seqno);
            (value, Some(nv))
        }
    }
}

/// Covering type for entries iterated by piece-wise full-table scanner.
///
/// This covering type is necessary because of the way [PiecewiseScan]
/// implementation works. Refer to the documentation of the trait for
/// additional detail. To meet the trait's expectation, the implementing
/// index should have the ability to differentiate between end-of-iteration
/// and end-of-iteration to release the read-lock, if any.
pub enum ScanEntry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Entry found, continue with iteration.
    Found(Entry<K, V>),
    /// Refill denotes end-of-iteration to release the read-lock.
    Retry(K),
}

/// Container type for types implementing [CommitIterator] trait.
///
/// Refer to the trait for more details. Instead of using [CommitIterator]
/// type directly, we are using [CommitIter] indirection for [Index::commit]
/// operation to handle situations where it is not possible/efficient to
/// construct a filtered-iterator, but the target index is known to pick any
/// of the [CommitIterator] method to construct the actual iterators.
pub struct CommitIter<K, V, C>
where
    K: Clone + Ord,
    V: Clone + Diff,
    C: CommitIterator<K, V>,
{
    scanner: C,
    start: Bound<u64>,
    end: Bound<u64>,

    _phantom_key: marker::PhantomData<K>,
    _phantom_val: marker::PhantomData<V>,
}

impl<K, V, C> CommitIter<K, V, C>
where
    K: Clone + Ord,
    V: Clone + Diff,
    C: CommitIterator<K, V>,
{
    /// Construct a new commitable iterator from scanner, `within` shall
    /// be passed to scanner that allows target index to generate the actual
    /// iterator allowing for both efficiency and flexibility.
    pub fn new<G>(scanner: C, within: G) -> CommitIter<K, V, C>
    where
        G: RangeBounds<u64>,
    {
        let (start, end) = util::to_start_end(within);
        CommitIter {
            scanner,
            start,
            end,
            _phantom_key: marker::PhantomData,
            _phantom_val: marker::PhantomData,
        }
    }

    /// Return the `within` argument supplied while constructing this iterator.
    pub fn to_within(&self) -> (Bound<u64>, Bound<u64>) {
        (self.start.clone(), self.end.clone())
    }

    /// Calls underlying scanner's [scan][CommitIterator::scan] method
    /// along with `within` to generate the actual commitable iterator.
    pub fn scan(&mut self) -> Result<IndexIter<K, V>> {
        let within = (self.start.clone(), self.end.clone());
        self.scanner.scan(within)
    }

    /// Same as scan, except that it calls scanner's
    /// [scans][CommitIterator::scans] method.
    pub fn scans(&mut self, n_shards: usize) -> Result<Vec<IndexIter<K, V>>> {
        let within = (self.start.clone(), self.end.clone());
        self.scanner.scans(n_shards, within)
    }

    /// Same as scan, except that it calls scanner's
    /// [range_scans][CommitIterator::range_scans] method.
    pub fn range_scans<N>(&mut self, rs: Vec<N>) -> Result<Vec<IndexIter<K, V>>>
    where
        N: Clone + RangeBounds<K>,
    {
        let within = (self.start.clone(), self.end.clone());
        self.scanner.range_scans(rs, within)
    }
}

#[cfg(test)]
#[path = "core_test.rs"]
mod core_test;
