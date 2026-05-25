//! Ring buffer: mmap-backed pcap file with atomic write_head + wrap policy.
//!
//! File layout: `[pcap hdr (24)] [data region] [trailer (64)]`. The data
//! region is `[data_region_start .. data_region_end)`. `write_head` is the
//! absolute file offset for the next record. On wrap (a record would cross
//! `data_region_end`), the writer pads the tail with `0xFF`, resets head
//! to `data_region_start`, bumps generation, and writes at the start.
//!
//! Records never split across the wrap boundary.
//!
//! Multi-producer safety: writes serialize through `Mutex<MmapMut>`; the
//! atomic `write_head` + `generation` are mirrored from the locked-state
//! source of truth so lock-free readers (the dump iterator) can snapshot
//! progress without blocking writes.

use super::pcap::{
    write_file_header, DEFAULT_SNAPLEN, PCAP_FILE_HEADER_LEN, PCAP_MAGIC,
};
use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const TRAILER_LEN: usize = 64;
pub const TRAILER_MAGIC: &[u8; 8] = b"ENRPCAPR";
pub const TRAILER_FORMAT_VERSION: u32 = 1;
pub const PAD_BYTE: u8 = 0xFF;

const MIN_RING_SIZE: u64 = (PCAP_FILE_HEADER_LEN + TRAILER_LEN + 1024) as u64;

pub struct RingBuffer {
    mmap: Mutex<MmapMut>,
    size: u64,
    pub(crate) data_region_start: u64,
    pub(crate) data_region_end: u64,
    pub(crate) write_head: AtomicU64,
    pub(crate) generation: AtomicU64,
}

#[derive(Debug, thiserror::Error)]
pub enum RingError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("ring size {0} too small; minimum {1}")]
    TooSmall(u64, u64),
    #[error("record size {0} exceeds data region size {1}")]
    RecordTooLarge(usize, u64),
}

