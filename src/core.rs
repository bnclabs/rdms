use std::borrow::Borrow;
use std::ops::{Bound, RangeBounds};

use crate::error::Error;
use crate::vlog;

/// Result returned by bogn functions and methods.
pub type Result<T> = std::result::Result<T, Error>;

/// Index entry iterator.
pub type IndexIter<'a, K, V> = Box<dyn Iterator<Item = Result<Entry<K, V>>> + 'a>;

/// Index operations.
pub trait Index<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
    Self: Reader<K, V> + Writer<K, V>,
{
    /// Make a new empty index of this type, with same configuration.
    fn make_new(&self) -> Self;

    // Create a new writer handle. Note that, not all indexes allow
    // concurrent writers, and not all indexes support concurrent
    // read/write.
    // fn to_writer(&self) -> Writer<K, V>;
}

/// Index read operation.
pub trait Reader<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Get ``key`` from index. Returned entry may not have all its
    /// previous versions, if it is costly to fetch from disk.
    fn get<Q>(&self, key: &Q) -> Result<Entry<K, V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized;

    /// Iterate over all entries in this index. Returned entry may not
    /// have all its previous versions, if it is costly to fetch from disk.
    fn iter(&self) -> Result<IndexIter<K, V>>;

    /// Iterate from lower bound to upper bound. Returned entry may not
    /// have all its previous versions, if it is costly to fetch from disk.
    fn range<'a, R, Q>(&'a self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized;

    /// Iterate from upper bound to lower bound. Returned entry may not
    /// have all its previous versions, if it is costly to fetch from disk.
    fn reverse<'a, R, Q>(&'a self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized;

    /// Get ``key`` from index. Returned entry shall have all its
    /// previous versions, can be a costly call.
    fn get_with_versions<Q>(&self, key: &Q) -> Result<Entry<K, V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized;

    /// Iterate over all entries in this index. Returned entry shall
    /// have all its previous versions, can be a costly call.
    fn iter_with_versions(&self) -> Result<IndexIter<K, V>>;

    /// Iterate from lower bound to upper bound. Returned entry shall
    /// have all its previous versions, can be a costly call.
    fn range_with_versions<'a, R, Q>(&'a self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized;

    /// Iterate from upper bound to lower bound. Returned entry shall
    /// have all its previous versions, can be a costly call.
    fn reverse_with_versions<'a, R, Q>(&'a self, rng: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized;
}

/// Index read operation.
pub trait FullScan<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff + From<<V as Diff>::D>,
{
    /// Return an iterator over entries that meet following properties
    /// * Only entries greater than range.start_bound().
    /// * Only entries whose modified seqno is within seqno-range.
    ///
    /// This method is typically valid only for memory-only indexes. Also,
    /// returned entry may not have all its previous versions, if it is
    /// costly to fetch from disk.
    fn full_scan<G>(&self, from: Bound<K>, within: G) -> Result<IndexIter<K, V>>
    where
        G: Clone + RangeBounds<u64>;
}

/// Index write operations.
pub trait Writer<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    /// Set {key, value} in index. Return older entry if present.
    fn set_index(
        &mut self,
        key: K,
        value: V,
        index: u64, // seqno for this mutation
    ) -> Result<Option<Entry<K, V>>>;

    /// Set {key, value} in index if an older entry exists with the
    /// same ``cas`` value. To create a fresh entry, pass ``cas`` as ZERO.
    /// Return the seqno (index) for this mutation and older entry
    /// if present. If operation was invalid or NOOP, returned seqno shall
    /// be ZERO.
    fn set_cas_index(
        &mut self,
        key: K,
        value: V,
        cas: u64,
        index: u64,
    ) -> (u64, Result<Option<Entry<K, V>>>);

    /// Delete key from DB. Return the seqno (index) for this mutation
    /// and entry if present. If operation was invalid or NOOP, returned
    /// seqno shall be ZERO.
    fn delete_index<Q>(
        &mut self,
        key: &Q,
        index: u64, // seqno for this mutation
    ) -> (u64, Result<Option<Entry<K, V>>>)
    where
        K: Borrow<Q>,
        Q: ToOwned<Owned = K> + Ord + ?Sized;
}

/// Replay WAL (Write-Ahead-Log) entries on index.
pub trait Replay<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    fn set(
        &mut self,
        key: K,
        value: V,
        index: u64, // replay seqno
    ) -> Result<Entry<K, V>>;

    fn set_cas(
        &mut self,
        key: K,
        value: V,
        cas: u64,
        index: u64, // replay seqno
    ) -> Result<Entry<K, V>>;

    fn delete<Q>(&mut self, key: &Q, index: u64) -> Result<Entry<K, V>>;
}

