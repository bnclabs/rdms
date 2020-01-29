use std::{
    borrow::Borrow,
    collections::hash_map::RandomState,
    convert::{self, TryInto},
    ffi, fmt, fs,
    hash::{BuildHasher, Hash, Hasher},
    result,
    sync::{atomic::AtomicU64, atomic::Ordering::SeqCst, Arc},
};

use crate::{
    core::{Diff, Replay, Result, Serialize},
    dlog::{Dlog, DlogState, OpRequest, OpResponse},
    dlog_entry::DEntry,
    dlog_journal::Shard,
    error::Error,
    thread as rt, util,
};

pub struct Wal<K, V, H>
where
    K: 'static + Send + Clone + Default + Serialize,
    V: 'static + Send + Clone + Default + Serialize,
    H: Clone + BuildHasher,
{
    dir: ffi::OsString,
    name: String,
    hash_builder: H,

    index: Arc<AtomicU64>, // seqno
    threads: Vec<rt::Thread<OpRequest<Op<K, V>>, OpResponse, Shard<State, Op<K, V>>>>,
}

impl<K, V> From<Dlog<State, Op<K, V>>> for Wal<K, V, RandomState>
where
    K: 'static + Send + Clone + Default + Serialize,
    V: 'static + Send + Clone + Default + Serialize,
{
    fn from(dl: Dlog<State, Op<K, V>>) -> Wal<K, V, RandomState> {
        let mut wl = Wal {
            dir: dl.dir,
            name: dl.name,

            hash_builder: RandomState::new(),
            index: dl.index,
            threads: Default::default(),
        };

        for shard in dl.shards {
            wl.threads.push(shard.into_thread())
        }

        wl
    }
}

impl<K, V, H> fmt::Debug for Wal<K, V, H>
where
    K: 'static + Send + Clone + Default + Serialize,
    V: 'static + Send + Clone + Default + Serialize,
    H: Clone + BuildHasher,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        write!(f, "Wal<{:?},{}>", self.dir, self.name)
    }
}

impl<K, V, H> Wal<K, V, H>
where
    K: 'static + Send + Clone + Default + Ord + Hash + Serialize,
    V: 'static + Send + Clone + Default + Diff + Serialize,
    H: Clone + BuildHasher,
{
    pub fn set_hasher(&mut self, hash_builder: H) -> &mut Self {
        self.hash_builder = hash_builder;
        self
    }

    /// Purge all journal files whose ``last_index`` is  less than ``before``.
    pub fn purge_till(&mut self, before: u64) -> Result<u64> {
        for thread in self.threads.iter() {
            thread.request(OpRequest::new_purge_till(before))?;
        }

        Ok(before)
    }

    /// Purge this ``Wal`` instance and all its memory and disk footprints.
    pub fn purge(self) -> Result<u64> {
        for thread in self.threads.into_iter() {
            let shard = thread.close_wait()?;
            shard.purge()?;
        }

        Ok(self.index.load(SeqCst))
    }

    /// Close the [`Wal`] instance. It is possible to get back the [`Wal`]
    /// instance using the [`Wal::load`] constructor. To purge the instance use
    /// [`Wal::purge`] api.
    pub fn close(&mut self) -> Result<u64> {
        for thread in self.threads.drain(..).into_iter() {
            let shard = thread.close_wait()?;
            shard.close()?;
        }

        Ok(self.index.load(SeqCst))
    }

    pub fn to_writer(&mut self) -> Result<Writer<K, V, H>> {
        Ok(Writer {
            hash_builder: self.hash_builder.clone(),
            shards: self
                .threads
                .iter()
                .map(|thread| thread.to_writer())
                .collect(),
        })
    }
}