impl RingBuffer {
    /// Open or create the ring file at `path` with the given total size in
    /// bytes. If the file exists at a different size it is truncated and
    /// re-initialized (capture data from prior runs is lost on size change).
    pub fn open_or_create(path: &Path, size: u64) -> Result<Self, RingError> {
        if size < MIN_RING_SIZE {
            return Err(RingError::TooSmall(size, MIN_RING_SIZE));
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let current_len = file.metadata()?.len();
        let mut needs_init = current_len != size;
        if needs_init {
            file.set_len(size)?;
        }

        // Best-effort pre-allocation. Some filesystems (tmpfs, FUSE mounts)
        // don't support fallocate; we already sized via set_len so this is
        // a hint, not a requirement.
        let _ = nix::fcntl::fallocate(
            file.as_raw_fd(),
            nix::fcntl::FallocateFlags::empty(),
            0,
            size as i64,
        );

        // SAFETY: mmap of a regular file with read+write access. The file
        // outlives the mmap because we hold the File until after mmap is
        // created; memmap2 takes its own dup of the fd internally on Linux.
        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        let data_region_start = PCAP_FILE_HEADER_LEN as u64;
        let data_region_end = size - TRAILER_LEN as u64;

        // If the pcap magic at offset 0 doesn't match, treat as fresh and
        // initialize everything. (set_len doesn't zero the existing file,
        // but a size mismatch already triggered set_len so the existing
        // content is whatever was there; we'll overwrite the header and
        // trailer regions either way.)
        if needs_init || mmap[0..4] != PCAP_MAGIC.to_le_bytes() {
            needs_init = true;
            write_file_header(&mut mmap[0..PCAP_FILE_HEADER_LEN], DEFAULT_SNAPLEN);
        }

        let trailer_offset = (size - TRAILER_LEN as u64) as usize;
        let trailer_magic_matches =
            &mmap[trailer_offset..trailer_offset + 8] == TRAILER_MAGIC;

        let (write_head_init, gen_init) = if needs_init || !trailer_magic_matches {
            init_trailer(&mut mmap[trailer_offset..trailer_offset + TRAILER_LEN], data_region_start);
            (data_region_start, 0u64)
        } else {
            let head = u64::from_le_bytes(
                mmap[trailer_offset + 16..trailer_offset + 24]
                    .try_into()
                    .unwrap(),
            );
            let gen = u64::from_le_bytes(
                mmap[trailer_offset + 24..trailer_offset + 32]
                    .try_into()
                    .unwrap(),
            );
            // Sanity-clamp recovered head to data region
            let head = head.clamp(data_region_start, data_region_end);
            (head, gen)
        };

        Ok(Self {
            mmap: Mutex::new(mmap),
            size,
            data_region_start,
            data_region_end,
            write_head: AtomicU64::new(write_head_init),
            generation: AtomicU64::new(gen_init),
        })
    }

    /// Append `record` bytes to the ring. Wraps with 0xFF padding if the
    /// record would cross `data_region_end`. Updates trailer write_head +
    /// generation after the write.
    pub fn append(&self, record: &[u8]) -> Result<(), RingError> {
        let record_len = record.len() as u64;
        let data_region_size = self.data_region_end - self.data_region_start;
        if record_len > data_region_size {
            return Err(RingError::RecordTooLarge(record.len(), data_region_size));
        }

        let mut guard = self.mmap.lock().unwrap();
        let mut head = self.write_head.load(Ordering::Acquire);
        let mut gen = self.generation.load(Ordering::Acquire);

        // Wrap if record won't fit at current head
        if head + record_len > self.data_region_end {
            // Pad tail with 0xFF
            let pad_start = head as usize;
            let pad_end = self.data_region_end as usize;
            for b in &mut guard[pad_start..pad_end] {
                *b = PAD_BYTE;
            }
            head = self.data_region_start;
            gen = gen.wrapping_add(1);
        }

        let write_start = head as usize;
        let write_end = write_start + record.len();
        guard[write_start..write_end].copy_from_slice(record);

        let new_head = head + record_len;

        // Update trailer write_head + generation (file-visible state)
        let trailer_off = (self.size - TRAILER_LEN as u64) as usize;
        guard[trailer_off + 16..trailer_off + 24].copy_from_slice(&new_head.to_le_bytes());
        guard[trailer_off + 24..trailer_off + 32].copy_from_slice(&gen.to_le_bytes());

        // Publish atomics for lock-free readers. Generation first so a
        // reader seeing the new head also sees the right generation.
        self.generation.store(gen, Ordering::Release);
        self.write_head.store(new_head, Ordering::Release);

        Ok(())
    }

    /// Reset write_head to data_region_start and increment generation.
    /// Used by `POST /debug/p2p-capture/reset` for clean repro captures.
    ///
    /// The data region is filled with `0xFF` padding so the dump iterator
    /// returns zero records until new captures land. Without this, the
    /// pre-reset bytes would still parse as valid records (walker treats
    /// gen>0 as "whole region has content") and operators would see
    /// stale frames in their post-reset dump.
    ///
    /// Cost: one O(data_region_size) memset, held under the write mutex.
    /// For a 1 GB ring this blocks captures for ~50-100ms — acceptable
    /// for an explicit operator action.
    pub fn reset(&self) {
        let mut guard = self.mmap.lock().unwrap();
        let new_gen = self.generation.load(Ordering::Acquire).wrapping_add(1);
        let new_head = self.data_region_start;

        let drs = self.data_region_start as usize;
        let dre = self.data_region_end as usize;
        for b in &mut guard[drs..dre] {
            *b = PAD_BYTE;
        }

        let trailer_off = (self.size - TRAILER_LEN as u64) as usize;
        guard[trailer_off + 16..trailer_off + 24].copy_from_slice(&new_head.to_le_bytes());
        guard[trailer_off + 24..trailer_off + 32].copy_from_slice(&new_gen.to_le_bytes());

        self.generation.store(new_gen, Ordering::Release);
        self.write_head.store(new_head, Ordering::Release);
    }

    /// Total file size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Lock-free snapshot of write_head and generation. Used by the dump
    /// iterator to anchor its walk.
    pub fn position(&self) -> (u64, u64) {
        // Load generation first, then head; readers MUST tolerate a head
        // moving under them either way (records eventually look like
        // padding to the parser, dump skips them).
        let gen = self.generation.load(Ordering::Acquire);
        let head = self.write_head.load(Ordering::Acquire);
        (head, gen)
    }

    /// Borrow the underlying bytes. Holds the write lock for the lifetime
    /// of the returned guard — use only for test inspection or short reads.
    pub fn snapshot(&self) -> impl std::ops::Deref<Target = MmapMut> + '_ {
        self.mmap.lock().unwrap()
    }
}

