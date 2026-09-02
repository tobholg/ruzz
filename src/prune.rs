//! Filtered top-k under block-WAND pruning.
//!
//! A fuzzy query with an exact filter used to be an intersection at the top
//! level — and an intersection is a shape tantivy cannot prune, so every
//! document sharing a driving trigram with the query was BM25-scored. The
//! wider the filter, the worse: `country_code=NO` on a single-country
//! dataset matches every document and buys nothing but the full scoring
//! pass.
//!
//! Here the bare fuzzy union stays the driving scorer, with pruning, and
//! the filters are evaluated per candidate against fast-field columns.
//! That is sound under WAND: pruning skips only documents that cannot beat
//! the current threshold, and a rejected document never raises it — it is
//! exactly how `TopDocs` treats deleted documents.
//!
//! Case-insensitive string fields are the one wrinkle: the fast column
//! keeps the raw value (so sorting keeps its casing) while the filter is
//! the lowercased term. Matching ordinals are found by one scan of the
//! column's dictionary, cached per (segment, field, value) — cheap for the
//! low-cardinality fields broad filters live on, and refused past a size
//! cap where the intersection path is the right tool anyway.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::{Arc, Mutex};

use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::{Column, StrColumn};
use tantivy::query::Weight;
use tantivy::{DocAddress, DocId, Score, SegmentId, SegmentOrdinal, SegmentReader};

/// Largest string dictionary a case-insensitive filter is resolved against
/// by scanning. Fields above this are high-cardinality identifiers, where a
/// filter is selective and the intersection path wins regardless.
pub const MAX_SCAN_TERMS: usize = 100_000;
/// Entries kept in the ordinal cache before it is reset wholesale.
const ORD_CACHE_CAP: usize = 8_192;

/// One exact filter, expressed against fast-field columns. Values are in
/// the form the index term takes — lowercased for case-insensitive fields.
#[derive(Debug, Clone, PartialEq)]
pub enum FastFilter {
    Str {
        field: String,
        values: Vec<String>,
        /// Compare column terms lowercased (case-insensitive field) rather
        /// than looking the value up verbatim.
        fold: bool,
    },
    Num {
        field: String,
        values: Vec<f64>,
    },
    Range {
        field: String,
        min: f64,
        max: f64,
    },
}

/// (segment, field, value) — what a resolved ordinal set is keyed by.
type OrdKey = (SegmentId, String, String);

/// Resolved term ordinals per (segment, field, value). Segments are
/// immutable, so an entry stays valid until the segment is merged away.
#[derive(Default)]
pub struct OrdCache {
    entries: Mutex<HashMap<OrdKey, Arc<Vec<u64>>>>,
}

impl OrdCache {
    fn ords_for(
        &self,
        segment: SegmentId,
        column: &StrColumn,
        field: &str,
        value: &str,
        fold: bool,
    ) -> tantivy::Result<Arc<Vec<u64>>> {
        let key = (segment, field.to_string(), value.to_string());
        if let Some(hit) = self.entries.lock().unwrap().get(&key) {
            return Ok(Arc::clone(hit));
        }
        let ords = Arc::new(resolve_ords(column, value, fold)?);
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= ORD_CACHE_CAP {
            entries.clear();
        }
        entries.insert(key, Arc::clone(&ords));
        Ok(ords)
    }
}

/// Term ordinals in `column` whose term matches `value`: an exact lookup,
/// or every term that lowercases to it.
fn resolve_ords(column: &StrColumn, value: &str, fold: bool) -> tantivy::Result<Vec<u64>> {
    let dictionary = column.dictionary();
    if !fold {
        return Ok(dictionary.term_ord(value.as_bytes())?.into_iter().collect());
    }
    let mut ords = Vec::new();
    let mut stream = dictionary.stream()?;
    while stream.advance() {
        if let Ok(term) = std::str::from_utf8(stream.key()) {
            if term.to_lowercase() == value {
                ords.push(stream.term_ord());
            }
        }
    }
    Ok(ords)
}