impl<K, V, H> Wal<K, V, H>
where
    K: 'static + Send + Clone + Default + Ord + Hash + Serialize,
    V: 'static + Send + Clone + Default + Diff + Serialize,
    H: Clone + BuildHasher,
{
    /// When DB suffer a crash and looses latest set of mutations, [`Wal`]
    /// can be used to fetch the latest set of mutations and replay them on
    /// DB. Return total number of operations replayed on DB.
    pub fn replay<P>(self, db: &mut P, seqno: u64) -> Result<usize>
    where
        P: Replay<K, V>,
    {
        // validate
        if self.is_active() {
            let msg = format!("cannot replay with active shards");
            return Err(Error::InvalidWAL(msg));
        }

        let mut ops = 0;

        for thread in self.threads.into_iter() {
            let journals = thread.close_wait()?.into_journals();
            for journal in journals.into_iter() {
                if journal.is_cold() {
                    continue;
                }
                match journal.to_last_index() {
                    Some(index) if index < seqno => continue,
                    _ => (),
                }

                let mut fd = {
                    let file_path = journal.to_file_path();
                    let mut opts = fs::OpenOptions::new();
                    opts.read(true).write(false).open(file_path)?
                };

                for batch in journal.into_batches()? {
                    match batch.to_last_index() {
                        Some(index) if index < seqno => continue,
                        _ => (),
                    }
                    for entry in batch.into_active(&mut fd)?.into_entries() {
                        let (index, op) = entry.into_index_op();
                        match op {
                            Op::Set { key, value } => {
                                db.set_index(key, value, index)?;
                            }
                            Op::SetCAS { key, value, cas } => {
                                db.set_cas_index(key, value, cas, index)?;
                            }
                            Op::Delete { key } => {
                                db.delete_index(key, index)?;
                            }
                        }
                        ops += 1;
                    }
                }
            }
        }

        Ok(ops)
    }

    fn is_active(&self) -> bool {
        self.threads
            .iter()
            .map(|thread| thread.ref_count() > 1)
            .any(convert::identity)
    }
}

/// Writer handle for [`Wal`] instance.
pub struct Writer<K, V, H>
where
    K: 'static + Send + Default + Hash + Serialize,
    V: 'static + Send + Default + Serialize,
    H: BuildHasher,
{
    hash_builder: H,
    shards: Vec<rt::Writer<OpRequest<Op<K, V>>, OpResponse>>,
}

impl<K, V, H> Writer<K, V, H>
where
    K: 'static + Send + Default + Hash + Serialize,
    V: 'static + Send + Default + Serialize,
    H: BuildHasher,
{
    /// Append ``set`` operation into the log. Return the sequence-no
    /// for this mutation.
    pub fn set(&mut self, key: K, value: V) -> Result<u64> {
        let shard = self.as_shard(&key);

        let op = Op::new_set(key, value);
        match shard.request(OpRequest::new_op(op))? {
            OpResponse::Index(index) => Ok(index),
            _ => unreachable!(),
        }
    }

    /// Append ``set_cas`` operation into the log. Return the sequence-no
    /// for this mutation.
    pub fn set_cas(&mut self, key: K, value: V, cas: u64) -> Result<u64> {
        let shard = self.as_shard(&key);

        let op = Op::new_set_cas(key, value, cas);
        match shard.request(OpRequest::new_op(op))? {
            OpResponse::Index(index) => Ok(index),
            _ => unreachable!(),
        }
    }

    /// Append ``delete`` operation into the log. Return the sequence-no
    /// for this mutation.
    pub fn delete<Q>(&mut self, key: &Q) -> Result<u64>
    where
        K: Borrow<Q>,
        Q: ToOwned<Owned = K> + ?Sized,
    {
        let key: K = key.to_owned();

        let shard = self.as_shard(&key);

        let op = Op::new_delete(key);
        match shard.request(OpRequest::new_op(op))? {
            OpResponse::Index(index) => Ok(index),
            _ => unreachable!(),
        }
    }

    fn as_shard<'a>(&'a mut self, key: &K) -> &'a mut rt::Writer<OpRequest<Op<K, V>>, OpResponse> {
        let hash = {
            let mut hasher = self.hash_builder.build_hasher();
            key.hash(&mut hasher);
            hasher.finish()
        };

        let n: u64 = self.shards.len().try_into().unwrap();
        let n: usize = (hash % n).try_into().unwrap();
        &mut self.shards[n]
    }
}

