use std::{convert::TryInto, fmt, mem, result};

use crate::{
    core::{Result, Serialize},
    util,
};

enum EntryType {
    Term = 1,
    Client,
}

impl From<u64> for EntryType {
    fn from(value: u64) -> EntryType {
        match value {
            1 => EntryType::Term,
            2 => EntryType::Client,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone)]
pub(crate) enum Entry<K, V>
where
    K: Serialize,
    V: Serialize,
{
    Term {
        // Term in which the entry is created.
        term: u64,
        // Index seqno for this entry. This will be monotonically
        // increasing number.
        index: u64,
        // Operation on host data structure.
        op: Op<K, V>,
    },
    Client {
        // Term in which the entry is created.
        term: u64,
        // Index seqno for this entry. This will be monotonically
        // increasing number.
        index: u64,
        // Id of client applying this entry. To deal with false negatives.
        id: u64,
        // Client seqno monotonically increased by client. To deal
        // with false negatives.
        ceqno: u64,
        // Operation on host data structure.
        op: Op<K, V>,
    },
}

impl<K, V> PartialEq for Entry<K, V>
where
    K: PartialEq + Serialize,
    V: PartialEq + Serialize,
{
    fn eq(&self, other: &Entry<K, V>) -> bool {
        match (self, other) {
            (
                Entry::Term {
                    term: t1,
                    index: i1,
                    op: op1,
                },
                Entry::Term {
                    term: t2,
                    index: i2,
                    op: op2,
                },
            ) => t1 == t2 && i1 == i2 && op1.eq(&op2),
            (
                Entry::Client {
                    term: t1,
                    index: i1,
                    id: id1,
                    ceqno: n1,
                    op: op1,
                },
                Entry::Client {
                    term: t2,
                    index: i2,
                    id: id2,
                    ceqno: n2,
                    op: op2,
                },
            ) => t1 == t2 && i1 == i2 && id1 == id2 && n1 == n2 && op1.eq(&op2),
            _ => false,
        }
    }
}

impl<K, V> fmt::Debug for Entry<K, V>
where
    K: Serialize + fmt::Debug,
    V: Serialize + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        match self {
            Entry::Term { term, index, op } => write!(
                f,
                "Entry::Term<term: {} index: {}  op: {:?}>",
                term, index, op
            ),
            Entry::Client {
                term,
                index,
                id,
                ceqno,
                op,
            } => write!(
                f,
                "Entry::Term<term: {} index: {}  id: {} ceqno: {} op: {:?}>",
                term, index, id, ceqno, op
            ),
        }
    }
}

impl<K, V> Entry<K, V>
where
    K: Serialize,
    V: Serialize,
{
    pub(crate) fn new_term(op: Op<K, V>, term: u64, index: u64) -> Entry<K, V> {
        Entry::Term { op, term, index }
    }

    pub(crate) fn new_client(
        op: Op<K, V>,
        term: u64,
        index: u64,
        id: u64,    // client id
        ceqno: u64, // client seqno
    ) -> Entry<K, V> {
        Entry::Client {
            op,
            term,
            index,
            id,
            ceqno,
        }
    }

    fn entry_type(buf: &[u8]) -> Result<EntryType> {
        util::check_remaining(buf, 8, "wal entry-type")?;
        let hdr1 = u64::from_be_bytes(buf[..8].try_into()?);
        Ok((hdr1 & 0x00000000000000FF).into())
    }

    pub(crate) fn to_index(&self) -> u64 {
        match self {
            Entry::Term { index, .. } => *index,
            Entry::Client { index, .. } => *index,
        }
    }

    pub(crate) fn into_op(self) -> Op<K, V> {
        match self {
            Entry::Term { op, .. } => op,
            Entry::Client { op, .. } => op,
        }
    }
}

impl<K, V> Serialize for Entry<K, V>
where
    K: Serialize,
    V: Serialize,
{
    fn encode(&self, buf: &mut Vec<u8>) -> Result<usize> {
        Ok(match self {
            Entry::Term { op, term, index } => {
                let n = Self::encode_term(op, *term, *index, buf)?;
                n
            }
            Entry::Client {
                op,
                term,
                index,
                id,
                ceqno,
            } => {
                let n = Self::encode_client(op, *term, *index, *id, *ceqno, buf)?;
                n
            }
        })
    }

    fn decode(&mut self, buf: &[u8]) -> Result<usize> {
        *self = match Self::entry_type(buf)? {
            EntryType::Term => {
                let op: Op<K, V> = unsafe { mem::zeroed() };
                let term: u64 = unsafe { mem::zeroed() };
                let index: u64 = unsafe { mem::zeroed() };
                Self::new_term(op, term, index)
            }
            EntryType::Client => {
                let op: Op<K, V> = unsafe { mem::zeroed() };
                let term: u64 = unsafe { mem::zeroed() };
                let index: u64 = unsafe { mem::zeroed() };
                let id: u64 = unsafe { mem::zeroed() };
                let ceqno: u64 = unsafe { mem::zeroed() };
                Self::new_client(op, term, index, id, ceqno)
            }
        };

        match self {
            Entry::Term { term, index, op } => {
                let res = Self::decode_term(buf, op, term, index);
                res
            }
            Entry::Client {
                op,
                term,
                index,
                id,
                ceqno,
            } => {
                let res = Self::decode_client(buf, op, term, index, id, ceqno);
                res
            }
        }
    }
}

