use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tantivy::schema::Schema;
use tantivy::{IndexWriter, TantivyDocument};

use crate::config::{Config, FieldType, SourceConfig};
use crate::schema::build_schema;

pub struct ImportStats {
    pub total_rows: u64,
    pub total_duration_secs: f64,
    pub per_source: Vec<SourceStats>,
}

pub struct SourceStats {
    pub path: String,
    pub rows: u64,
    pub duration_secs: f64,
}

/// Count lines in a file quickly (for progress bar total)
fn count_lines(path: &Path) -> u64 {
    use std::io::{BufRead, BufReader};
    let file = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return 0 };
    let reader = BufReader::with_capacity(256 * 1024, file);
    let mut count = 0u64;
    for _ in reader.lines() { count += 1; }
    // Subtract header row
    count.saturating_sub(1)
}

pub fn run_import(config: &Config) -> anyhow::Result<ImportStats> {
    let (schema, field_map) = build_schema(&config.schema);

    // Create or open index
    let index_path = &config.server.index_path;
    if index_path.exists() {
        std::fs::remove_dir_all(index_path)?;
    }
    std::fs::create_dir_all(index_path)?;

    let index = tantivy::Index::create_in_dir(index_path, schema.clone())?;

    // Register trigram tokenizer for fuzzy fields
    register_trigram_tokenizer(&index);

    let mut writer: IndexWriter = index.writer(256_000_000)?; // 256MB heap

    let multi = MultiProgress::new();
    let style = ProgressStyle::with_template(
        "{prefix:<30} {bar:30.cyan/dim} {pos:>10}/{len:10} {per_sec:>12} ETA {eta}"
    )
    .unwrap()
    .progress_chars("██░");

    let start = Instant::now();
    let mut stats = ImportStats {
        total_rows: 0,
        total_duration_secs: 0.0,
        per_source: Vec::new(),
    };

    for source in &config.sources {
        let source_start = Instant::now();
        let mapping = source.resolved_mapping(&config.mappings);
        let file_name = source.path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| source.path.display().to_string());

        // Count lines for progress bar
        let line_count = count_lines(&source.path);
        let pb = multi.add(ProgressBar::new(line_count));
        pb.set_style(style.clone());
        pb.set_prefix(file_name.clone());

        let rows = import_csv(
            &source.path,
            &mapping,
            &source.defaults,
            &schema,
            &field_map,
            &config.schema.fields,
            &mut writer,
            &pb,
        )?;

        pb.finish();

        let duration = source_start.elapsed().as_secs_f64();
        stats.per_source.push(SourceStats {
            path: file_name,
            rows,
            duration_secs: duration,
        });
        stats.total_rows += rows;
    }

    // Commit
    let commit_pb = multi.add(ProgressBar::new_spinner());
    commit_pb.set_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap());
    commit_pb.set_message("Committing index...");
    writer.commit()?;
    commit_pb.finish_with_message("Index committed.");

    stats.total_duration_secs = start.elapsed().as_secs_f64();

    println!(
        "\n✓ {} rows indexed in {:.1}s → {}",
        stats.total_rows,
        stats.total_duration_secs,
        config.server.index_path.display()
    );

    Ok(stats)
}

fn import_csv(
    path: &Path,
    mapping: &HashMap<String, String>,
    defaults: &HashMap<String, String>,
    schema: &Schema,
    field_map: &HashMap<String, tantivy::schema::Field>,
    field_configs: &[crate::config::FieldConfig],
    writer: &mut IndexWriter,
    pb: &ProgressBar,
) -> anyhow::Result<u64> {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(path)?;

    let headers: Vec<String> = rdr.headers()?.iter().map(|s| s.to_string()).collect();

    // Build reverse mapping: schema_field_name → csv_column_index
    let mut col_indices: HashMap<String, usize> = HashMap::new();
    for (schema_name, csv_col_name) in mapping {
        if let Some(idx) = headers.iter().position(|h| h == csv_col_name) {
            col_indices.insert(schema_name.clone(), idx);
        }
    }

    let mut count = 0u64;
    let mut record = csv::StringRecord::new();

    while rdr.read_record(&mut record)? {
        let mut doc = TantivyDocument::new();

        for fc in field_configs {
            let field = match field_map.get(&fc.name) {
                Some(f) => *f,
                None => continue,
            };

            // Try CSV column first, then defaults
            let value = col_indices.get(&fc.name)
                .and_then(|&idx| record.get(idx))
                .map(|s| s.to_string())
                .or_else(|| defaults.get(&fc.name).cloned())
                .unwrap_or_default();

            if value.is_empty() {
                continue;
            }

            match fc.field_type {
                FieldType::Text | FieldType::Keyword => {
                    doc.add_text(field, &value);
                }
            }
        }

        writer.add_document(doc)?;
        count += 1;

        if count % 10_000 == 0 {
            pb.set_position(count);
        }
    }

    pb.set_position(count);
    Ok(count)
}

/// Register a trigram tokenizer for fuzzy text matching
fn register_trigram_tokenizer(index: &tantivy::Index) {
    register_trigram_tokenizer_pub(index);
}

pub fn register_trigram_tokenizer_pub(index: &tantivy::Index) {
    use tantivy::tokenizer::*;

    let tokenizer = TextAnalyzer::builder(NgramTokenizer::new(2, 4, false).unwrap())
        .filter(LowerCaser)
        .build();

    index.tokenizers().register("trigram", tokenizer);
}