#[derive(Clone, Default, PartialEq)]
pub(crate) struct State;

impl<K, V> DlogState<Op<K, V>> for State
where
    K: 'static + Send + Default + Serialize,
    V: 'static + Send + Default + Serialize,
{
    type Key = K;
    type Val = V;

    fn on_add_entry(&mut self, _entry: &DEntry<Op<K, V>>) -> () {
        ()
    }

    fn to_type(&self) -> String {
        "wal".to_string()
    }
}

impl Serialize for State {
    fn encode(&self, _buf: &mut Vec<u8>) -> Result<usize> {
        Ok(0)
    }

    fn decode(&mut self, _buf: &[u8]) -> Result<usize> {
        Ok(0)
    }
}
#[derive(PartialEq, Debug)]
enum OpType {
    // Data operations
    Set = 1,
    SetCAS,
    Delete,
    // Config operations
    // TBD
}

impl From<u64> for OpType {
    fn from(value: u64) -> OpType {
        match value {
            1 => OpType::Set,
            2 => OpType::SetCAS,
            3 => OpType::Delete,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone)]
pub(crate) enum Op<K, V>
where
    K: 'static + Send + Default + Serialize,
    V: 'static + Send + Default + Serialize,
{
    // Data operations
    Set { key: K, value: V },
    SetCAS { key: K, value: V, cas: u64 },
    Delete { key: K },
}

impl<K, V> Default for Op<K, V>
where
    K: 'static + Send + Default + Serialize,
    V: 'static + Send + Default + Serialize,
{
    fn default() -> Self {
        Op::Delete {
            key: Default::default(),
        }
    }
}

impl<K, V> PartialEq for Op<K, V>
where
    K: 'static + Send + PartialEq + Default + Serialize,
    V: 'static + Send + PartialEq + Default + Serialize,
{
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Op::Set {
                    key: key1,
                    value: value1,
                },
                Op::Set {
                    key: key2,
                    value: value2,
                },
            ) => key1 == key2 && value1 == value2,
            (
                Op::SetCAS { key, value, cas },
                Op::SetCAS {
                    key: k,
                    value: v,
                    cas: c,
                },
            ) => key.eq(k) && value.eq(v) && cas.eq(c),
            (Op::Delete { key }, Op::Delete { key: k }) => key == k,
            _ => false,
        }
    }
}

impl<K, V> fmt::Debug for Op<K, V>
where
    K: 'static + Send + Default + Serialize + fmt::Debug,
    V: 'static + Send + Default + Serialize + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        match self {
            Op::Set { key: k, value: v } => {
                write!(f, "<Op::Set<key: {:?} value: {:?}>", k, v)?;
            }
            Op::SetCAS {
                key: k,
                value: v,
                cas,
            } => {
                write!(f, "Op::Set<key:{:?} val:{:?} cas:{}>", k, v, cas)?;
            }
            Op::Delete { key } => {
                write!(f, "Op::Set< key: {:?}>", key)?;
            }
        }
        Ok(())
    }
}

impl<K, V> Op<K, V>
where
    K: 'static + Send + Default + Serialize,
    V: 'static + Send + Default + Serialize,
{
    pub(crate) fn new_set(key: K, value: V) -> Op<K, V> {
        Op::Set { key, value }
    }

    pub(crate) fn new_set_cas(key: K, value: V, cas: u64) -> Op<K, V> {
        Op::SetCAS { cas, key, value }
    }

    pub(crate) fn new_delete(key: K) -> Op<K, V> {
        Op::Delete { key }
    }

    fn op_type(buf: &[u8]) -> Result<OpType> {
        util::check_remaining(buf, 8, "wal op-type")?;
        let hdr1 = u64::from_be_bytes(buf[..8].try_into()?);
        Ok(((hdr1 >> 32) & 0x00FFFFFF).into())
    }
}

