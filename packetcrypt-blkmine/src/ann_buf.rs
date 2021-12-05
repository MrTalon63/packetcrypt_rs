use crate::types::{AnnData,Hash};
use crate::databuf::DataBuf;
use rayon::prelude::*;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The purpose of AnnBuf is to be able to store and account for announcements in memory
/// and efficiently generate sorted lists on demand.
/// Every AnnBuf has a base address (in the big memory storage area).
pub struct AnnBuf<const ANNBUF_SZ: usize, const RANGES: usize> {
    db: Arc<DataBuf>,
    pub base_offset: usize,

    /// The index of the next push.
    /// Allows atomic adds to allocate space for additional anns.
    next_ann_index: AtomicUsize,
    /// The calculated hashes.
    /// Gives interior mutability, so this struct can be shared among threads.
    ann_data: UnsafeCell<[AnnData; ANNBUF_SZ]>,

    /// first range is assumed 0-ranges[0]
    /// second range is ranges[0]-ranges[1]
    /// last range is ranges[n-1]-ranges[n]
    /// ranges[n] is the length of this AnnBuf at time of lock
    ranges: [usize; RANGES],

    locked: bool,
}

unsafe impl<const ANNBUF_SZ: usize, const RANGES: usize> Send for AnnBuf<ANNBUF_SZ, RANGES> {}
unsafe impl<const ANNBUF_SZ: usize, const RANGES: usize> Sync for AnnBuf<ANNBUF_SZ, RANGES> {}

impl<const ANNBUF_SZ: usize, const RANGES: usize> AnnBuf<ANNBUF_SZ, RANGES> {
    pub fn new(db: Arc<DataBuf>, base_offset: usize) -> Self {
        Self {
            db,
            base_offset,
            next_ann_index: AtomicUsize::new(0),
            ann_data: [AnnData::default(); ANNBUF_SZ].into(),
            ranges: [0; RANGES],
            locked: false.into(),
        }
    }

    /// Push a slice of announcements into this buffer.
    /// Returns the number of actually inserted anns.
    pub fn push_anns(&self, anns: &[&[u8]], mut indexes: &[u32], hashes: &Vec<Hash>) -> usize {
        assert!(!self.locked);

        // atomically advance the next_ann_index to "claim" the space.
        let ann_index = self
            .next_ann_index
            .fetch_add(indexes.len(), Ordering::Relaxed);
        if ann_index >= ANNBUF_SZ {
            self.next_ann_index.store(ANNBUF_SZ, Ordering::Relaxed);
            return 0;
        }

        // verify if a partial push is necessary.
        if ann_index + indexes.len() > ANNBUF_SZ {
            indexes = &indexes[..ANNBUF_SZ - ann_index];
            self.next_ann_index.store(ANNBUF_SZ, Ordering::Relaxed);
        }

        let ann_data = self.ann_data.get();
        for (i, (ann, idx)) in (ann_index..).zip(indexes.iter().map(|&ci| (anns[ci as usize], ci))) {
            unsafe {
                // SAFETY: the starting index comes from an atomic, and we won't write out of indexes.len() range.
                (*ann_data)[i] = AnnData{
                    hash_pfx: hashes[idx as usize].to_u64(),
                    mloc: self.base_offset + i,
                };
            }

            // actually store ann in miner, with the index offset.
            self.db.put_ann(self.base_offset + i, ann, &hashes[idx as usize]);
        }

        indexes.len()
    }

    /// Locks this AnnBuf once it is full, which sorts the index table by ann hash.
    /// Working with pre-sorted anns is better because they need to be sorted later, and
    /// sorting a bunch of concatenated sorted lists is fast.
    pub fn lock(&mut self) {
        assert!(!self.locked);

        let last = self.next_ann_index();
        let ann_data = unsafe { &mut *self.ann_data.get() };
        ann_data[..last].par_sort_unstable_by_key(|d| d.hash_pfx);

        let mut pfx = ann_data[0].hash_pfx % RANGES as u64;
        let mut r = 0;
        for (i, ad) in ann_data[..last].iter().enumerate() {
            let this_pfx = ad.hash_pfx % RANGES as u64;
            if this_pfx != pfx {
                self.ranges[r] = i;
                pfx = this_pfx;
                r += 1;
            }
        }
        self.ranges[r] = last;
        self.locked = true
    }

    /// Clear the buf for another usage.
    pub fn reset(&mut self) {
        self.next_ann_index.store(0, Ordering::Relaxed);
        self.locked = false;
    }

    fn range(&self, range: usize) -> (usize, usize) {
        if range == 0 {
            (0, self.ranges[0])
        } else {
            (self.ranges[range-1], self.ranges[range])
        }
    }

    pub fn range_count(&self, range: usize) -> usize {
        let (begin, end) = self.range(range);
        end - begin
    }

    pub fn iter<'a>(&'a self, range: usize) -> impl Iterator<Item = &AnnData> + 'a {
        assert!(self.locked);
        let (begin, end) = self.range(range);
        let ptr = unsafe { &*self.ann_data.get() };
        (begin..end).map(move |i| &ptr[i])
    }

    /// Read out the data from the buf into an array of prooftree::AnnData, which will be used
    /// for building the final proof tree.
    pub fn read_ready_anns(&self, out: &mut [AnnData]) {
        assert!(self.locked);
        let last = self.next_ann_index();
        let ann_data = unsafe { &*self.ann_data.get() };
        for (i, ad) in ann_data[0..last].iter().enumerate() {
            out[i] = *ad;
        }
    }

    pub fn next_ann_index(&self) -> usize {
        self.next_ann_index.load(Ordering::Relaxed)
    }
}
