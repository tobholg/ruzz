use std::collections::HashMap;
use tantivy::schema::*;

use crate::config::{FieldType, SchemaConfig, SearchMode};

/// Build a Tantivy schema from config, return (Schema, field_name → Field map)
pub fn build_schema(config: &SchemaConfig) -> (Schema, HashMap<String, Field>) {
    let mut builder = Schema::builder();
    let mut field_map = HashMap::new();

    for fc in &config.fields {
        let field = match fc.field_type {
            FieldType::Text => {
                if fc.search == Some(SearchMode::Fuzzy) {
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
            FieldType::Keyword => {
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
            FieldType::Enum | FieldType::Boolean => {
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
            FieldType::Number => {
                // Store as f64 for flexibility (revenue, ratios, etc.)
                // FAST for columnar access (sort/range), STORED for retrieval
                builder.add_f64_field(&fc.name, NumericOptions::default().set_fast().set_stored().set_indexed())
            }
        };
        field_map.insert(fc.name.clone(), field);
    }

    (builder.build(), field_map)
}