/// A filter bound to one segment's columns.
enum SegmentFilter {
    /// Match if any of the doc's term ordinals is in the (sorted) set.
    Str {
        column: StrColumn,
        ords: Arc<Vec<u64>>,
    },
    /// The column is absent or no term matches: nothing passes.
    Never,
    Num {
        column: Column<f64>,
        values: Vec<f64>,
    },
    Range {
        column: Column<f64>,
        min: f64,
        max: f64,
    },
}

impl SegmentFilter {
    fn bind(
        filter: &FastFilter,
        reader: &SegmentReader,
        cache: &OrdCache,
    ) -> tantivy::Result<SegmentFilter> {
        Ok(match filter {
            FastFilter::Str {
                field,
                values,
                fold,
            } => {
                let Some(column) = reader.fast_fields().str(field)? else {
                    return Ok(SegmentFilter::Never);
                };
                let mut ords: Vec<u64> = Vec::new();
                for value in values {
                    ords.extend(
                        cache
                            .ords_for(reader.segment_id(), &column, field, value, *fold)?
                            .iter()
                            .copied(),
                    );
                }
                if ords.is_empty() {
                    SegmentFilter::Never
                } else {
                    ords.sort_unstable();
                    ords.dedup();
                    SegmentFilter::Str {
                        column,
                        ords: Arc::new(ords),
                    }
                }
            }
            FastFilter::Num { field, values } => SegmentFilter::Num {
                column: reader.fast_fields().f64(field)?,
                values: values.clone(),
            },
            FastFilter::Range { field, min, max } => SegmentFilter::Range {
                column: reader.fast_fields().f64(field)?,
                min: *min,
                max: *max,
            },
        })
    }

    #[inline]
    fn accepts(&self, doc: DocId) -> bool {
        match self {
            SegmentFilter::Str { column, ords } => column
                .term_ords(doc)
                .any(|ord| ords.binary_search(&ord).is_ok()),
            SegmentFilter::Never => false,
            SegmentFilter::Num { column, values } => {
                column.first(doc).is_some_and(|v| values.contains(&v))
            }
            SegmentFilter::Range { column, min, max } => {
                column.first(doc).is_some_and(|v| v >= *min && v <= *max)
            }
        }
    }
}

/// Top-k by score over a pruning-capable query, keeping only documents
/// that pass every filter. Mirrors `TopDocs::with_limit(..).and_offset(..)`
/// in what it returns.
pub struct FilteredTopDocs {
    limit: usize,
    offset: usize,
    filters: Vec<FastFilter>,
    cache: Arc<OrdCache>,
}

impl FilteredTopDocs {
    pub fn new(
        limit: usize,
        offset: usize,
        filters: Vec<FastFilter>,
        cache: Arc<OrdCache>,
    ) -> Self {
        Self {
            limit: limit.max(1),
            offset,
            filters,
            cache,
        }
    }
}

/// A scored candidate in the segment heap. Ordered so that the heap's top
/// is the entry to evict: lowest score, and among equal scores the highest
/// doc id — the mirror image of `TopDocs`' (score desc, address asc).
struct Candidate {
    score: Score,
    doc: DocId,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed on both keys: BinaryHeap is a max-heap and we want the
        // worst candidate on top.
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.doc.cmp(&other.doc))
    }
}

pub struct FilteredTopSegment {
    segment_ord: SegmentOrdinal,
    filters: Vec<SegmentFilter>,
    k: usize,
    heap: BinaryHeap<Candidate>,
}

impl FilteredTopSegment {
    #[inline]
    fn accepts(&self, doc: DocId) -> bool {
        self.filters.iter().all(|f| f.accepts(doc))
    }

    /// Score the k-th best candidate holds once the heap is full — what a
    /// new candidate must beat, and what the pruning scorer skips below.
    #[inline]
    fn threshold(&self) -> Score {
        if self.heap.len() >= self.k {
            self.heap.peek().map_or(Score::MIN, |worst| worst.score)
        } else {
            Score::MIN
        }
    }

