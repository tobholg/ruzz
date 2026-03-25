use std::collections::HashMap;
use tantivy::schema::*;

use crate::config::{FieldConfig, FieldType, SchemaConfig, SearchMode};

/// Build a Tantivy schema from config, return (Schema, field_name → Field map)
pub fn build_schema(config: &SchemaConfig) -> (Schema, HashMap<String, Field>) {
    let mut builder = Schema::builder();
    let mut field_map = HashMap::new();

    for fc in &config.fields {
        let field = match fc.field_type {
            FieldType::Text => {
                if fc.search == Some(SearchMode::Fuzzy) {
                    // Fuzzy text: index with trigram tokenizer + store for retrieval
                    let options = TextOptions::default()
                        .set_indexing_options(
                            TextFieldIndexing::default()
                                .set_tokenizer("trigram")
                                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                        )
                        .set_stored();
                    builder.add_text_field(&fc.name, options)
                } else {
                    // Regular text: default tokenizer + stored
                    builder.add_text_field(&fc.name, TEXT | STORED)
                }
            }
            FieldType::Keyword => {
                // Keyword: indexed as-is (no tokenization) + stored + fast field for filtering
                builder.add_text_field(
                    &fc.name,
                    TextOptions::default()
                        .set_indexing_options(
                            TextFieldIndexing::default()
                                .set_tokenizer("raw")
                                .set_index_option(IndexRecordOption::Basic),
                        )
                        .set_stored()
                        .set_fast(None),
                )
            }
        };
        field_map.insert(fc.name.clone(), field);
    }

    (builder.build(), field_map)
}