impl<K, V> Serialize for Op<K, V>
where
    K: 'static + Send + Default + Serialize,
    V: 'static + Send + Default + Serialize,
{
    fn encode(&self, buf: &mut Vec<u8>) -> Result<usize> {
        Ok(match self {
            Op::Set { key, value } => {
                let n = Self::encode_set(buf, key, value)?;
                n
            }
            Op::SetCAS { key, value, cas } => {
                let n = Self::encode_set_cas(buf, key, value, *cas)?;
                n
            }
            Op::Delete { key } => {
                let n = Self::encode_delete(buf, key)?;
                n
            }
        })
    }

    fn decode(&mut self, buf: &[u8]) -> Result<usize> {
        let key: K = Default::default();
        *self = match Self::op_type(buf)? {
            OpType::Set => Op::new_set(key, Default::default()),
            OpType::SetCAS => Op::new_set_cas(key, Default::default(), Default::default()),
            OpType::Delete => Op::new_delete(key),
        };

        match self {
            Op::Set { key, value } => Self::decode_set(buf, key, value),
            Op::SetCAS { key, value, cas } => Self::decode_set_cas(buf, key, value, cas),
            Op::Delete { key } => Self::decode_delete(buf, key),
        }
    }
}

// +--------------------------------+-------------------------------+
// | reserved |         op-type     |       key-len                 |
// +--------------------------------+-------------------------------+
// |                            value-len                           |
// +----------------------------------------------------------------+
// |                               key                              |
// +----------------------------------------------------------------+
// |                              value                             |
// +----------------------------------------------------------------+
//
// reserved:  bits 63, 62, 61, 60, 59, 58, 57, 56
// op-type:   24-bit
// key-len:   32-bit
// value-len: 64-bit
//
impl<K, V> Op<K, V>
where
    K: 'static + Send + Default + Serialize,
    V: 'static + Send + Default + Serialize,
{
    fn encode_set(buf: &mut Vec<u8>, key: &K, value: &V) -> Result<usize> {
        let n = buf.len();
        buf.resize(n + 16, 0);

        let klen: u64 = key.encode(buf)?.try_into()?;
        let hdr1: u64 = ((OpType::Set as u64) << 32) | klen;
        let vlen: u64 = value.encode(buf)?.try_into()?;

        buf[n..n + 8].copy_from_slice(&hdr1.to_be_bytes());
        buf[n + 8..n + 16].copy_from_slice(&vlen.to_be_bytes());

        Ok((klen + vlen + 16).try_into()?)
    }

    fn decode_set(buf: &[u8], k: &mut K, v: &mut V) -> Result<usize> {
        let mut n = 16;
        let (klen, vlen) = {
            util::check_remaining(buf, 16, "wal op-set-hdr")?;
            let hdr1 = u64::from_be_bytes(buf[..8].try_into()?);
            let klen: usize = (hdr1 & 0xFFFFFFFF).try_into()?;
            let vlen = u64::from_be_bytes(buf[8..16].try_into()?);
            let vlen: usize = vlen.try_into()?;
            (klen, vlen)
        };

        n += {
            util::check_remaining(buf, n + klen, "wal op-set-key")?;
            k.decode(&buf[n..n + klen])?;
            klen
        };

        n += {
            util::check_remaining(buf, n + vlen, "wal op-set-value")?;
            v.decode(&buf[n..n + vlen])?;
            vlen
        };

        Ok(n)
    }
}