// +------------------------------------------------------+---------+
// |                            reserved                  |   type  |
// +----------------------------------------------------------------+
// |                            term                                |
// +----------------------------------------------------------------+
// |                            index                               |
// +----------------------------------------------------------------+
// |                         entry-bytes                            |
// +----------------------------------------------------------------+
impl<K, V> Entry<K, V>
where
    K: Serialize,
    V: Serialize,
{
    fn encode_term(
        op: &Op<K, V>, // op
        term: u64,
        index: u64,
        buf: &mut Vec<u8>,
    ) -> Result<usize> {
        buf.extend_from_slice(&(EntryType::Term as u64).to_be_bytes());
        buf.extend_from_slice(&term.to_be_bytes());
        buf.extend_from_slice(&index.to_be_bytes());
        Ok(24 + op.encode(buf)?)
    }

    fn decode_term(
        buf: &[u8],
        op: &mut Op<K, V>,
        term: &mut u64,
        index: &mut u64,
    ) -> Result<usize> {
        util::check_remaining(buf, 24, "wal entry-term-hdr")?;
        *term = u64::from_be_bytes(buf[8..16].try_into()?);
        *index = u64::from_be_bytes(buf[16..24].try_into()?);
        Ok(24 + op.decode(&buf[24..])?)
    }
}

// +------------------------------------------------------+---------+
// |                            reserved                  |   type  |
// +----------------------------------------------------------------+
// |                            term                                |
// +----------------------------------------------------------------+
// |                            index                               |
// +----------------------------------------------------------------+
// |                          client-id                             |
// +----------------------------------------------------------------+
// |                         client-seqno                           |
// +----------------------------------------------------------------+
// |                         entry-bytes                            |
// +----------------------------------------------------------------+
impl<K, V> Entry<K, V>
where
    K: Serialize,
    V: Serialize,
{
    fn encode_client(
        op: &Op<K, V>,
        term: u64,
        index: u64,
        id: u64,
        ceqno: u64,
        buf: &mut Vec<u8>,
    ) -> Result<usize> {
        buf.extend_from_slice(&(EntryType::Client as u64).to_be_bytes());
        buf.extend_from_slice(&term.to_be_bytes());
        buf.extend_from_slice(&index.to_be_bytes());
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(&ceqno.to_be_bytes());
        Ok(40 + op.encode(buf)?)
    }

    fn decode_client(
        buf: &[u8],
        op: &mut Op<K, V>,
        term: &mut u64,
        index: &mut u64,
        id: &mut u64,
        ceqno: &mut u64,
    ) -> Result<usize> {
        util::check_remaining(buf, 40, "wal entry-client-hdr")?;
        *term = u64::from_be_bytes(buf[8..16].try_into()?);
        *index = u64::from_be_bytes(buf[16..24].try_into()?);
        *id = u64::from_be_bytes(buf[24..32].try_into()?);
        *ceqno = u64::from_be_bytes(buf[32..40].try_into()?);
        Ok(40 + op.decode(&buf[40..])?)
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
    K: Serialize,
    V: Serialize,
{
    // Data operations
    Set { key: K, value: V },
    SetCAS { key: K, value: V, cas: u64 },
    Delete { key: K },
    // Config operations,
    // TBD
}

impl<K, V> PartialEq for Op<K, V>
where
    K: PartialEq + Serialize,
    V: PartialEq + Serialize,
{
    fn eq(&self, other: &Op<K, V>) -> bool {
        match (self, other) {
            (Op::Set { key: k1, value: v1 }, Op::Set { key: k2, value: v2 }) => {
                k1 == k2 && v1 == v2
            }
            (
                Op::SetCAS {
                    key: k1,
                    value: v1,
                    cas: n1,
                },
                Op::SetCAS {
                    key: k2,
                    value: v2,
                    cas: n2,
                },
            ) => k1 == k2 && v1 == v2 && n1 == n2,
            (Op::Delete { key: k1 }, Op::Delete { key: k2 }) => k1 == k2,
            _ => false,
        }
    }
}

impl<K, V> fmt::Debug for Op<K, V>
where
    K: Serialize + fmt::Debug,
    V: Serialize + fmt::Debug,
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
    K: Serialize,
    V: Serialize,
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
    K: Serialize,
    V: Serialize,
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
        *self = match Self::op_type(buf)? {
            OpType::Set => {
                // key, value
                Op::new_set(unsafe { mem::zeroed() }, unsafe { mem::zeroed() })
            }
            OpType::SetCAS => {
                let key: K = unsafe { mem::zeroed() };
                let value: V = unsafe { mem::zeroed() };
                Op::new_set_cas(key, value, unsafe { mem::zeroed() })
            }
            OpType::Delete => {
                // key
                Op::new_delete(unsafe { mem::zeroed() })
            }
        };

        match self {
            Op::Set { key, value } => {
                let n = Self::decode_set(buf, key, value);
                n
            }
            Op::SetCAS { key, value, cas } => {
                let n = Self::decode_set_cas(buf, key, value, cas);
                n
            }
            Op::Delete { key } => {
                let n = Self::decode_delete(buf, key);
                n
            }
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
    K: Serialize,
    V: Serialize,
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
    K: Serialize,
    V: Serialize,
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
    K: Serialize,
    V: Serialize,
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

#[cfg(test)]
#[path = "dlog_entry_test.rs"]
mod dlog_entry_test;