    #[inline]
    fn push(&mut self, doc: DocId, score: Score) {
        let candidate = Candidate { score, doc };
        // "Greater" means worse in this ordering (see `Candidate`): once
        // full, anything at least as bad as the current worst stays out.
        if self.heap.len() >= self.k {
            if let Some(worst) = self.heap.peek() {
                if candidate >= *worst {
                    return;
                }
            }
        }
        self.heap.push(candidate);
        if self.heap.len() > self.k {
            self.heap.pop();
        }
    }
}

impl Collector for FilteredTopDocs {
    type Fruit = Vec<(Score, DocAddress)>;
    type Child = FilteredTopSegment;

    fn for_segment(
        &self,
        segment_local_id: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<FilteredTopSegment> {
        let filters = self
            .filters
            .iter()
            .map(|f| SegmentFilter::bind(f, reader, &self.cache))
            .collect::<tantivy::Result<Vec<_>>>()?;
        let k = self.limit + self.offset;
        Ok(FilteredTopSegment {
            segment_ord: segment_local_id,
            filters,
            k,
            heap: BinaryHeap::with_capacity(k + 1),
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(
        &self,
        segment_fruits: Vec<Vec<(Score, DocAddress)>>,
    ) -> tantivy::Result<Vec<(Score, DocAddress)>> {
        let mut all: Vec<(Score, DocAddress)> = segment_fruits.into_iter().flatten().collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        Ok(all.into_iter().skip(self.offset).take(self.limit).collect())
    }

    /// The pruning entry point — the same loop `TopDocs` runs, with the
    /// filter check where it checks the alive bitset. A rejected document
    /// returns the threshold unchanged, so it never widens what pruning may
    /// skip.
    fn collect_segment(
        &self,
        weight: &dyn Weight,
        segment_ord: u32,
        reader: &SegmentReader,
    ) -> tantivy::Result<Vec<(Score, DocAddress)>> {
        let mut child = self.for_segment(segment_ord, reader)?;
        let alive = reader.alive_bitset();
        weight.for_each_pruning(Score::MIN, reader, &mut |doc, score| {
            if alive.is_some_and(|bitset| bitset.is_deleted(doc)) || !child.accepts(doc) {
                return child.threshold();
            }
            child.push(doc, score);
            child.threshold()
        })?;
        Ok(child.harvest())
    }
}

impl SegmentCollector for FilteredTopSegment {
    type Fruit = Vec<(Score, DocAddress)>;

    fn collect(&mut self, doc: DocId, score: Score) {
        if self.accepts(doc) {
            self.push(doc, score);
        }
    }

    fn harvest(self) -> Vec<(Score, DocAddress)> {
        let segment_ord = self.segment_ord;
        let mut hits: Vec<(Score, DocAddress)> = self
            .heap
            .into_iter()
            .map(|c| (c.score, DocAddress::new(segment_ord, c.doc)))
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        hits
    }
}

/// Exact match count under the same filters, without scoring. Deleted
/// documents are excluded by the default `collect_segment`.
pub struct FilteredCount {
    filters: Vec<FastFilter>,
    cache: Arc<OrdCache>,
}

impl FilteredCount {
    pub fn new(filters: Vec<FastFilter>, cache: Arc<OrdCache>) -> Self {
        Self { filters, cache }
    }
}

pub struct FilteredCountSegment {
    filters: Vec<SegmentFilter>,
    count: usize,
}

impl Collector for FilteredCount {
    type Fruit = usize;
    type Child = FilteredCountSegment;

    fn for_segment(
        &self,
        _segment_local_id: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<FilteredCountSegment> {
        let filters = self
            .filters
            .iter()
            .map(|f| SegmentFilter::bind(f, reader, &self.cache))
            .collect::<tantivy::Result<Vec<_>>>()?;
        Ok(FilteredCountSegment { filters, count: 0 })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, segment_fruits: Vec<usize>) -> tantivy::Result<usize> {
        Ok(segment_fruits.into_iter().sum())
    }
}

impl SegmentCollector for FilteredCountSegment {
    type Fruit = usize;

    fn collect(&mut self, doc: DocId, _score: Score) {
        if self.filters.iter().all(|f| f.accepts(doc)) {
            self.count += 1;
        }
    }

    fn harvest(self) -> usize {
        self.count
    }
}
