//! Fuzzy *membership*: which documents count as matches of a text query,
//! defined on the inverted index alone.
//!
//! Relevance ranking retrieves a candidate pool by BM25 over a few rare
//! trigrams and reranks it by string similarity — right for "the best few",
//! wrong as a definition of "every match": a pool is bounded, so anything
//! that must see the whole match set (a field sort, an exact count) cannot
//! be built on it. Sorting the top-200 BM25 candidates by revenue returned
//! the richest of an arbitrary subset, with a count that changed with the
//! page size.
//!
//! A word matches a document when at least `k` of the word's `n` distinct
//! trigrams occur in one fuzzy field, with `k = max(n − 3, min(n, 2))`: one
//! edit destroys at most three consecutive trigrams, so words of seven
//! characters and up keep edit-distance-1 tolerance, while short words —
//! whose trigrams are too few to be discriminative — must match nearly
//! whole. A multi-word query requires every word. This is exact, cheap to
//! count, and independent of any page: the ordinary sort collectors run
//! straight over it.

use tantivy::query::{EmptyScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::schema::IndexRecordOption;
use tantivy::{DocId, DocSet, Score, SegmentReader, Term, TERMINATED};

/// How many of a word's `n` distinct trigrams a document must contain.
pub fn required_trigrams(n: usize) -> usize {
    n.saturating_sub(3).max(n.min(2))
}

/// Documents containing at least `at_least` of `terms` (all on one field).
#[derive(Debug, Clone)]
pub struct AtLeastQuery {
    terms: Vec<Term>,
    at_least: usize,
}

impl AtLeastQuery {
    pub fn new(terms: Vec<Term>, at_least: usize) -> Self {
        Self {
            terms,
            at_least: at_least.max(1),
        }
    }
}

impl Query for AtLeastQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(AtLeastWeight {
            terms: self.terms.clone(),
            at_least: self.at_least,
        }))
    }
}

struct AtLeastWeight {
    terms: Vec<Term>,
    at_least: usize,
}

impl Weight for AtLeastWeight {
    fn scorer(&self, reader: &SegmentReader, _boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let mut postings = Vec::with_capacity(self.terms.len());
        for term in &self.terms {
            let inverted = reader.inverted_index(term.field())?;
            if let Some(p) = inverted.read_postings(term, IndexRecordOption::Basic)? {
                postings.push(p);
            }
        }
        // Terms absent from this segment can never contribute; if too few
        // remain, nothing in the segment can reach the threshold.
        if postings.len() < self.at_least {
            return Ok(Box::new(EmptyScorer));
        }
        Ok(Box::new(AtLeastScorer::new(postings, self.at_least)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) == doc {
            Ok(Explanation::new(
                "terms matched (at-least query)",
                scorer.score(),
            ))
        } else {
            Err(tantivy::TantivyError::InvalidArgument(
                "document does not match".to_string(),
            ))
        }
    }
}

/// A counted multiway merge over the term postings: the next document at
/// which at least `at_least` of them coincide. Every posting list sits on
/// the current document or beyond it; the ones on it are stepped past when
/// the scorer advances.
struct AtLeastScorer<D: DocSet> {
    sets: Vec<D>,
    at_least: usize,
    doc: DocId,
    hits: usize,
}

impl<D: DocSet> AtLeastScorer<D> {
    fn new(sets: Vec<D>, at_least: usize) -> Self {
        let mut scorer = Self {
            sets,
            at_least,
            doc: 0,
            hits: 0,
        };
        scorer.doc = scorer.find_match();
        scorer
    }

    /// From the lists' current positions, the smallest document reached by
    /// at least `at_least` of them; lists on rejected documents are stepped
    /// past on the way.
    fn find_match(&mut self) -> DocId {
        loop {
            let candidate = self
                .sets
                .iter()
                .map(|s| s.doc())
                .min()
                .unwrap_or(TERMINATED);
            if candidate == TERMINATED {
                self.hits = 0;
                return TERMINATED;
            }
            let hits = self.sets.iter().filter(|s| s.doc() == candidate).count();
            if hits >= self.at_least {
                self.hits = hits;
                return candidate;
            }
            for set in self.sets.iter_mut() {
                if set.doc() == candidate {
                    set.advance();
                }
            }
        }
    }
}

impl<D: DocSet + 'static> DocSet for AtLeastScorer<D> {
    fn advance(&mut self) -> DocId {
        let current = self.doc;
        if current == TERMINATED {
            return TERMINATED;
        }
        for set in self.sets.iter_mut() {
            if set.doc() == current {
                set.advance();
            }
        }
        self.doc = self.find_match();
        self.doc
    }

