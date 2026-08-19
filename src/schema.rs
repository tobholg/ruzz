use std::collections::HashMap;
use tantivy::schema::*;

use crate::config::{FieldType, SchemaConfig, SearchMode};

/// Sequential import ordinal linking an index row to its full document in the
/// on-disk store. Only present when the store is enabled. Not stable across
/// re-imports — external systems should persist user keys, not refs.
pub const REF_FIELD: &str = "_ref";

/// Whole-value tokenizer that lowercases, so keyword filters match
/// regardless of the caller's casing. Stored values keep their original
/// case — only the indexed term is folded.
pub const KEYWORD_CI_TOKENIZER: &str = "keyword_ci";

/// Build a Tantivy schema from config, return (Schema, field_name → Field map)
pub fn build_schema(config: &SchemaConfig, with_ref: bool) -> (Schema, HashMap<String, Field>) {
    let mut builder = Schema::builder();
    let mut field_map = HashMap::new();

    if with_ref {
        // STORED so search hits carry it; FAST so future features (delete by
        // ref, hydration push-down) get columnar access. Not indexed.
        builder.add_u64_field(REF_FIELD, NumericOptions::default().set_stored().set_fast());
    }

    for fc in &config.fields {
        let field = match fc.field_type {
            FieldType::Text => {
                if matches!(
                    fc.search,
                    Some(SearchMode::Fuzzy) | Some(SearchMode::Substring)
                ) {
                    let options = TextOptions::default()
                        .set_indexing_options(
                            TextFieldIndexing::default()
                                .set_tokenizer("trigram")
                                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                        )
                        .set_stored();
                    builder.add_text_field(&fc.name, options)
                } else {
                    builder.add_text_field(&fc.name, TEXT | STORED)
                }
            }
            FieldType::Keyword => builder.add_text_field(
                &fc.name,
                TextOptions::default()
                    .set_indexing_options(
                        TextFieldIndexing::default()
                            .set_tokenizer(if fc.case_sensitive {
                                "raw"
                            } else {
                                KEYWORD_CI_TOKENIZER
                            })
                            .set_index_option(IndexRecordOption::Basic),
                    )
                    .set_stored()
                    // Fast field keeps the raw value, so sorting is unaffected
                    .set_fast(None),
            ),
            // Enums match case-insensitively like keywords, through the
            // index term rather than by rewriting the value. Booleans keep
            // the raw tokenizer: they are canonicalised to TRUE/FALSE, so
            // there is no original casing to preserve.
            FieldType::Enum => builder.add_text_field(
                &fc.name,
                TextOptions::default()
                    .set_indexing_options(
                        TextFieldIndexing::default()
                            .set_tokenizer(if fc.case_sensitive {
                                "raw"
                            } else {
                                KEYWORD_CI_TOKENIZER
                            })
                            .set_index_option(IndexRecordOption::Basic),
                    )
                    .set_stored()
                    .set_fast(None),
            ),
            FieldType::Boolean => builder.add_text_field(
                &fc.name,
                TextOptions::default()
                    .set_indexing_options(
                        TextFieldIndexing::default()
                            .set_tokenizer("raw")
                            .set_index_option(IndexRecordOption::Basic),
                    )
                    .set_stored()
                    .set_fast(None),
            ),
            FieldType::Number => {
                // Store as f64 for flexibility (revenue, ratios, etc.)
                // FAST for columnar access (sort/range), STORED for retrieval
                builder.add_f64_field(
                    &fc.name,
                    NumericOptions::default()
                        .set_fast()
                        .set_stored()
                        .set_indexed(),
                )
            }
        };
        field_map.insert(fc.name.clone(), field);
    }

    (builder.build(), field_map)
}
