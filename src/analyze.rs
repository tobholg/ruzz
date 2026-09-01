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
