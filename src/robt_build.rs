// TODO: flush put blocks into tx channel. Right now we simply unwrap()

use std::sync::mpsc;
use std::{cmp, fs, io::Write, marker, mem, thread, time};

use crate::core::{Diff, Entry, Serialize};
use crate::error::Error;
use crate::robt_config::{self, Config, MetaItem, MARKER_BLOCK};
use crate::robt_indx::{MBlock, ZBlock};
use crate::robt_stats::Stats;
use crate::util;

pub struct Builder<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Serialize,
{
    config: Config,
    iflusher: Flusher,
    vflusher: Option<Flusher>,
    stats: Stats,

    phantom_key: marker::PhantomData<K>,
    phantom_val: marker::PhantomData<V>,
}

impl<K, V> Builder<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Serialize,
{
    pub fn initial(config: Config) -> Result<Builder<K, V>, Error> {
        let (index_reuse, vlog_reuse) = (false, false);
        let index_file = config.to_index_file();
        let iflusher = Flusher::new(index_file, index_reuse)?;
        let vflusher = match config.to_value_log() {
            Some(vlog_file) => Some(Flusher::new(vlog_file, vlog_reuse)?),
            None => None,
        };

        Ok(Builder {
            config: config.clone(),
            iflusher,
            vflusher,
            stats: From::from(config),
            phantom_key: marker::PhantomData,
            phantom_val: marker::PhantomData,
        })
    }

    pub fn incremental(config: Config) -> Result<Builder<K, V>, Error> {
        let (index_reuse, vlog_reuse) = (false, true);
        let index_file = config.to_index_file();
        let iflusher = Flusher::new(index_file, index_reuse)?;
        let vflusher = match config.to_value_log() {
            Some(vlog_file) => Some(Flusher::new(vlog_file, vlog_reuse)?),
            None => None,
        };

        let mut stats: Stats = From::from(config.clone());
        stats.n_abytes = vflusher
            .as_ref()
            .map_or(Default::default(), |x| x.fpos as usize);

        Ok(Builder {
            config: config.clone(),
            iflusher,
            vflusher,
            stats,
            phantom_key: marker::PhantomData,
            phantom_val: marker::PhantomData,
        })
    }

    pub fn build<I>(mut self, mut iter: I, metadata: Vec<u8>) -> Result<(), Error>
    where
        I: Iterator<Item = Result<Entry<K, V>, Error>>,
    {
        let start = time::SystemTime::now();
        let mut b = BuildData::new(self.stats.n_abytes, self.config.clone());
        while let Some(entry) = iter.next() {
            let mut entry = entry?;
            if self.preprocess_entry(&mut entry) {
                continue; // if true, this entry and all its versions purged
            }

            match b.z.insert(&entry, &mut self.stats) {
                Ok(_) => (),
                Err(Error::ZBlockOverflow(_)) => {
                    let (zbytes, vbytes) = b.z.finalize(&mut self.stats);
                    b.z.flush(&mut self.iflusher, self.vflusher.as_mut());
                    b.update_z_flush(zbytes, vbytes);
                    let mut m = b.mstack.pop().unwrap();
                    if let Err(_) = m.insertz(b.z.as_first_key(), b.z_fpos) {
                        let mbytes = m.finalize(&mut self.stats);
                        m.flush(&mut self.iflusher);
                        b.update_m_flush(mbytes);
                        self.insertms(m.as_first_key(), &mut b)?;
                        m.reset();
                        m.insertz(b.z.as_first_key(), b.z_fpos)?;
                    }
                    b.mstack.push(m);
                    b.reset();
                    b.z.insert(&entry, &mut self.stats)?;
                }
                Err(_) => unreachable!(),
            };
            self.postprocess_entry(&mut entry);
        }

        // flush final set partial blocks
        if self.stats.n_count > 0 {
            self.finalize1(&mut b)?; // flush zblock
            self.finalize2(&mut b)?; // flush mblocks
        }

        // start building metadata items for index files
        let mut meta_items: Vec<MetaItem> = vec![];

        // meta-stats
        self.stats.buildtime = start.elapsed().unwrap().as_nanos() as u64;
        self.stats.epoch = time::UNIX_EPOCH.elapsed().unwrap().as_nanos() as i128;
        let stats = self.stats.to_string();
        if (stats.len() + 8) > self.config.m_blocksize {
            panic!("impossible case");
        }
        meta_items.push(MetaItem::Stats(stats));
        // metadata
        meta_items.push(MetaItem::Metadata(metadata));
        // marker
        meta_items.push(MetaItem::Marker(MARKER_BLOCK.clone()));
        // flush them down
        robt_config::write_meta_items(meta_items, &mut self.iflusher);

        // flush marker block and close
        self.iflusher.close_wait();
        self.vflusher.take().map(|x| x.close_wait());

        Ok(())
    }

