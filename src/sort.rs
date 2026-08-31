//! Global sorting by a string fast field.
//!
//! Sorting by a keyword/enum/boolean field used to fetch the page by
//! relevance and alphabetize just that page, so every page was sorted
//! internally but pages did not connect: page 2 could hold values that
//! belonged on page 1. This collector sorts the whole matching set.
//!
//! Cost model: within a segment, docs are compared by term ordinal — one u64
//! per doc from the fast-field column, no string in sight. Only the top
//! `k` survivors have their ordinals resolved to strings, which is what
//! makes cross-segment merging possible (ordinals from different segments
//! are not comparable, bytes are).

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::StrColumn;
use tantivy::{DocAddress, DocId, Score, SegmentOrdinal, SegmentReader};

/// Collect the top `k` documents ordered by a string fast field.
/// Documents without a value sort last in both directions; a multi-value
/// field sorts by its first value.
pub struct TopByStrField {
    field: String,
    k: usize,
    ascending: bool,
}

impl TopByStrField {
    pub fn new(field: &str, k: usize, ascending: bool) -> Self {
        Self {
            field: field.to_string(),
            k: k.max(1),
            ascending,
        }
    }

    fn compare(&self, a: &SortedHit, b: &SortedHit) -> Ordering {
        let by_value = match (&a.value, &b.value) {
            // Missing last, regardless of direction
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => {
                if self.ascending {
                    a.cmp(b)
                } else {
                    b.cmp(a)
                }
            }
        };
        // Deterministic page boundaries under ties
        by_value.then_with(|| a.address.cmp(&b.address))
    }
}

pub struct SortedHit {
    pub value: Option<String>,
    pub address: DocAddress,
}

impl Collector for TopByStrField {
    type Fruit = Vec<SortedHit>;
    type Child = TopByStrSegment;

    fn for_segment(
        &self,
        segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<TopByStrSegment> {
        Ok(TopByStrSegment {
            segment_ord: segment_local_id,
            column: segment.fast_fields().str(&self.field)?,
            k: self.k,
            ascending: self.ascending,
            heap: BinaryHeap::with_capacity(self.k + 1),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, segment_fruits: Vec<Vec<SortedHit>>) -> tantivy::Result<Vec<SortedHit>> {
        let mut all: Vec<SortedHit> = segment_fruits.into_iter().flatten().collect();
        all.sort_by(|a, b| self.compare(a, b));
        all.truncate(self.k);
        Ok(all)
    }
}

/// A doc in the segment-local heap. `rank` folds value and direction into
/// one key such that "smaller rank = better placed": the ordinal itself for
/// ascending, its complement for descending, and (missing-flag, _) puts
/// value-less docs behind every real value either way. The heap is a
/// max-heap on rank, so the doc at the top is always the one to evict.
struct HeapEntry {
    rank: (bool, u64),
    ord: u64,
    doc: DocId,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.rank == other.rank && self.doc == other.doc
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank.cmp(&other.rank)
    }
}

pub struct TopByStrSegment {
    segment_ord: SegmentOrdinal,
    column: Option<StrColumn>,
    k: usize,
    ascending: bool,
    heap: BinaryHeap<HeapEntry>,
}

impl SegmentCollector for TopByStrSegment {
    type Fruit = Vec<SortedHit>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        let ord = self
            .column
            .as_ref()
            .and_then(|col| col.term_ords(doc).next());
        let rank = match ord {
            Some(ord) if self.ascending => (false, ord),
            Some(ord) => (false, !ord),
            None => (true, 0),
        };
        self.heap.push(HeapEntry {
            rank,
            ord: ord.unwrap_or(0),
            doc,
        });
        if self.heap.len() > self.k {
            self.heap.pop();
        }
    }

    fn harvest(self) -> Vec<SortedHit> {
        let mut bytes = Vec::new();
        self.heap
            .into_iter()
            .map(|entry| {
                let value = match (&self.column, entry.rank.0) {
                    (Some(column), false) => {
                        bytes.clear();
                        match column.ord_to_bytes(entry.ord, &mut bytes) {
                            Ok(true) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                SortedHit {
                    value,
                    address: DocAddress::new(self.segment_ord, entry.doc),
                }
            })
            .collect()
    }
}
