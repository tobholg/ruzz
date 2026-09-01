//! Text analysis shared by the index and the query side.
//!
//! The trigram tokenizer and `query_trigrams` in search must produce the
//! same bytes for the same input or fuzzy matching silently breaks; both
//! call `fold` here, and nothing else may normalize query or index text.
//!
//! Folding lowercases and reduces European Latin text to its unaccented
//! skeleton: `Müller` → `muller`, `Café` → `cafe`, `Sørlandet` →
//! `sorlandet`, `Sæter` → `saeter`. Combining marks are dropped, so NFC and
//! NFD encodings of the same string fold identically. This is deliberately a
//! transliteration table, not full Unicode normalization — it covers the
//! Latin scripts this engine is used with, byte-for-byte reproducibly.
//!
//! Folding happens BEFORE trigramming, never per-trigram: mappings like
//! `æ → ae` change length, and folding trigrams after the fact would emit
//! 4-char tokens the query side can never produce.

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// One-or-two-character transliterations that a per-character diacritic
/// strip cannot express.
fn fold_char(c: char, out: &mut String) {
    match c {
        'æ' => out.push_str("ae"),
        'œ' => out.push_str("oe"),
        'ß' => out.push_str("ss"),
        'þ' => out.push_str("th"),
        'ð' | 'đ' => out.push('d'),
        'ø' => out.push('o'),
        'å' | 'à' | 'á' | 'â' | 'ã' | 'ä' | 'ā' | 'ă' | 'ą' => out.push('a'),
        'ç' | 'ć' | 'č' => out.push('c'),
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => out.push('e'),
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => out.push('i'),
        'ğ' => out.push('g'),
        'ľ' | 'ĺ' | 'ł' => out.push('l'),
        'ñ' | 'ń' | 'ň' => out.push('n'),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ō' | 'ő' => out.push('o'),
        'ř' => out.push('r'),
        'š' | 'ś' | 'ş' => out.push('s'),
        'ť' | 'ţ' => out.push('t'),
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => out.push('u'),
        'ý' | 'ÿ' => out.push('y'),
        'ž' | 'ź' | 'ż' => out.push('z'),
        // Combining diacritical marks: dropping them makes decomposed input
        // (e + U+0301) fold the same as precomposed (é).
        '\u{0300}'..='\u{036F}' => {}
        _ => out.push(c),
    }
}

/// Lowercase + transliterate. The single normalization used everywhere.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        for lc in c.to_lowercase() {
            fold_char(lc, &mut out);
        }
    }
    out
}

/// Eager token stream: analysis happens up front on an owned folded copy,
/// which is what lets folding run before tokenization.
pub struct VecTokenStream {
    tokens: Vec<Token>,
    index: usize,
}

impl VecTokenStream {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, index: 0 }
    }
}

impl TokenStream for VecTokenStream {
    fn advance(&mut self) -> bool {
        if self.index < self.tokens.len() {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        &self.tokens[self.index - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.index - 1]
    }
}

/// Trigrams over the folded text — the index side of fuzzy and substring
/// matching. Offsets are character offsets into the folded string; nothing
/// reads them (no highlighting, no positions), they only need to be
/// monotonic.
#[derive(Clone, Default)]
pub struct FoldingTrigramTokenizer;

impl Tokenizer for FoldingTrigramTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecTokenStream {
        let folded = fold(text);
        let chars: Vec<char> = folded.chars().collect();
        let mut tokens = Vec::new();
        if chars.len() >= 3 {
            for i in 0..=chars.len() - 3 {
                tokens.push(Token {
                    offset_from: i,
                    offset_to: i + 3,
                    position: i,
                    text: chars[i..i + 3].iter().collect(),
                    position_length: 1,
                });
            }
        }
        VecTokenStream::new(tokens)
    }
}

/// One- and two-character word prefixes over the folded text — the index
/// side of typeahead. "Sørlandet Eiendom" emits s, so, e, ei; a query too
/// short to form a trigram matches through these instead of matching
/// nothing.
#[derive(Clone, Default)]
pub struct EdgePrefixTokenizer;

impl Tokenizer for EdgePrefixTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecTokenStream {
        let folded = fold(text);
        let mut tokens = Vec::new();
        let mut position = 0;
        for word in folded.split(|c: char| !c.is_alphanumeric()) {
            if word.is_empty() {
                continue;
            }
            let chars: Vec<char> = word.chars().collect();
            for len in [1usize, 2] {
                if chars.len() < len {
                    break;
                }
                tokens.push(Token {
                    offset_from: position,
                    offset_to: position + len,
                    position,
                    text: chars[..len].iter().collect(),
                    position_length: 1,
                });
                position += 1;
            }
        }
        VecTokenStream::new(tokens)
    }
}

// ── string similarity (the rerank stage) ───────────────────────────────────