/// Diffable values.
///
/// O = previous value
/// N = next value
/// D = difference between O and N
///
/// Then,
///
/// D = N - O (diff operation)
/// O = N - D (merge operation, to get old value)
pub trait Diff: Sized {
    type D: Clone + From<Self> + Into<Self> + Send + Sync;

    /// Return the delta between two version of value.
    /// D = N - O
    fn diff(&self, old: &Self) -> Self::D;

    /// Merge delta with this value to create another value.
    /// O = N - D
    fn merge(&self, delta: &Self::D) -> Self;
}

/// Serialize types and values to binary sequence of bytes.
pub trait Serialize: Sized {
    /// Convert this value into binary equivalent. Encoded bytes shall
    /// appended to the input-buffer `buf`. Return bytes encoded.
    fn encode(&self, buf: &mut Vec<u8>) -> usize;

    /// Reverse process of encode, given the binary equivalent, `buf`,
    /// of a value, construct self.
    fn decode(&mut self, buf: &[u8]) -> Result<usize>;
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

    #[allow(dead_code)] // TODO: remove this once bogn is weaved-up.
    pub(crate) fn into_upserted(self) -> Option<(vlog::Delta<V>, u64)> {
        match self.data {
            InnerDelta::U { delta, seqno } => Some((delta, seqno)),
            InnerDelta::D { .. } => None,
        }
    }

    #[allow(dead_code)] // TODO: remove this once bogn is weaved-up.
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
}

impl<V> AsRef<InnerDelta<V>> for Delta<V>
where
    V: Clone + Diff,
{
    fn as_ref(&self) -> &InnerDelta<V> {
        &self.data
    }
}

/// Read methods.
impl<V> Delta<V>
where
    V: Clone + Diff,
{
    /// Return the underlying `difference` value for this delta.
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
    #[allow(dead_code)] // TODO: remove if not required.
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
}

#[derive(Clone)]
pub(crate) enum Value<V>
where
    V: Clone + Diff,
{
    U { value: vlog::Value<V>, seqno: u64 },
    D { seqno: u64 },
}

impl<V> Value<V>
where
    V: Clone + Diff,
{
    pub(crate) fn new_upsert(value: vlog::Value<V>, seqno: u64) -> Value<V> {
        Value::U { value, seqno }
    }

    pub(crate) fn new_upsert_value(value: V, seqno: u64) -> Value<V> {
        let value = vlog::Value::new_native(value);
        Value::U { value, seqno }
    }

    pub(crate) fn new_delete(seqno: u64) -> Value<V> {
        Value::D { seqno }
    }

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
            Value::U {
                value: vlog::Value::Reference { .. },
                ..
            } => true,
            _ => false,
        }
    }
}

/// Entry is the covering structure for a {Key, value} pair
/// indexed by bogn data structures.
///
/// It is a user facing structure, also used in stitching together
/// different components of Bogn.
#[derive(Clone)]
pub struct Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    key: K,
    value: Box<Value<V>>,
    deltas: Vec<Delta<V>>,
}

// Entry construction methods.
impl<K, V> Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    pub const KEY_SIZE_LIMIT: usize = 1024 * 1024 * 1024; // 1GB
    pub const DIFF_SIZE_LIMIT: usize = 1024 * 1024 * 1024 * 1024; // 1TB
    pub const VALUE_SIZE_LIMIT: usize = 1024 * 1024 * 1024 * 1024; // 1TB

    pub(crate) fn new(key: K, value: Box<Value<V>>) -> Entry<K, V> {
        Entry {
            key,
            value,
            deltas: vec![],
        }
    }

    pub(crate) fn set_deltas(&mut self, deltas: Vec<Delta<V>>) {
        self.deltas = deltas;
    }
}