    fn seek(&mut self, target: DocId) -> DocId {
        for set in self.sets.iter_mut() {
            if set.doc() < target {
                set.seek(target);
            }
        }
        self.doc = self.find_match();
        self.doc
    }

    fn doc(&self) -> DocId {
        self.doc
    }

    fn size_hint(&self) -> u32 {
        // An upper bound: no document can match without being in the
        // longest list.
        self.sets.iter().map(|s| s.size_hint()).max().unwrap_or(0)
    }
}

impl<D: DocSet + 'static> Scorer for AtLeastScorer<D> {
    /// How many of the terms matched — informative, never used for ranking.
    fn score(&mut self) -> Score {
        self.hits as Score
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::collector::{Count, DocSetCollector};
    use tantivy::schema::{Schema, TEXT};
    use tantivy::{doc, Index};

    #[test]
    fn required_trigrams_keeps_edit_tolerance_for_long_words_only() {
        assert_eq!(required_trigrams(1), 1); // 3-char word: its one trigram
        assert_eq!(required_trigrams(2), 2); // "berg": both, no wildcards
        assert_eq!(required_trigrams(4), 2);
        assert_eq!(required_trigrams(5), 2); // "bergsen" survives one edit
        assert_eq!(required_trigrams(6), 3);
        assert_eq!(required_trigrams(9), 6); // "sparebanken": n - 3
        assert_eq!(required_trigrams(12), 9);
    }

    /// Words as whole terms, so the counting is easy to see: a document
    /// matches when it contains at least k of the query's words.
    #[test]
    fn matches_documents_with_at_least_k_of_n_terms() {
        let mut builder = Schema::builder();
        let body = builder.add_text_field("body", TEXT);
        let index = Index::create_in_ram(builder.build());
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(15_000_000)
            .unwrap();
        // doc 0: 3 of 4 · doc 1: 1 · doc 2: 4 · doc 3: 0 · doc 4: 2
        for text in [
            "alpha beta gamma",
            "alpha other",
            "alpha beta gamma delta",
            "nothing here",
            "gamma delta",
        ] {
            writer.add_document(doc!(body => text)).unwrap();
        }
        writer.commit().unwrap();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let terms: Vec<Term> = ["alpha", "beta", "gamma", "delta"]
            .iter()
            .map(|w| Term::from_field_text(body, w))
            .collect();

        let matching = |k: usize| -> Vec<u32> {
            let query = AtLeastQuery::new(terms.clone(), k);
            let mut docs: Vec<u32> = searcher
                .search(&query, &DocSetCollector)
                .unwrap()
                .into_iter()
                .map(|addr| addr.doc_id)
                .collect();
            docs.sort_unstable();
            assert_eq!(
                searcher.search(&query, &Count).unwrap(),
                docs.len(),
                "count agrees with the doc set"
            );
            docs
        };
        assert_eq!(matching(1), vec![0, 1, 2, 4]);
        assert_eq!(matching(2), vec![0, 2, 4]);
        assert_eq!(matching(3), vec![0, 2]);
        assert_eq!(matching(4), vec![2]);
        assert_eq!(matching(5), Vec::<u32>::new(), "more than exist: nothing");
    }

    /// Seeking must land on the next match at or past the target and keep
    /// every list consistent afterwards.
    #[test]
    fn seek_lands_on_the_next_match() {
        let mut builder = Schema::builder();
        let body = builder.add_text_field("body", TEXT);
        let index = Index::create_in_ram(builder.build());
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(15_000_000)
            .unwrap();
        for i in 0..200u32 {
            // Every third doc has both words; the others have one or none.
            let text = match i % 3 {
                0 => "alpha beta",
                1 => "alpha",
                _ => "beta",
            };
            writer.add_document(doc!(body => text)).unwrap();
        }
        writer.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();
        let segment = searcher.segment_reader(0);
        let terms = vec![
            Term::from_field_text(body, "alpha"),
            Term::from_field_text(body, "beta"),
        ];
        let weight = AtLeastQuery::new(terms, 2)
            .weight(EnableScoring::disabled_from_searcher(&searcher))
            .unwrap();
        let mut scorer = weight.scorer(segment, 1.0).unwrap();
        assert_eq!(scorer.doc(), 0);
        assert_eq!(scorer.seek(4), 6);
        assert_eq!(scorer.seek(6), 6, "seeking to the current doc stays");
        assert_eq!(scorer.advance(), 9);
        assert_eq!(scorer.seek(100), 102);
        assert_eq!(scorer.seek(199), TERMINATED);
    }
}