// +--------------------------------+-------------------------------+
// | reserved |         op-type     |       key-len                 |
// +--------------------------------+-------------------------------+
// |                            value-len                           |
// +--------------------------------+-------------------------------+
// |                               cas                              |
// +----------------------------------------------------------------+
// |                               key                              |
// +----------------------------------------------------------------+
// |                              value                             |
// +----------------------------------------------------------------+
//
// reserved:  bits 63, 62, 61, 60, 59, 58, 57, 56
// op-type:   24-bit
// key-len:   32-bit
// value-len: 64-bit
//
impl<K, V> Op<K, V>
where
    K: 'static + Send + Default + Serialize,
    V: 'static + Send + Default + Serialize,
{
    fn encode_set_cas(
        buf: &mut Vec<u8>,
        key: &K,
        value: &V,
        cas: u64, // cas is seqno
    ) -> Result<usize> {
        let n = buf.len();
        buf.resize(n + 24, 0);

        let klen: u64 = key.encode(buf)?.try_into()?;
        let hdr1: u64 = ((OpType::SetCAS as u64) << 32) | klen;
        let vlen: u64 = value.encode(buf)?.try_into()?;

        buf[n..n + 8].copy_from_slice(&hdr1.to_be_bytes());
        buf[n + 8..n + 16].copy_from_slice(&vlen.to_be_bytes());
        buf[n + 16..n + 24].copy_from_slice(&cas.to_be_bytes());

        Ok((klen + vlen + 24).try_into()?)
    }

    fn decode_set_cas(
        buf: &[u8],
        key: &mut K,
        value: &mut V,
        cas: &mut u64, // reference
    ) -> Result<usize> {
        let mut n = 24;
        let (klen, vlen, cas_seqno) = {
            util::check_remaining(buf, n, "wal op-setcas-hdr")?;
            let hdr1 = u64::from_be_bytes(buf[..8].try_into()?);
            let klen: usize = (hdr1 & 0xFFFFFFFF).try_into()?;
            let vlen = u64::from_be_bytes(buf[8..16].try_into()?);
            let vlen: usize = vlen.try_into()?;
            let cas = u64::from_be_bytes(buf[16..24].try_into()?);
            (klen, vlen, cas)
        };
        *cas = cas_seqno;

        n += {
            util::check_remaining(buf, n + klen, "wal op-setcas-key")?;
            key.decode(&buf[n..n + klen])?;
            klen
        };

        n += {
            util::check_remaining(buf, n + vlen, "wal op-setcas-value")?;
            value.decode(&buf[n..n + vlen])?;
            vlen
        };

        Ok(n)
    }
}

// +--------------------------------+-------------------------------+
// | reserved |         op-type     |       key-len                 |
// +----------------------------------------------------------------+
// |                               key                              |
// +----------------------------------------------------------------+
//
// reserved: bits 63, 62, 61, 60, 59, 58, 57, 56
// op-type:  24-bit
// key-len:  32-bit
//
impl<K, V> Op<K, V>
where
    K: 'static + Send + Default + Serialize,
    V: 'static + Send + Default + Serialize,
{
    fn encode_delete(buf: &mut Vec<u8>, key: &K) -> Result<usize> {
        let n = buf.len();
        buf.resize(n + 8, 0);

        let klen = {
            let klen: u64 = key.encode(buf)?.try_into()?;
            let hdr1: u64 = ((OpType::Delete as u64) << 32) | klen;
            buf[n..n + 8].copy_from_slice(&hdr1.to_be_bytes());
            klen
        };

        Ok((klen + 8).try_into()?)
    }

    fn decode_delete(buf: &[u8], key: &mut K) -> Result<usize> {
        let mut n = 8;
        let klen: usize = {
            util::check_remaining(buf, n, "wal op-delete-hdr1")?;
            let hdr1 = u64::from_be_bytes(buf[..n].try_into()?);
            (hdr1 & 0xFFFFFFFF).try_into()?
        };

        n += {
            util::check_remaining(buf, n + klen, "wal op-delete-key")?;
            key.decode(&buf[n..n + klen])?;
            klen
        };

        Ok(n)
    }
}

//#[cfg(test)]
//#[path = "wal_test.rs"]
//mod wal_test;