fn init_trailer(trailer: &mut [u8], data_region_start: u64) {
    assert_eq!(trailer.len(), TRAILER_LEN);
    for b in trailer.iter_mut() {
        *b = 0;
    }
    trailer[0..8].copy_from_slice(TRAILER_MAGIC);
    trailer[8..12].copy_from_slice(&TRAILER_FORMAT_VERSION.to_le_bytes());
    // 12..16 reserved (already zeroed)
    trailer[16..24].copy_from_slice(&data_region_start.to_le_bytes());
    trailer[24..32].copy_from_slice(&0u64.to_le_bytes());
    // 32..64 reserved
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn new_ring(size: u64) -> (tempfile::TempDir, RingBuffer) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cap.ring");
        let ring = RingBuffer::open_or_create(&path, size).unwrap();
        (dir, ring)
    }

    #[test]
    fn create_initializes_pcap_header_and_trailer() {
        let (_d, ring) = new_ring(1024 * 1024);
        let bytes = ring.snapshot();
        // pcap magic at offset 0
        assert_eq!(&bytes[0..4], &PCAP_MAGIC.to_le_bytes());
        // trailer magic
        let tend = bytes.len();
        assert_eq!(
            &bytes[tend - TRAILER_LEN..tend - TRAILER_LEN + 8],
            TRAILER_MAGIC
        );
        // initial write_head = data_region_start
        assert_eq!(ring.write_head.load(Ordering::Acquire), ring.data_region_start);
        assert_eq!(ring.generation.load(Ordering::Acquire), 0);
    }

    #[test]
    fn append_writes_at_current_head_and_advances() {
        let (_d, ring) = new_ring(1024 * 1024);
        let head_before = ring.write_head.load(Ordering::Acquire);
        ring.append(b"hello world").unwrap();
        let head_after = ring.write_head.load(Ordering::Acquire);
        assert_eq!(head_after - head_before, 11);
        // Bytes appear in the snapshot at head_before
        let bytes = ring.snapshot();
        assert_eq!(&bytes[head_before as usize..head_after as usize], b"hello world");
    }

    #[test]
    fn trailer_reflects_position_after_append() {
        let (_d, ring) = new_ring(1024 * 1024);
        ring.append(b"abc").unwrap();
        let bytes = ring.snapshot();
        let tend = bytes.len();
        let head_in_trailer = u64::from_le_bytes(
            bytes[tend - TRAILER_LEN + 16..tend - TRAILER_LEN + 24]
                .try_into()
                .unwrap(),
        );
        assert_eq!(head_in_trailer, ring.data_region_start + 3);
    }

    #[test]
    fn wrap_at_boundary_pads_and_restarts_at_data_region_start() {
        let size = 4096;
        let (_d, ring) = new_ring(size);
        let chunk = vec![0xAA; 1000];
        while ring.write_head.load(Ordering::Acquire) + chunk.len() as u64
            <= ring.data_region_end
        {
            ring.append(&chunk).unwrap();
        }
        let pre_wrap_head = ring.write_head.load(Ordering::Acquire);
        let pre_gen = ring.generation.load(Ordering::Acquire);
        let big = vec![0xBB; 500];
        ring.append(&big).unwrap();
        let post_wrap_head = ring.write_head.load(Ordering::Acquire);
        let post_gen = ring.generation.load(Ordering::Acquire);
        assert!(post_wrap_head < pre_wrap_head);
        assert_eq!(post_wrap_head, ring.data_region_start + 500);
        assert_eq!(post_gen, pre_gen + 1);
        // Padding 0xFF from pre_wrap_head to data_region_end
        let bytes = ring.snapshot();
        for &b in &bytes[pre_wrap_head as usize..ring.data_region_end as usize] {
            assert_eq!(b, PAD_BYTE);
        }
        // Big record at the start of data region
        assert_eq!(
            &bytes[ring.data_region_start as usize
                ..(ring.data_region_start + 500) as usize],
            &big[..]
        );
    }

    #[test]
    fn reset_clears_head_and_bumps_generation() {
        let (_d, ring) = new_ring(1024 * 1024);
        ring.append(b"data").unwrap();
        let gen_before = ring.generation.load(Ordering::Acquire);
        ring.reset();
        assert_eq!(
            ring.write_head.load(Ordering::Acquire),
            ring.data_region_start
        );
        assert_eq!(ring.generation.load(Ordering::Acquire), gen_before + 1);
    }

    #[test]
    fn record_larger_than_data_region_errors() {
        let (_d, ring) = new_ring(MIN_RING_SIZE);
        let data_region_size = ring.data_region_end - ring.data_region_start;
        let huge = vec![0u8; (data_region_size + 1) as usize];
        assert!(matches!(
            ring.append(&huge),
            Err(RingError::RecordTooLarge(_, _))
        ));
    }

    #[test]
    fn too_small_ring_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tiny.ring");
        let r = RingBuffer::open_or_create(&path, 64);
        assert!(matches!(r, Err(RingError::TooSmall(_, _))));
    }

    #[test]
    fn reopen_preserves_write_head_and_generation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cap.ring");
        let size = 1024 * 1024;
        {
            let ring = RingBuffer::open_or_create(&path, size).unwrap();
            ring.append(b"first record").unwrap();
            ring.append(b"second").unwrap();
        }
        let ring2 = RingBuffer::open_or_create(&path, size).unwrap();
        let expected_head = ring2.data_region_start + (b"first record".len() + b"second".len()) as u64;
        assert_eq!(ring2.write_head.load(Ordering::Acquire), expected_head);
        assert_eq!(ring2.generation.load(Ordering::Acquire), 0);
    }

    #[test]
    fn reopen_at_different_size_reinitializes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cap.ring");
        {
            let ring = RingBuffer::open_or_create(&path, 1024 * 1024).unwrap();
            ring.append(b"will be lost").unwrap();
        }
        let ring2 = RingBuffer::open_or_create(&path, 2 * 1024 * 1024).unwrap();
        // Re-initialized: head at data_region_start, gen 0
        assert_eq!(ring2.write_head.load(Ordering::Acquire), ring2.data_region_start);
        assert_eq!(ring2.generation.load(Ordering::Acquire), 0);
    }
}