// write/update methods.
impl<K, V> Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
    // Prepend a new version, also the latest version, for this entry.
    // In non-lsm mode this is equivalent to over-writing previous value.
    pub(crate) fn prepend_version(&mut self, new_entry: Self, lsm: bool) {
        if lsm {
            self.prepend_version_lsm(new_entry)
        } else {
            self.prepend_version_nolsm(new_entry)
        }
    }

    fn prepend_version_nolsm(&mut self, new_entry: Self) {
        self.value = new_entry.value.clone();
    }

    fn prepend_version_lsm(&mut self, new_entry: Self) {
        match self.value.as_ref() {
            Value::D { seqno } => {
                self.deltas.insert(0, Delta::new_delete(*seqno));
            }
            Value::U {
                value: vlog::Value::Native { value },
                seqno,
            } => {
                let delta = {
                    let d = new_entry.to_native_value().unwrap().diff(value);
                    vlog::Delta::new_native(d)
                };
                self.deltas.insert(0, Delta::new_upsert(delta, *seqno));
            }
            Value::U {
                value: vlog::Value::Reference { .. },
                ..
            } => unreachable!(),
        }
        self.prepend_version_nolsm(new_entry)
    }

    // only lsm, if entry is already deleted this call becomes a no-op.
    pub(crate) fn delete(&mut self, seqno: u64) {
        match self.value.as_ref() {
            Value::D { .. } => (), // NOOP
            Value::U {
                value: vlog::Value::Native { value },
                seqno,
            } => {
                let delta = {
                    let d: <V as Diff>::D = From::from(value.clone());
                    vlog::Delta::new_native(d)
                };
                self.deltas.insert(0, Delta::new_upsert(delta, *seqno));
            }
            Value::U {
                value: vlog::Value::Reference { .. },
                ..
            } => unreachable!(),
        }
        *self.value = Value::D { seqno };
    }

    // purge all versions whose seqno < or <= ``cutoff``.
    pub(crate) fn purge(mut self, cutoff: Bound<u64>) -> Option<Entry<K, V>> {
        let e = self.to_seqno();
        // If all versions of this entry are before cutoff, then purge entry
        match cutoff {
            Bound::Included(cutoff) if e < cutoff => return None,
            Bound::Excluded(cutoff) if e <= cutoff => return None,
            _ => (),
        }
        // Otherwise, purge only those versions that are before cutoff
        self.deltas = self
            .deltas
            .into_iter()
            .take_while(|d| {
                let seqno = d.to_seqno();
                match cutoff {
                    Bound::Included(cutoff) if seqno >= cutoff => true,
                    Bound::Excluded(cutoff) if seqno > cutoff => true,
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
    V: Clone + Diff + From<<V as Diff>::D>,
{
    // Pick all versions whose seqno is within the specified range.
    // Note that, by bogn-design only memory-indexes ingesting new
    // mutations are subjected to this filter function.
    pub(crate) fn filter_within(
        &self,
        start: Bound<u64>, // filter from
        end: Bound<u64>,   // filter till
    ) -> Option<Entry<K, V>> {
        // skip versions newer than requested range.
        let entry = self.skip_till(start.clone(), end)?;
        // purge versions older than request range.
        entry.purge(match start {
            Bound::Included(x) => Bound::Included(x),
            Bound::Excluded(x) => Bound::Excluded(x),
            Bound::Unbounded => Bound::Unbounded,
        })
    }

    fn skip_till(&self, sb: Bound<u64>, eb: Bound<u64>) -> Option<Entry<K, V>> {
        use std::ops::Bound::{Excluded, Included};

        // skip entire entry if it is before the specified range.
        let e = self.to_seqno();
        match sb {
            Included(s_seqno) if e < s_seqno => return None,
            Excluded(s_seqno) if e <= s_seqno => return None,
            _ => (),
        }
        // skip the entire entry if it is after the specified range.
        let s = self.deltas.last().map_or(e, |d| d.to_seqno());
        match eb {
            Included(e_seqno) if s > e_seqno => return None,
            Excluded(e_seqno) if s >= e_seqno => return None,
            Included(e_seqno) if e <= e_seqno => return Some(self.clone()),
            Included(e_seqno) if e < e_seqno => return Some(self.clone()),
            _ => (),
        };

        // partial skip.
        let mut entry = self.clone();
        let mut iter = entry.deltas.into_iter();
        while let Some(delta) = iter.next() {
            let value = entry.value.to_native_value();
            let (value, _) = next_value(value, delta.data);
            entry.value = Box::new(value);
            let seqno = entry.value.to_seqno();
            let ok = match eb {
                Included(e_seqno) if seqno <= e_seqno => true,
                Excluded(e_seqno) if seqno < e_seqno => true,
                _ => false,
            };
            if ok {
                // collect the remaining deltas and return
                entry.deltas = iter.collect();
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
    K: Clone + Ord,
    V: Clone + Diff + From<<V as Diff>::D>,
{
    // Merge two version chain for same entry. This can happen between
    // two entries from memory-index and disk-index, or disk-index and
    // disk-index. In either case it is expected that all versions of
    // one entry shall either be greater than all versions of the other entry.
    pub(crate) fn lsm_merge(self, entry: Entry<K, V>) -> Entry<K, V> {
        // `a` is newer than `b`, and all versions in a and b are mutually
        // exclusive in seqno ordering.
        let (a, mut b) = if self.to_seqno() > entry.to_seqno() {
            (self, entry)
        } else if entry.to_seqno() > self.to_seqno() {
            (entry, self)
        } else {
            unreachable!()
        };
        // TODO remove this validation logic once bogn is fully stable.
        a.validate_lsm_merge(&b);
        for ne in a.versions().collect::<Vec<Entry<K, V>>>().into_iter().rev() {
            b.prepend_version(ne, true /* lsm */);
        }
        b
    }

    // `self` is newer than `entr`
    fn validate_lsm_merge(&self, entr: &Entry<K, V>) {
        // validate ordering
        let mut seqnos = vec![self.to_seqno()];
        self.deltas.iter().for_each(|d| seqnos.push(d.to_seqno()));
        seqnos.push(entr.to_seqno());
        entr.deltas.iter().for_each(|d| seqnos.push(d.to_seqno()));
        let mut fail = seqnos[0..seqnos.len() - 1]
            .into_iter()
            .zip(seqnos[1..].into_iter())
            .any(|(a, b)| a <= b);
        // validate self contains all native value and deltas.
        fail = fail || self.value.is_reference();
        fail = fail || self.deltas.iter().any(|d| d.is_reference());

        if fail {
            unreachable!()
        }
    }
}

// read methods.
impl<K, V> Entry<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff,
{
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

    /// Return ownership of key.
    #[inline]
    pub fn to_key(&self) -> K {
        self.key.clone()
    }

    /// Return a reference to key.
    #[inline]
    pub fn as_key(&self) -> &K {
        &self.key
    }

    /// Return value.
    pub fn to_native_value(&self) -> Option<V> {
        self.value.to_native_value()
    }

    /// Return the latest seqno that created/updated/deleted this entry.
    #[inline]
    pub fn to_seqno(&self) -> u64 {
        match self.value.as_ref() {
            Value::U { seqno, .. } => *seqno,
            Value::D { seqno, .. } => *seqno,
        }
    }

    /// Return the seqno and the state of modification. `true` means
    /// latest value was a create/update, and `false` means latest value
    /// was deleted.
    #[inline]
    pub fn to_seqno_state(&self) -> (bool, u64) {
        match &self.value.as_ref() {
            Value::U { seqno, .. } => (true, *seqno),
            Value::D { seqno, .. } => (false, *seqno),
        }
    }

    /// Return whether this entry is in deleted state, applicable onle
    /// in lsm mode.
    pub fn is_deleted(&self) -> bool {
        self.value.is_deleted()
    }

    /// Return the previous versions of this entry as Deltas.
    #[inline]
    pub(crate) fn to_deltas(&self) -> Vec<Delta<V>> {
        self.deltas.clone()
    }
}

/// Iterate from latest to oldest available version for this entry.
pub struct VersionIter<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff + From<<V as Diff>::D>,
{
    key: K,
    entry: Option<Entry<K, V>>,
    curval: Option<V>,
    deltas: Option<std::vec::IntoIter<Delta<V>>>,
}

impl<K, V> Iterator for VersionIter<K, V>
where
    K: Clone + Ord,
    V: Clone + Diff + From<<V as Diff>::D>,
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
        Some(Entry::new(self.key.clone(), Box::new(value)))
    }
}

fn next_value<V>(value: Option<V>, delta: InnerDelta<V>) -> (Value<V>, Option<V>)
where
    V: Clone + Diff + From<<V as Diff>::D>,
{
    match (value, delta) {
        (None, InnerDelta::D { .. }) => {
            panic!("consecutive versions can't be a delete");
        }
        (Some(_), InnerDelta::D { seqno }) => {
            // this entry is deleted.
            (Value::new_delete(seqno), None)
        }
        (None, InnerDelta::U { delta, seqno }) => {
            // previous entry was a delete.
            let nv: V = From::from(delta.into_native_delta().unwrap());
            let v = vlog::Value::new_native(nv.clone());
            let value = Value::new_upsert(v, seqno);
            (value, Some(nv))
        }
        (Some(curval), InnerDelta::U { delta, seqno }) => {
            // this and previous entry are create/update.
            let nv = curval.merge(&delta.into_native_delta().unwrap());
            let v = vlog::Value::new_native(nv.clone());
            let value = Value::new_upsert(v, seqno);
            (value, Some(nv))
        }
    }
}