/// Jaro-Winkler over chars: 1.0 for identical strings, 0.0 for disjoint,
/// with the Winkler prefix bonus that suits names (typos cluster at the
/// end, prefixes carry identity).
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut b_taken = vec![false; b.len()];
    let mut matches = 0usize;
    let mut a_matched = Vec::with_capacity(a.len());
    for (i, &ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_taken[j] && b[j] == ca {
                b_taken[j] = true;
                matches += 1;
                a_matched.push(ca);
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let b_matched: Vec<char> = b
        .iter()
        .zip(&b_taken)
        .filter(|(_, taken)| **taken)
        .map(|(c, _)| *c)
        .collect();
    let transpositions = a_matched
        .iter()
        .zip(&b_matched)
        .filter(|(x, y)| x != y)
        .count()
        / 2;
    let m = matches as f64;
    let jaro = (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0;
    let prefix = a.iter().zip(&b).take(4).take_while(|(x, y)| x == y).count() as f64;
    jaro + prefix * 0.1 * (1.0 - jaro)
}

/// Similarity between a folded query and a folded field value, word-aware:
/// each query word takes its best match among the value's words, weighted
/// by word length. Order-invariant ("kraft berg" ≈ "berg kraft as") and
/// indifferent to extra value words (legal-form suffixes, middle names) —
/// what the query asked for has to be present; what it didn't is free.
pub fn name_similarity(query_folded: &str, value_folded: &str) -> f64 {
    let value_words: Vec<&str> = value_folded.split_whitespace().collect();
    if value_words.is_empty() {
        return 0.0;
    }
    let mut weighted = 0.0;
    let mut weight = 0.0;
    for query_word in query_folded.split_whitespace() {
        let best = value_words
            .iter()
            .map(|value_word| jaro_winkler(query_word, value_word))
            .fold(0.0, f64::max);
        let w = query_word.chars().count() as f64;
        weighted += best * w;
        weight += w;
    }
    if weight == 0.0 {
        0.0
    } else {
        weighted / weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_european_text_to_its_skeleton() {
        assert_eq!(fold("Müller"), "muller");
        assert_eq!(fold("Café"), "cafe");
        assert_eq!(fold("SØRLANDET"), "sorlandet");
        assert_eq!(fold("Sæter Gård"), "saeter gard");
        assert_eq!(fold("Straße"), "strasse");
        assert_eq!(fold("Ærø"), "aero");
        assert_eq!(fold("plain ascii 123"), "plain ascii 123");
    }

    #[test]
    fn nfc_and_nfd_fold_identically() {
        // "é" precomposed vs "e" + combining acute
        assert_eq!(fold("caf\u{00E9}"), fold("cafe\u{0301}"));
        // "å" precomposed vs "a" + combining ring
        assert_eq!(fold("\u{00E5}s"), fold("a\u{030A}s"));
    }

    #[test]
    fn trigram_tokenizer_folds_before_slicing() {
        let mut tokenizer = FoldingTrigramTokenizer;
        let mut stream = tokenizer.token_stream("Sæt");
        let mut texts = Vec::new();
        while stream.advance() {
            texts.push(stream.token().text.clone());
        }
        // Folded to "saet" first, then trigrammed — never a 4-char token.
        assert_eq!(texts, vec!["sae", "aet"]);
    }

    #[test]
    fn jaro_winkler_behaves_at_the_edges() {
        assert_eq!(jaro_winkler("bergsen", "bergsen"), 1.0);
        assert_eq!(jaro_winkler("abc", "xyz"), 0.0);
        assert_eq!(jaro_winkler("", ""), 1.0);
        assert_eq!(jaro_winkler("abc", ""), 0.0);
        // One late typo stays close; prefix bonus keeps it above a reshuffle.
        let typo = jaro_winkler("bergsen", "bergson");
        assert!(typo > 0.9, "late typo scores high, got {typo}");
        let scramble = jaro_winkler("bergsen", "nesgreb");
        assert!(typo > scramble);
    }

    #[test]
    fn name_similarity_ignores_word_order_and_suffixes() {
        let straight = name_similarity("berg kraft", "berg kraft as");
        let reordered = name_similarity("kraft berg", "berg kraft as");
        assert!((straight - reordered).abs() < 1e-9, "order must not matter");
        assert!(straight > 0.99, "extra suffix words are free");
        // A query word with no counterpart drags the score down.
        let missing = name_similarity("berg kraft nordvik", "berg kraft as");
        assert!(missing < straight - 0.1);
    }

    #[test]
    fn prefix_tokenizer_emits_per_word_edges() {
        let mut tokenizer = EdgePrefixTokenizer;
        let mut stream = tokenizer.token_stream("Sørlandet Eiendom");
        let mut texts = Vec::new();
        while stream.advance() {
            texts.push(stream.token().text.clone());
        }
        assert_eq!(texts, vec!["s", "so", "e", "ei"]);
    }
}
