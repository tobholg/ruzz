//! On-disk document store: full documents behind the compact search rows.
//!
//! Documents are keyed by `_ref` — the sequential u64 ordinal assigned during
//! import (0..N-1 across all sources). Because refs are dense and the store is
//! written in ref order, lookup needs no hash map or tree: a small in-memory
//! block table plus binary search on `first_ref`.
//!
//! Layout (`store/` directory, sibling of the index directory):
//!   docs.dat    magic header, then zstd-compressed blocks appended in ref order
//!   blocks.idx  fixed-width block table: offset, lengths, first_ref, doc count
//!   meta.json   doc count, generation, compression settings — written LAST,
//!               acts as the commit marker
//!
//! Raw block layout (before compression):
//!   [u32 doc_len × n_docs][doc bytes concatenated]

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{bail, Context};

pub const DOCS_FILE: &str = "docs.dat";
pub const BLOCKS_FILE: &str = "blocks.idx";
pub const META_FILE: &str = "meta.json";
/// Generation marker written into the *index* directory so a store can be
/// verified to belong to the index it was imported with.
pub const INDEX_PAIRING_FILE: &str = "ruzz_store.json";

const DOCS_MAGIC: &[u8; 8] = b"RZDOCS01";
const BLOCKS_MAGIC: &[u8; 8] = b"RZBLKS01";
const FORMAT_VERSION: u32 = 1;
/// Bounded channel between the import thread and the store writer thread.
const CHANNEL_DEPTH: usize = 4096;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoreMeta {
    pub format_version: u32,
    pub generation: String,
    pub doc_count: u64,
    pub block_count: u64,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
    pub compression: String,
    pub compression_level: i32,
    pub source_mode: String,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexPairing {
    pub generation: String,
    /// Total refs ever issued against this store. After incremental updates
    /// the index holds fewer live docs than the store holds versions, so
    /// this — not the index doc count — is what the store must match.
    /// Absent on indexes written before incremental updates existed; those
    /// still require store doc count == index doc count.
    #[serde(default)]
    pub doc_count: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct BlockEntry {
    offset: u64,
    comp_len: u32,
    raw_len: u32,
    first_ref: u64,
    n_docs: u32,
}

const BLOCK_ENTRY_SIZE: usize = 28;

/// Parse an absolute size string like "256KB", "64MB", "1GB". No percentages.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    let (num, mult) = if let Some(n) = s.strip_suffix("gb") {
        (n, 1024u64 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("mb") {
        (n, 1024u64 * 1024)
    } else if let Some(n) = s.strip_suffix("kb") {
        (n, 1024u64)
    } else {
        (s.as_str(), 1u64)
    };
    let num: f64 = num.trim().parse().ok()?;
    if num <= 0.0 {
        return None;
    }
    Some((num * mult as f64) as u64)
}

pub fn new_generation() -> String {
    let nanos = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    format!("{:x}-{:x}", nanos, std::process::id())
}

/// True if the directory is empty or contains ruzz store files (including
/// leftovers from a crashed import). Used as a safety check before wiping.
pub fn looks_like_store_dir(dir: &Path) -> bool {
    let Ok(mut entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let first = entries.next();
    if first.is_none() {
        return true; // empty
    }
    for name in [META_FILE, DOCS_FILE, BLOCKS_FILE] {
        if dir.join(name).exists() {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

pub struct WriterOutcome {
    pub doc_count: u64,
    pub block_count: u64,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
}

/// Where the writer thread starts: fresh for a full import, mid-file for an
/// incremental append. All counters are store totals, so the outcome always
/// describes the whole store.
struct WriterInit {
    blocks: Vec<BlockEntry>,
    offset: u64,
    next_ref: u64,
    raw_bytes: u64,
    compressed_bytes: u64,
}

/// Accepts full-document bytes in ref order and writes docs.dat + blocks.idx
/// on a background thread. `meta.json` is NOT written here — the caller writes
/// it after the search index has committed, so a missing meta marks the store
/// invalid on any failure path.
pub struct StoreWriter {
    tx: Option<SyncSender<Vec<u8>>>,
    handle: Option<JoinHandle<anyhow::Result<WriterOutcome>>>,
    first_ref: u64,
}

impl StoreWriter {
    pub fn create(dir: &Path, block_size: u64, level: i32) -> anyhow::Result<Self> {
        let docs_path = dir.join(DOCS_FILE);
        let mut docs_file = File::create(&docs_path)
            .with_context(|| format!("failed creating {}", docs_path.display()))?;
        docs_file.write_all(DOCS_MAGIC)?;

        let init = WriterInit {
            blocks: Vec::new(),
            offset: DOCS_MAGIC.len() as u64,
            next_ref: 0,
            raw_bytes: 0,
            compressed_bytes: 0,
        };
        Self::spawn(dir, docs_file, init, block_size, level)
    }

    /// Open an existing, committed store to append documents after its
    /// current tail. New docs continue the ref sequence at `next_ref()`.
    /// Nothing already in the store is touched, and until the caller writes
    /// an updated meta.json the appended tail is invisible to readers — so a
    /// failed append leaves the store exactly as it was.
    pub fn append_to(dir: &Path, block_size: u64, level: i32) -> anyhow::Result<Self> {
        let meta = read_meta(dir)?;
        let blocks = read_block_table(dir, &meta)?;

        let docs_path = dir.join(DOCS_FILE);
        let mut docs_file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&docs_path)
            .with_context(|| format!("failed opening {} for append", docs_path.display()))?;
        let mut magic = [0u8; 8];
        docs_file
            .read_exact(&mut magic)
            .context("docs.dat truncated")?;
        if &magic != DOCS_MAGIC {
            bail!("docs.dat has wrong magic");
        }
        // Appends land at the physical end of the file, which can sit past
        // the last committed block if a previous append crashed before its
        // meta.json. Offsets in the block table are explicit, so the dead
        // gap is merely wasted bytes, not corruption.
        let end = docs_file.metadata()?.len();
        if let Some(last) = blocks.last() {
            if end < last.offset + last.comp_len as u64 {
                bail!("docs.dat is shorter than its block table describes");
            }
        }

        let init = WriterInit {
            blocks,
            offset: end,
            next_ref: meta.doc_count,
            raw_bytes: meta.raw_bytes,
            compressed_bytes: meta.compressed_bytes,
        };
        Self::spawn(dir, docs_file, init, block_size, level)
    }

    fn spawn(
        dir: &Path,
        docs_file: File,
        init: WriterInit,
        block_size: u64,
        level: i32,
    ) -> anyhow::Result<Self> {
        let blocks_path = dir.join(BLOCKS_FILE);
        let first_ref = init.next_ref;
        let (tx, rx) = sync_channel::<Vec<u8>>(CHANNEL_DEPTH);
        let handle = std::thread::Builder::new()
            .name("ruzz-store-writer".to_string())
            .spawn(move || writer_thread(docs_file, blocks_path, rx, block_size, level, init))?;

        Ok(Self {
            tx: Some(tx),
            handle: Some(handle),
            first_ref,
        })
    }

    /// The ref the next `add` will be stored under.
    pub fn next_ref_start(&self) -> u64 {
        self.first_ref
    }

    pub fn add(&self, doc: Vec<u8>) -> anyhow::Result<()> {
        let Some(tx) = self.tx.as_ref() else {
            bail!("store writer already finished");
        };
        if tx.send(doc).is_err() {
            bail!("store writer thread terminated early");
        }
        Ok(())
    }

    /// Close the channel, drain remaining docs, flush + fsync files.
    pub fn finish(mut self) -> anyhow::Result<WriterOutcome> {
        drop(self.tx.take());
        let handle = self.handle.take().expect("finish called twice");
        match handle.join() {
            Ok(result) => result,
            Err(_) => bail!("store writer thread panicked"),
        }
    }
}

/// A writer abandoned mid-import (the import failed before `finish`) must
/// not leave its thread running: closing the channel wakes it, and it then
/// flushes and CREATES files in the staging directory — racing the next
/// import's cleanup of that same path (observed as a flaky ENOTEMPTY from
/// `remove_dir_all`). Joining bounds the wait to draining an already-closed
/// channel and one flush.
impl Drop for StoreWriter {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn writer_thread(
    docs_file: File,
    blocks_path: PathBuf,
    rx: Receiver<Vec<u8>>,
    block_size: u64,
    level: i32,
    init: WriterInit,
) -> anyhow::Result<WriterOutcome> {
    let mut docs = BufWriter::with_capacity(1024 * 1024, docs_file);
    let mut offset = init.offset;

    let mut blocks: Vec<BlockEntry> = init.blocks;
    let mut buf: Vec<u8> = Vec::with_capacity(block_size as usize + 64 * 1024);
    let mut lens: Vec<u32> = Vec::new();
    let mut first_ref = init.next_ref;
    let mut next_ref = init.next_ref;
    let mut raw_bytes = init.raw_bytes;
    let mut compressed_bytes = init.compressed_bytes;

    let flush_block = |buf: &mut Vec<u8>,
                       lens: &mut Vec<u32>,
                       first_ref: u64,
                       offset: &mut u64,
                       blocks: &mut Vec<BlockEntry>,
                       docs: &mut BufWriter<File>,
                       compressed_bytes: &mut u64|
     -> anyhow::Result<()> {
        if lens.is_empty() {
            return Ok(());
        }
        let mut raw = Vec::with_capacity(lens.len() * 4 + buf.len());
        for len in lens.iter() {
            raw.extend_from_slice(&len.to_le_bytes());
        }
        raw.extend_from_slice(buf);
        let compressed = zstd::bulk::compress(&raw, level).context("zstd compression failed")?;
        docs.write_all(&compressed)?;
        blocks.push(BlockEntry {
            offset: *offset,
            comp_len: compressed.len() as u32,
            raw_len: raw.len() as u32,
            first_ref,
            n_docs: lens.len() as u32,
        });
        *offset += compressed.len() as u64;
        *compressed_bytes += compressed.len() as u64;
        buf.clear();
        lens.clear();
        Ok(())
    };

    while let Ok(doc) = rx.recv() {
        if doc.len() > u32::MAX as usize {
            bail!("document {} exceeds 4GB", next_ref);
        }
        lens.push(doc.len() as u32);
        buf.extend_from_slice(&doc);
        raw_bytes += doc.len() as u64;
        next_ref += 1;
        if buf.len() as u64 >= block_size {
            flush_block(
                &mut buf,
                &mut lens,
                first_ref,
                &mut offset,
                &mut blocks,
                &mut docs,
                &mut compressed_bytes,
            )?;
            first_ref = next_ref;
        }
    }
    flush_block(
        &mut buf,
        &mut lens,
        first_ref,
        &mut offset,
        &mut blocks,
        &mut docs,
        &mut compressed_bytes,
    )?;

    let docs_file = docs.into_inner().context("flushing docs.dat")?;
    docs_file.sync_all()?;

    // The full block table (existing + appended), written to a temp file and
    // renamed: an append updates this file in a live store directory, and a
    // crash mid-write must not clobber the committed table.
    let mut raw = Vec::with_capacity(20 + blocks.len() * BLOCK_ENTRY_SIZE);
    raw.extend_from_slice(BLOCKS_MAGIC);
    raw.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    raw.extend_from_slice(&(blocks.len() as u64).to_le_bytes());
    for b in &blocks {
        raw.extend_from_slice(&b.offset.to_le_bytes());
        raw.extend_from_slice(&b.comp_len.to_le_bytes());
        raw.extend_from_slice(&b.raw_len.to_le_bytes());
        raw.extend_from_slice(&b.first_ref.to_le_bytes());
        raw.extend_from_slice(&b.n_docs.to_le_bytes());
    }
    write_atomically(&blocks_path, &raw)?;

    Ok(WriterOutcome {
        doc_count: next_ref,
        block_count: blocks.len() as u64,
        raw_bytes,
        compressed_bytes,
    })
}

/// Write via a temp file + rename, fsynced, so a reader (or a crash) never
/// sees a half-written file at `path`.
fn write_atomically(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file =
        File::create(&tmp).with_context(|| format!("failed creating {}", tmp.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed moving {} into place", path.display()))?;
    Ok(())
}

pub fn write_meta(dir: &Path, meta: &StoreMeta) -> anyhow::Result<()> {
    write_atomically(
        &dir.join(META_FILE),
        serde_json::to_string_pretty(meta)?.as_bytes(),
    )
}

pub fn read_meta(dir: &Path) -> anyhow::Result<StoreMeta> {
    let meta_raw = std::fs::read_to_string(dir.join(META_FILE))
        .with_context(|| format!("store meta missing at {}", dir.join(META_FILE).display()))?;
    let meta: StoreMeta = serde_json::from_str(&meta_raw).context("invalid store meta.json")?;
    if meta.format_version != FORMAT_VERSION {
        bail!(
            "unsupported store format version {} (expected {})",
            meta.format_version,
            FORMAT_VERSION
        );
    }
    Ok(meta)
}

/// Load the block table, trusting meta.json over blocks.idx: an append that
/// crashed after rewriting the table but before committing meta leaves extra
/// trailing entries, which are simply not part of the store yet.
fn read_block_table(dir: &Path, meta: &StoreMeta) -> anyhow::Result<Vec<BlockEntry>> {
    let blocks_path = dir.join(BLOCKS_FILE);
    let mut idx = File::open(&blocks_path)
        .with_context(|| format!("failed opening {}", blocks_path.display()))?;
    let mut header = [0u8; 20];
    idx.read_exact(&mut header)
        .context("blocks.idx truncated")?;
    if &header[0..8] != BLOCKS_MAGIC {
        bail!("blocks.idx has wrong magic");
    }
    let n_blocks = u64::from_le_bytes(header[12..20].try_into().unwrap());
    if n_blocks < meta.block_count {
        bail!(
            "blocks.idx has {} blocks, meta.json says {}",
            n_blocks,
            meta.block_count
        );
    }
    let mut raw = vec![0u8; meta.block_count as usize * BLOCK_ENTRY_SIZE];
    idx.read_exact(&mut raw).context("blocks.idx truncated")?;

    let mut blocks = Vec::with_capacity(meta.block_count as usize);
    for chunk in raw.chunks_exact(BLOCK_ENTRY_SIZE) {
        blocks.push(BlockEntry {
            offset: u64::from_le_bytes(chunk[0..8].try_into().unwrap()),
            comp_len: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
            raw_len: u32::from_le_bytes(chunk[12..16].try_into().unwrap()),
            first_ref: u64::from_le_bytes(chunk[16..24].try_into().unwrap()),
            n_docs: u32::from_le_bytes(chunk[24..28].try_into().unwrap()),
        });
    }
    Ok(blocks)
}

pub fn write_index_pairing(
    index_dir: &Path,
    generation: &str,
    doc_count: u64,
) -> anyhow::Result<()> {
    let pairing = IndexPairing {
        generation: generation.to_string(),
        doc_count: Some(doc_count),
    };
    write_atomically(
        &index_dir.join(INDEX_PAIRING_FILE),
        serde_json::to_string_pretty(&pairing)?.as_bytes(),
    )
}

pub fn read_index_pairing(index_dir: &Path) -> Option<IndexPairing> {
    let raw = std::fs::read_to_string(index_dir.join(INDEX_PAIRING_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

struct BlockCache {
    entries: HashMap<u32, (Arc<Vec<u8>>, u64)>,
    capacity_bytes: u64,
    used_bytes: u64,
    tick: u64,
}

impl BlockCache {
    fn new(capacity_bytes: u64) -> Self {
        Self {
            entries: HashMap::new(),
            capacity_bytes,
            used_bytes: 0,
            tick: 0,
        }
    }

    fn get(&mut self, block: u32) -> Option<Arc<Vec<u8>>> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(&block).map(|(data, used)| {
            *used = tick;
            Arc::clone(data)
        })
    }

    fn insert(&mut self, block: u32, data: Arc<Vec<u8>>) {
        if self.capacity_bytes == 0 || data.len() as u64 > self.capacity_bytes {
            return;
        }
        self.tick += 1;
        self.used_bytes += data.len() as u64;
        self.entries.insert(block, (data, self.tick));
        // Evict least-recently-used until under capacity. The scan is O(n) but
        // n is small (capacity / block size) and this only runs on cache miss.
        while self.used_bytes > self.capacity_bytes {
            let Some((&oldest, _)) = self.entries.iter().min_by_key(|(_, (_, used))| *used) else {
                break;
            };
            if let Some((data, _)) = self.entries.remove(&oldest) {
                self.used_bytes -= data.len() as u64;
            }
        }
    }
}

pub struct StoreStats {
    pub doc_count: u64,
    pub block_count: u64,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
    pub generation: String,
    pub source_mode: String,
    pub cache_capacity_bytes: u64,
    pub cache_used_bytes: u64,
    pub cache_entries: usize,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

/// The mutable view of the store: grows in place when an incremental update
/// appends documents, refreshed from disk when a ref past the known end shows
/// up (the tantivy reader picks up commits from another process on its own;
/// this is the store-side counterpart).
struct StoreState {
    blocks: Vec<BlockEntry>,
    meta: StoreMeta,
}

pub struct StoreReader {
    dir: PathBuf,
    file: File,
    state: std::sync::RwLock<StoreState>,
    cache: Mutex<BlockCache>,
    hits: AtomicU64,
    misses: AtomicU64,
    /// Throttles refresh attempts, so a burst of unknown refs cannot turn
    /// into a burst of meta.json reads.
    last_refresh: Mutex<std::time::Instant>,
}

/// Minimum time between two on-disk refresh checks.
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

impl StoreReader {
    pub fn open(dir: &Path, cache_bytes: u64) -> anyhow::Result<Self> {
        let meta = read_meta(dir)?;
        let blocks = read_block_table(dir, &meta)?;

        let docs_path = dir.join(DOCS_FILE);
        let mut file = File::open(&docs_path)
            .with_context(|| format!("failed opening {}", docs_path.display()))?;
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic).context("docs.dat truncated")?;
        if &magic != DOCS_MAGIC {
            bail!("docs.dat has wrong magic");
        }

        Ok(Self {
            dir: dir.to_path_buf(),
            file,
            state: std::sync::RwLock::new(StoreState { blocks, meta }),
            cache: Mutex::new(BlockCache::new(cache_bytes)),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            // Backdated so the first refresh attempt is never throttled.
            last_refresh: Mutex::new(
                std::time::Instant::now()
                    .checked_sub(REFRESH_INTERVAL)
                    .unwrap_or_else(std::time::Instant::now),
            ),
        })
    }

    pub fn doc_count(&self) -> u64 {
        self.state.read().unwrap().meta.doc_count
    }

    pub fn generation(&self) -> String {
        self.state.read().unwrap().meta.generation.clone()
    }

    /// Pick up documents an incremental update appended since this reader
    /// (last) loaded the store. Only ever moves forward within the same
    /// generation: a full re-import swaps directories and pairs with a new
    /// index, so this open handle keeps serving the store it was opened on.
    fn refresh(&self) -> anyhow::Result<()> {
        {
            let mut last = self.last_refresh.lock().unwrap();
            if last.elapsed() < REFRESH_INTERVAL {
                return Ok(());
            }
            *last = std::time::Instant::now();
        }
        let current_doc_count;
        let current_generation;
        {
            let state = self.state.read().unwrap();
            current_doc_count = state.meta.doc_count;
            current_generation = state.meta.generation.clone();
        }
        let Ok(meta) = read_meta(&self.dir) else {
            return Ok(()); // store dir mid-swap or gone; keep the loaded view
        };
        if meta.generation != current_generation || meta.doc_count <= current_doc_count {
            return Ok(());
        }
        let blocks = read_block_table(&self.dir, &meta)?;
        let mut state = self.state.write().unwrap();
        // Another thread may have refreshed while this one read the files.
        if meta.doc_count > state.meta.doc_count {
            *state = StoreState { blocks, meta };
        }
        Ok(())
    }

    /// Fetch the raw bytes of one document. Returns None for refs out of range.
    pub fn get(&self, doc_ref: u64) -> anyhow::Result<Option<Vec<u8>>> {
        if doc_ref >= self.state.read().unwrap().meta.doc_count {
            // The index may have committed an update whose store tail this
            // reader has not seen yet.
            self.refresh()?;
        }
        let state = self.state.read().unwrap();
        if doc_ref >= state.meta.doc_count {
            return Ok(None);
        }
        // Last block whose first_ref <= doc_ref
        let idx = state
            .blocks
            .partition_point(|b| b.first_ref <= doc_ref)
            .checked_sub(1)
            .context("block table empty for non-empty store")?;
        let entry = state.blocks[idx];
        drop(state);
        if doc_ref >= entry.first_ref + entry.n_docs as u64 {
            bail!("ref {} falls in a gap in the block table", doc_ref);
        }

        let raw = self.load_block(idx as u32, &entry)?;

        let n_docs = entry.n_docs as usize;
        let lens_end = n_docs * 4;
        if raw.len() < lens_end {
            bail!("block {} shorter than its length table", idx);
        }
        let pos = (doc_ref - entry.first_ref) as usize;
        let mut start = lens_end;
        for i in 0..pos {
            let len = u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
            start += len;
        }
        let len = u32::from_le_bytes(raw[pos * 4..pos * 4 + 4].try_into().unwrap()) as usize;
        if start + len > raw.len() {
            bail!("document {} extends past block {}", doc_ref, idx);
        }
        Ok(Some(raw[start..start + len].to_vec()))
    }

    fn load_block(&self, block_id: u32, entry: &BlockEntry) -> anyhow::Result<Arc<Vec<u8>>> {
        if let Some(data) = self.cache.lock().unwrap().get(block_id) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(data);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);

        let mut compressed = vec![0u8; entry.comp_len as usize];
        read_exact_at(&self.file, &mut compressed, entry.offset)
            .with_context(|| format!("reading block {} from docs.dat", block_id))?;
        let raw = zstd::bulk::decompress(&compressed, entry.raw_len as usize)
            .with_context(|| format!("decompressing block {}", block_id))?;
        if raw.len() != entry.raw_len as usize {
            bail!("block {} decompressed to unexpected size", block_id);
        }
        let raw = Arc::new(raw);
        self.cache
            .lock()
            .unwrap()
            .insert(block_id, Arc::clone(&raw));
        Ok(raw)
    }

    pub fn stats(&self) -> StoreStats {
        let state = self.state.read().unwrap();
        let cache = self.cache.lock().unwrap();
        StoreStats {
            doc_count: state.meta.doc_count,
            block_count: state.meta.block_count,
            raw_bytes: state.meta.raw_bytes,
            compressed_bytes: state.meta.compressed_bytes,
            generation: state.meta.generation.clone(),
            source_mode: state.meta.source_mode.clone(),
            cache_capacity_bytes: cache.capacity_bytes,
            cache_used_bytes: cache.used_bytes,
            cache_entries: cache.entries.len(),
            cache_hits: self.hits.load(Ordering::Relaxed),
            cache_misses: self.misses.load(Ordering::Relaxed),
        }
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short read",
            ));
        }
        done += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ruzz-store-{prefix}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_test_store(dir: &Path, docs: &[Vec<u8>], block_size: u64) -> WriterOutcome {
        let writer = StoreWriter::create(dir, block_size, 1).unwrap();
        for d in docs {
            writer.add(d.clone()).unwrap();
        }
        let outcome = writer.finish().unwrap();
        let meta = StoreMeta {
            format_version: FORMAT_VERSION,
            generation: new_generation(),
            doc_count: outcome.doc_count,
            block_count: outcome.block_count,
            raw_bytes: outcome.raw_bytes,
            compressed_bytes: outcome.compressed_bytes,
            compression: "zstd".to_string(),
            compression_level: 1,
            source_mode: "row".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        write_meta(dir, &meta).unwrap();
        outcome
    }

    #[test]
    fn roundtrips_documents_across_blocks() {
        let dir = temp_dir("roundtrip");
        // Small block size to force many blocks; varied doc sizes incl. empty
        let docs: Vec<Vec<u8>> = (0..500)
            .map(|i| {
                if i % 97 == 0 {
                    Vec::new()
                } else {
                    format!("{{\"id\":{i},\"payload\":\"{}\"}}", "x".repeat(i % 200)).into_bytes()
                }
            })
            .collect();
        let outcome = write_test_store(&dir, &docs, 2048);
        assert_eq!(outcome.doc_count, 500);
        assert!(outcome.block_count > 5, "expected multiple blocks");

        let reader = StoreReader::open(&dir, 1024 * 1024).unwrap();
        assert_eq!(reader.doc_count(), 500);
        for (i, expected) in docs.iter().enumerate() {
            let got = reader.get(i as u64).unwrap().unwrap();
            assert_eq!(&got, expected, "doc {i} mismatch");
        }
        assert_eq!(reader.get(500).unwrap(), None);
        assert_eq!(reader.get(u64::MAX).unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn single_doc_larger_than_block_size() {
        let dir = temp_dir("bigdoc");
        let docs = vec![vec![7u8; 100_000], b"small".to_vec()];
        write_test_store(&dir, &docs, 4096);
        let reader = StoreReader::open(&dir, 0).unwrap(); // cache disabled
        assert_eq!(reader.get(0).unwrap().unwrap().len(), 100_000);
        assert_eq!(reader.get(1).unwrap().unwrap(), b"small");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_store_roundtrips() {
        let dir = temp_dir("empty");
        write_test_store(&dir, &[], 4096);
        let reader = StoreReader::open(&dir, 1024).unwrap();
        assert_eq!(reader.doc_count(), 0);
        assert_eq!(reader.get(0).unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_hits_and_evicts() {
        let dir = temp_dir("cache");
        let docs: Vec<Vec<u8>> = (0..200).map(|i| vec![i as u8; 512]).collect();
        write_test_store(&dir, &docs, 4096);
        // Cache big enough for ~2 blocks
        let reader = StoreReader::open(&dir, 10_000).unwrap();
        reader.get(0).unwrap();
        reader.get(1).unwrap(); // same block → hit
        let stats = reader.stats();
        assert_eq!(stats.cache_hits, 1);
        assert_eq!(stats.cache_misses, 1);
        // Touch every block to force eviction churn, then verify reads still correct
        for i in 0..200 {
            assert_eq!(reader.get(i).unwrap().unwrap(), docs[i as usize]);
        }
        let stats = reader.stats();
        assert!(stats.cache_used_bytes <= 10_000);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The incremental path: an append extends a committed store in place,
    /// an uncommitted append (no meta yet) is invisible, and a reader that
    /// was already open picks the committed tail up on its own.
    #[test]
    fn append_extends_the_store_and_an_open_reader_picks_it_up() {
        let dir = temp_dir("append");
        let doc = |i: u64| format!("{{\"id\":{i}}}").into_bytes();
        let docs: Vec<Vec<u8>> = (0..100).map(doc).collect();
        write_test_store(&dir, &docs, 1024);
        let generation = read_meta(&dir).unwrap().generation;

        let reader = StoreReader::open(&dir, 1024 * 1024).unwrap();
        assert_eq!(reader.doc_count(), 100);
        assert_eq!(reader.get(100).unwrap(), None);

        let writer = StoreWriter::append_to(&dir, 1024, 1).unwrap();
        assert_eq!(writer.next_ref_start(), 100);
        for i in 100..150 {
            writer.add(doc(i)).unwrap();
        }
        let outcome = writer.finish().unwrap();
        assert_eq!(outcome.doc_count, 150);

        // blocks.idx already lists the appended blocks, but meta.json is the
        // commit marker: until it is rewritten, readers see the old store.
        let stale = StoreReader::open(&dir, 0).unwrap();
        assert_eq!(stale.doc_count(), 100);
        assert_eq!(stale.get(120).unwrap(), None);
        assert_eq!(stale.get(50).unwrap().unwrap(), doc(50));

        let mut meta = read_meta(&dir).unwrap();
        meta.doc_count = outcome.doc_count;
        meta.block_count = outcome.block_count;
        meta.raw_bytes = outcome.raw_bytes;
        meta.compressed_bytes = outcome.compressed_bytes;
        write_meta(&dir, &meta).unwrap();

        // The reader from before the append refreshes when asked past its
        // known end — same generation, so this is safe in place. (The get()
        // above consumed a refresh attempt; wait out the throttle.)
        std::thread::sleep(REFRESH_INTERVAL);
        assert_eq!(reader.get(149).unwrap().unwrap(), doc(149));
        assert_eq!(reader.doc_count(), 150);
        assert_eq!(reader.get(0).unwrap().unwrap(), doc(0));
        assert_eq!(reader.generation(), generation);

        // A fresh reader sees the whole store.
        let fresh = StoreReader::open(&dir, 0).unwrap();
        for i in 0..150 {
            assert_eq!(fresh.get(i).unwrap().unwrap(), doc(i), "doc {i}");
        }
        assert_eq!(fresh.get(150).unwrap(), None);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_meta_fails_open() {
        let dir = temp_dir("nometa");
        let writer = StoreWriter::create(&dir, 4096, 1).unwrap();
        writer.add(b"doc".to_vec()).unwrap();
        writer.finish().unwrap();
        // No meta.json written → open must fail (commit marker semantics)
        assert!(StoreReader::open(&dir, 1024).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("256KB"), Some(256 * 1024));
        assert_eq!(parse_size("64mb"), Some(64 * 1024 * 1024));
        assert_eq!(parse_size("1GB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("nope"), None);
    }

    #[test]
    fn store_dir_safety_check() {
        let dir = temp_dir("safety");
        assert!(looks_like_store_dir(&dir)); // empty is fine
        std::fs::write(dir.join("random.txt"), "hello").unwrap();
        assert!(!looks_like_store_dir(&dir)); // foreign content is not
        std::fs::write(dir.join(DOCS_FILE), "x").unwrap();
        assert!(looks_like_store_dir(&dir)); // crash leftovers are
        let _ = std::fs::remove_dir_all(dir);
    }
}