    fn insertms(&mut self, key: &K, b: &mut BuildData<K, V>) -> Result<(), Error> {
        let (mut m0, overflow, m_fpos) = match b.mstack.pop() {
            Some(mut m0) => match m0.insertm(key, b.m_fpos) {
                Ok(_) => (m0, false, b.m_fpos),
                Err(_) => (m0, true, b.m_fpos),
            },
            None => {
                let mut m0 = MBlock::new_encode(self.config.clone());
                m0.insertm(key, b.m_fpos)?;
                (m0, false, b.m_fpos)
            }
        };
        if overflow {
            let mbytes = m0.finalize(&mut self.stats);
            m0.flush(&mut self.iflusher);
            b.update_m_flush(mbytes);
            self.insertms(m0.as_first_key(), b)?;
            m0.reset();
            m0.insertm(key, m_fpos)?;
        }
        b.mstack.push(m0);
        Ok(())
    }

    fn finalize1(&mut self, b: &mut BuildData<K, V>) -> Result<(), Error> {
        if b.z.has_first_key() {
            let (zbytes, vbytes) = b.z.finalize(&mut self.stats);
            b.z.flush(&mut self.iflusher, self.vflusher.as_mut());
            b.update_z_flush(zbytes, vbytes);
            let mut m = b.mstack.pop().unwrap();
            if let Err(_) = m.insertz(b.z.as_first_key(), b.z_fpos) {
                let mbytes = m.finalize(&mut self.stats);
                m.flush(&mut self.iflusher);
                b.update_m_flush(mbytes);
                self.insertms(m.as_first_key(), b)?;

                m.reset();
                m.insertz(b.z.as_first_key(), b.z_fpos)?;
                b.reset();
            }
            b.mstack.push(m);
        };
        Ok(())
    }

    fn finalize2(&mut self, b: &mut BuildData<K, V>) -> Result<(), Error> {
        while let Some(mut m) = b.mstack.pop() {
            if m.has_first_key() {
                let mbytes = m.finalize(&mut self.stats);
                m.flush(&mut self.iflusher);
                b.update_m_flush(mbytes);
                self.insertms(m.as_first_key(), b)?;

                b.reset();
            }
        }
        Ok(())
    }

    fn preprocess_entry(&mut self, entry: &mut Entry<K, V>) -> bool {
        self.stats.seqno = cmp::max(self.stats.seqno, entry.to_seqno());
        self.purge_values(entry)
    }

    fn postprocess_entry(&mut self, entry: &mut Entry<K, V>) {
        self.stats.n_count += 1;
        if entry.is_deleted() {
            self.stats.n_deleted += 1;
        }
    }

    fn purge_values(&self, entry: &mut Entry<K, V>) -> bool {
        match self.config.tomb_purge {
            Some(before) => entry.purge(before),
            _ => false,
        }
    }
}

pub struct BuildData<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    z: ZBlock<K, V>,
    mstack: Vec<MBlock<K, V>>,
    fpos: u64,
    z_fpos: u64,
    m_fpos: u64,
    vlog_fpos: u64,
}

impl<K, V> BuildData<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    fn new(n_abytes: usize, config: Config) -> BuildData<K, V> {
        let vlog_fpos = n_abytes as u64;
        let mut obj = BuildData {
            z: ZBlock::new_encode(vlog_fpos, config.clone()),
            mstack: vec![],
            z_fpos: 0,
            m_fpos: 0,
            fpos: 0,
            vlog_fpos,
        };
        obj.mstack.push(MBlock::new_encode(config));
        obj
    }

    fn update_z_flush(&mut self, zbytes: usize, vbytes: usize) {
        self.fpos += zbytes as u64;
        self.vlog_fpos += vbytes as u64;
    }

    fn update_m_flush(&mut self, mbytes: usize) {
        self.m_fpos = self.fpos;
        self.fpos += mbytes as u64;
    }

    fn reset(&mut self) {
        self.z_fpos = self.fpos;
        self.m_fpos = self.fpos;
        self.z.reset(self.vlog_fpos);
    }
}

pub(crate) struct Flusher {
    tx: mpsc::SyncSender<Vec<u8>>,
    handle: thread::JoinHandle<()>,
    fpos: u64,
}

impl Flusher {
    fn new(file: String, reuse: bool) -> Result<Flusher, Error> {
        let fpos = if reuse {
            fs::metadata(&file)?.len()
        } else {
            Default::default()
        };
        let fd = util::open_file_w(&file, reuse)?;

        let (tx, rx) = mpsc::sync_channel(16); // TODO: No magic number
        let handle = thread::spawn(move || flush_thread(file, fd, rx));

        Ok(Flusher { tx, handle, fpos })
    }

    fn close_wait(self) {
        mem::drop(self.tx);
        self.handle.join().unwrap();
    }

    pub(crate) fn send(&mut self, block: Vec<u8>) {
        self.tx.send(block).unwrap();
    }
}

fn flush_thread(file: String, mut fd: fs::File, rx: mpsc::Receiver<Vec<u8>>) {
    for data in rx.iter() {
        if !flush_write_data(&file, &mut fd, &data) {
            break;
        }
    }
    // file descriptor and receiver channel shall be dropped.
}

fn flush_write_data(file: &str, fd: &mut fs::File, data: &[u8]) -> bool {
    match fd.write(data) {
        Err(err) => {
            panic!("flusher: {:?} error {}...", file, err);
        }
        Ok(n) if n != data.len() => {
            panic!("flusher: {:?} partial write {}/{}...", file, n, data.len());
        }
        Ok(_) => true,
    }
}
