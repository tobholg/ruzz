use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tantivy::schema::Schema;
use tantivy::{IndexWriter, TantivyDocument};

use crate::config::{Config, FieldType, StoreSource};
use crate::field_meta::{
    canonicalize_stored_value, write_stored_field_metadata, ImportFieldMetadataCollector,
};
use crate::schema::{build_schema, REF_FIELD};
use crate::store;

#[derive(Debug)]
pub struct ImportStats {
    pub total_rows: u64,
    pub total_duration_secs: f64,
    pub per_source: Vec<SourceStats>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct SourceStats {
    pub path: String,
    pub rows: u64,
    pub duration_secs: f64,
}

/// Delete index files the current commit no longer references.
///
/// Safe to run against a live index: on Unix the inode outlives the
/// directory entry, so a serving process keeps reading the files it has
/// already mapped. The space it holds is returned when it next restarts.
pub fn collect_garbage(config: &Config) -> anyhow::Result<()> {
    let index_path = &config.server.index_path;
    let before = dir_bytes(index_path);
    let index = tantivy::Index::open_in_dir(index_path)
        .with_context(|| format!("opening index at {}", index_path.display()))?;
    let writer: IndexWriter = index.writer(15_000_000)?;
    let outcome = writer
        .garbage_collect_files()
        .wait()
        .map_err(|e| anyhow::anyhow!("garbage collection failed: {}", e))?;
    let after = dir_bytes(index_path);
    println!(
        "✓ removed {} unreferenced files, {} → {} ({} reclaimed)",
        outcome.deleted_files.len(),
        human_bytes(before),
        human_bytes(after),
        human_bytes(before.saturating_sub(after)),
    );
    if outcome.deleted_files.is_empty() {
        println!("  nothing to collect — this index is already clean");
    } else {
        println!("  a running server keeps its mapped files until it restarts");
    }
    Ok(())
}

fn dir_bytes(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{:.2} {}", value, UNITS[unit])
}

/// Count lines in a file quickly (for progress bar total)
fn count_lines(path: &Path) -> u64 {
    use std::io::{BufRead, BufReader};
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let reader = BufReader::with_capacity(256 * 1024, file);
    let mut count = 0u64;
    for _ in reader.lines() {
        count += 1;
    }
    // Subtract header row
    count.saturating_sub(1)
}

/// Everything the per-source import loop needs to feed the document store.
struct StoreImport {
    writer: store::StoreWriter,
    mode: StoreSource,
    next_ref: u64,
}

pub fn run_import(config: &Config) -> anyhow::Result<ImportStats> {
    let store_enabled = config.store.enabled;
    let (schema, field_map) = build_schema(&config.schema, store_enabled);
    // Imports always write to the current default (or an explicit [store]
    // path). A store left at the pre-0.2 location is not reused: that shared
    // path is exactly what let one index clobber another's store.
    let store_path = config.store_path();
    let legacy_path = config.legacy_store_path();
    let legacy_in_the_way = legacy_path != store_path && store::looks_like_store_dir(&legacy_path);

    // Validate everything that can fail BEFORE wiping the previous index.
    if store_enabled {
        if config.store.source == StoreSource::Sidecar {
            for source in &config.sources {
                let sidecar = source.sidecar.as_ref().expect("validated by config");
                if !sidecar.exists() {
                    bail!("sidecar file not found: {}", sidecar.display());
                }
            }
        }
        if store_path.exists() && !store::looks_like_store_dir(&store_path) {
            bail!(
                "store path {} exists but does not look like a ruzz store — refusing to overwrite",
                store_path.display()
            );
        }
    }

    // Build into staging directories beside the final paths. The previous
    // index used to be deleted before the first row was read, so a failed
    // or interrupted import left nothing to serve; now it survives, live
    // and untouched, until the replacement is complete — only then do a
    // couple of renames put the new one in place. A crash mid-import
    // leaves at worst a *.building leftover, cleared on the next run.
    let final_index_path = config.server.index_path.clone();
    let index_path = staging_path(&final_index_path);
    let final_store_path = store_path;
    let store_path = staging_path(&final_store_path);
    for stale in [&index_path, &store_path] {
        if stale.exists() {
            std::fs::remove_dir_all(stale).with_context(|| {
                format!("clearing leftover staging directory {}", stale.display())
            })?;
        }
    }
    std::fs::create_dir_all(&index_path)?;

    let mut store_import = if store_enabled {
        std::fs::create_dir_all(&store_path)?;
        if legacy_in_the_way {
            println!(
                "  note: a store also exists at the legacy path {} — it belongs to no index now and can be deleted",
                legacy_path.display()
            );
        }
        let block_size = store::parse_size(&config.store.block_size)
            .with_context(|| format!("invalid store block_size '{}'", config.store.block_size))?;
        let writer =
            store::StoreWriter::create(&store_path, block_size, config.store.compression_level)?;
        Some(StoreImport {
            writer,
            mode: config.store.source,
            next_ref: 0,
        })
    } else {
        None
    };

    let index = tantivy::Index::create_in_dir(&index_path, schema.clone())?;

    // Register trigram tokenizer for fuzzy fields
    register_trigram_tokenizer(&index);

    let mut writer: IndexWriter = index.writer(256_000_000)?; // 256MB heap
    let mut metadata_collector = ImportFieldMetadataCollector::new(&config.schema.fields);

    let multi = MultiProgress::new();
    let style = ProgressStyle::with_template(
        "{prefix:<30} {bar:30.cyan/dim} {pos:>10}/{len:10} {per_sec:>12} ETA {eta}",
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
        let file_name = source
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| source.path.display().to_string());

        // Count lines for progress bar
        let line_count = count_lines(&source.path);
        let pb = multi.add(ProgressBar::new(line_count));
        pb.set_style(style.clone());
        pb.set_prefix(file_name.clone());

        let rows = import_csv(
            &source.path,
            source.sidecar.as_deref(),
            &mapping,
            &source.defaults,
            &schema,
            &field_map,
            &config.schema.fields,
            &mut writer,
            &mut metadata_collector,
            store_import.as_mut(),
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

    // Merge segments for faster queries — use the same writer.
    // The store writer thread keeps compressing in parallel with this.
    let merge_pb = multi.add(ProgressBar::new_spinner());
    merge_pb.set_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap());
    merge_pb.set_message("Merging segments (this improves query speed)...");
    let segment_ids = index.searchable_segment_ids()?;
    if segment_ids.len() > 1 {
        // merge() hands the work to the merge thread pool and returns a handle
        // to the result. We do not need the resulting SegmentMeta, and dropping
        // the handle does not cancel the merge — wait_merging_threads() below is
        // what blocks until the pool has finished.
        drop(writer.merge(&segment_ids));
        writer.wait_merging_threads()?;

        // Merging leaves the pre-merge segments on disk. Tantivy only removes
        // files no longer referenced by meta.json during a garbage collect,
        // and the merge here happens after the final commit, so nothing ever
        // triggered one — every index carried its own superseded segments
        // forever. Measured before this ran: a 16GB index dragging 27GB of
        // dead files behind it, 63% of the directory.
        //
        // wait_merging_threads() consumes the writer, so collect with a fresh
        // one. Deleting is safe for a reader that still has a file mapped: on
        // Unix the inode outlives the directory entry, so a serving process
        // keeps working and the space is returned when it next restarts.
        let collector: IndexWriter = index.writer(15_000_000)?;
        match collector.garbage_collect_files().wait() {
            Ok(result) => {
                let freed = result.deleted_files.len();
                if freed > 0 {
                    merge_pb.set_message(format!("Collected {} superseded files.", freed));
                }
            }
            // Reclaiming disk is not worth failing a finished import over.
            Err(e) => eprintln!("  note: could not collect superseded segments: {}", e),
        }
    }
    merge_pb.finish_with_message("Segments merged.");

    write_stored_field_metadata(&index_path, &metadata_collector.into_stored())?;

    // Finalize the store: meta.json is written last, after both the index
    // commit and the store data files succeeded (commit marker semantics).
    if let Some(store_import) = store_import.take() {
        let mode = store_import.mode;
        let doc_count = store_import.next_ref;
        let outcome = store_import.writer.finish()?;
        if outcome.doc_count != doc_count {
            bail!(
                "store wrote {} docs but import produced {}",
                outcome.doc_count,
                doc_count
            );
        }
        let generation = store::new_generation();
        let meta = store::StoreMeta {
            format_version: 1,
            generation: generation.clone(),
            doc_count: outcome.doc_count,
            block_count: outcome.block_count,
            raw_bytes: outcome.raw_bytes,
            compressed_bytes: outcome.compressed_bytes,
            compression: "zstd".to_string(),
            compression_level: config.store.compression_level,
            source_mode: mode.as_str().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        store::write_meta(&store_path, &meta)?;
        store::write_index_pairing(&index_path, &generation)?;
        println!(
            "✓ document store: {} docs, {} raw → {} on disk ({:.1}x) → {}",
            outcome.doc_count,
            format_bytes(outcome.raw_bytes),
            format_bytes(outcome.compressed_bytes),
            outcome.raw_bytes.max(1) as f64 / outcome.compressed_bytes.max(1) as f64,
            final_store_path.display()
        );
    }

    // Everything is built and internally consistent — put it live. Store
    // first: if interrupted between the two swaps, the old index pairs with
    // the new store, the generation check fails safe (store endpoints down,
    // search up on the old data) and the next import heals it.
    if store_enabled {
        swap_into_place(&store_path, &final_store_path)?;
    } else {
        // A store left behind at either path would outlive the index it was
        // built for, and the legacy one would still be picked up when
        // serving. Cleared only now: the build has succeeded, so final
        // state may be touched.
        for stale in [&final_store_path, &legacy_path] {
            if store::looks_like_store_dir(stale) {
                std::fs::remove_dir_all(stale)?;
                println!("  removed stale document store at {}", stale.display());
            }
        }
    }
    swap_into_place(&index_path, &final_index_path)?;

    stats.total_duration_secs = start.elapsed().as_secs_f64();

    println!(
        "\n✓ {} rows indexed in {:.1}s → {}",
        stats.total_rows,
        stats.total_duration_secs,
        config.server.index_path.display()
    );

    Ok(stats)
}

/// Sibling staging directory a new index or store is built into before the
/// swap: "<path>.building". Beside the destination, so the final rename
/// stays on one filesystem and therefore atomic.
fn staging_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "index".to_string());
    path.with_file_name(format!("{name}.building"))
}

/// Replace `dest` with `build` by rename. Each rename is atomic; between
/// the two there is a moment where `dest` is absent and "<dest>.previous"
/// holds the old data — a crash in that window is recoverable by hand and
/// the next import clears the leftovers. A serving process keeps the files
/// it has mapped either way (the inode outlives the directory entry) and
/// picks the new data up on restart.
fn swap_into_place(build: &Path, dest: &Path) -> anyhow::Result<()> {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "index".to_string());
    let previous = dest.with_file_name(format!("{name}.previous"));
    if previous.exists() {
        std::fs::remove_dir_all(&previous)
            .with_context(|| format!("clearing {}", previous.display()))?;
    }
    if dest.exists() {
        std::fs::rename(dest, &previous)
            .with_context(|| format!("moving {} aside", dest.display()))?;
    }
    std::fs::rename(build, dest)
        .with_context(|| format!("moving {} into place", dest.display()))?;
    if previous.exists() {
        let _ = std::fs::remove_dir_all(&previous);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn import_csv(
    path: &Path,
    sidecar_path: Option<&Path>,
    mapping: &HashMap<String, String>,
    defaults: &HashMap<String, String>,
    schema: &Schema,
    field_map: &HashMap<String, tantivy::schema::Field>,
    field_configs: &[crate::config::FieldConfig],
    writer: &mut IndexWriter,
    metadata_collector: &mut ImportFieldMetadataCollector,
    mut store_import: Option<&mut StoreImport>,
    pb: &ProgressBar,
) -> anyhow::Result<u64> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_path(path)?;

    let headers: Vec<String> = rdr.headers()?.iter().map(|s| s.to_string()).collect();

    // Build reverse mapping: schema_field_name → csv_column_index
    let mut col_indices: HashMap<String, usize> = HashMap::new();
    for (schema_name, csv_col_name) in mapping {
        if let Some(idx) = headers.iter().position(|h| h == csv_col_name) {
            col_indices.insert(schema_name.clone(), idx);
        }
    }

    let ref_field = schema.get_field(REF_FIELD).ok();
    let prefix_fields: HashMap<String, tantivy::schema::Field> = field_configs
        .iter()
        .filter(|fc| fc.search == Some(crate::config::SearchMode::Fuzzy))
        .filter_map(|fc| {
            schema
                .get_field(&crate::schema::prefix_field_name(&fc.name))
                .ok()
                .map(|field| (fc.name.clone(), field))
        })
        .collect();
    let mut sidecar = match (&store_import, sidecar_path) {
        (Some(si), Some(sp)) if si.mode == StoreSource::Sidecar => Some(SidecarReader::open(sp)?),
        _ => None,
    };

    let mut count = 0u64;
    let mut bad_number_cells = 0u64;
    let mut record = csv::StringRecord::new();

    while rdr.read_record(&mut record)? {
        let mut doc = TantivyDocument::new();

        for fc in field_configs {
            let field = match field_map.get(&fc.name) {
                Some(f) => *f,
                None => continue,
            };

            // Try CSV column first, then defaults
            let value = col_indices
                .get(&fc.name)
                .and_then(|&idx| record.get(idx))
                .map(|s| s.to_string())
                .or_else(|| defaults.get(&fc.name).cloned())
                .unwrap_or_default();

            match fc.field_type {
                FieldType::Text | FieldType::Keyword | FieldType::Enum | FieldType::Boolean => {
                    // A multi field contributes one term per element, so a
                    // document can match a filter for any of them.
                    for part in fc.split_values(&value) {
                        if let Some(normalized) = canonicalize_stored_value(fc, part)? {
                            if fc.field_type == FieldType::Enum {
                                metadata_collector.observe(fc, &normalized);
                            }
                            doc.add_text(field, &normalized);
                            // Fuzzy fields feed their typeahead shadow field
                            // the same value; the edge_prefix tokenizer does
                            // the rest.
                            if let Some(&prefix_field) = prefix_fields.get(&fc.name) {
                                doc.add_text(prefix_field, &normalized);
                            }
                        }
                    }
                }
                FieldType::Number => {
                    // An empty or unparseable cell is a missing value, not a
                    // zero. Storing 0.0 for it (as this used to) made every
                    // missing revenue match revenue_max=10, sort as the
                    // smallest value, and equal a genuine zero.
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        match trimmed.parse::<f64>() {
                            Ok(num) if num.is_finite() => doc.add_f64(field, num),
                            _ => bad_number_cells += 1,
                        }
                    }
                }
            }
        }

        if let Some(store_import) = store_import.as_deref_mut() {
            let doc_ref = store_import.next_ref;
            if let Some(rf) = ref_field {
                doc.add_u64(rf, doc_ref);
            }
            let full = match store_import.mode {
                StoreSource::Row => build_row_doc(&headers, &record, defaults),
                StoreSource::Sidecar => sidecar
                    .as_mut()
                    .expect("sidecar reader opened for sidecar mode")
                    .next_line()?,
            };
            store_import.writer.add(full)?;
            store_import.next_ref += 1;
        }

        writer.add_document(doc)?;
        count += 1;

        if count.is_multiple_of(10_000) {
            pb.set_position(count);
        }
    }

    if let Some(sidecar) = sidecar.as_mut() {
        sidecar.expect_exhausted(count)?;
    }

    if bad_number_cells > 0 {
        eprintln!(
            "  note: {} skipped {} non-numeric cell(s) in numeric fields — those values are null, not 0",
            path.display(),
            bad_number_cells
        );
    }

    pb.set_position(count);
    Ok(count)
}

/// Row-mode full document: every CSV column under its original name, plus
/// source defaults (under their schema field names) where not already present.
fn build_row_doc(
    headers: &[String],
    record: &csv::StringRecord,
    defaults: &HashMap<String, String>,
) -> Vec<u8> {
    let mut obj = serde_json::Map::with_capacity(headers.len() + defaults.len());
    for (i, header) in headers.iter().enumerate() {
        let value = record.get(i).unwrap_or("");
        obj.insert(header.clone(), serde_json::Value::String(value.to_string()));
    }
    for (key, value) in defaults {
        if !obj.contains_key(key) {
            obj.insert(key.clone(), serde_json::Value::String(value.clone()));
        }
    }
    serde_json::to_vec(&serde_json::Value::Object(obj)).expect("string map serializes")
}

/// Reads an aligned JSONL sidecar in lockstep with CSV rows: line i is the
/// full document for row i, stored verbatim.
struct SidecarReader {
    reader: std::io::BufReader<std::fs::File>,
    path: std::path::PathBuf,
    line_no: u64,
    validated_first: bool,
}

impl SidecarReader {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("failed opening sidecar {}", path.display()))?;
        Ok(Self {
            reader: std::io::BufReader::with_capacity(256 * 1024, file),
            path: path.to_path_buf(),
            line_no: 0,
            validated_first: false,
        })
    }

    fn next_line(&mut self) -> anyhow::Result<Vec<u8>> {
        let mut line = Vec::new();
        let n = self.reader.read_until(b'\n', &mut line)?;
        self.line_no += 1;
        if n == 0 {
            bail!(
                "sidecar {} has fewer lines than its CSV has rows (ran out at line {})",
                self.path.display(),
                self.line_no
            );
        }
        while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            bail!(
                "sidecar {} line {} is empty — every CSV row needs a JSON document (use null for none)",
                self.path.display(),
                self.line_no
            );
        }
        // Cheap sanity check: validate the first line of each sidecar is JSON,
        // catching wrong-file mistakes without paying to parse every line.
        if !self.validated_first {
            serde_json::from_slice::<&serde_json::value::RawValue>(&line).with_context(|| {
                format!("sidecar {} line 1 is not valid JSON", self.path.display())
            })?;
            self.validated_first = true;
        }
        Ok(line)
    }

    fn expect_exhausted(&mut self, rows: u64) -> anyhow::Result<()> {
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut self.reader, &mut rest)?;
        if !rest.trim().is_empty() {
            bail!(
                "sidecar {} has more lines than its CSV has rows ({} rows consumed)",
                self.path.display(),
                rows
            );
        }
        Ok(())
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Register a trigram tokenizer for fuzzy text matching
fn register_trigram_tokenizer(index: &tantivy::Index) {
    register_trigram_tokenizer_pub(index);
}

pub fn register_trigram_tokenizer_pub(index: &tantivy::Index) {
    use tantivy::tokenizer::*;

    // Folding happens inside the tokenizers, before trigramming/prefixing —
    // see crate::analyze for why the order is load-bearing. Only affects
    // indexing; queries build terms directly and fold on their own side.
    index
        .tokenizers()
        .register("trigram", crate::analyze::FoldingTrigramTokenizer);
    index
        .tokenizers()
        .register("edge_prefix", crate::analyze::EdgePrefixTokenizer);

    // Whole value as one token, lowercased — keyword filters match any casing
    // while the stored value keeps its original form.
    let keyword_ci = TextAnalyzer::builder(RawTokenizer::default())
        .filter(LowerCaser)
        .build();
    index
        .tokenizers()
        .register(crate::schema::KEYWORD_CI_TOKENIZER, keyword_ci);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Config, DashboardConfig, FieldConfig, SchemaConfig, ServerConfig, SourceConfig, StoreConfig,
    };
    use crate::search::{SearchEngine, SortOrder, StoreStatus};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ruzz-import-{prefix}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn text_field(name: &str, fuzzy: bool) -> FieldConfig {
        FieldConfig {
            name: name.to_string(),
            field_type: FieldType::Text,
            search: fuzzy.then_some(crate::config::SearchMode::Fuzzy),
            values: None,
            max_values: None,
            case_sensitive: false,
            multi: false,
            separator: None,
            description: None,
        }
    }

    fn keyword_field(name: &str) -> FieldConfig {
        FieldConfig {
            name: name.to_string(),
            field_type: FieldType::Keyword,
            search: None,
            values: None,
            max_values: None,
            case_sensitive: false,
            multi: false,
            separator: None,
            description: None,
        }
    }

    fn test_config(dir: &Path, sources: Vec<SourceConfig>, store: StoreConfig) -> Arc<Config> {
        Arc::new(Config {
            server: ServerConfig {
                port: 0,
                index_path: dir.join("index"),
                bind: "0.0.0.0".to_string(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
            },
            schema: SchemaConfig {
                fields: vec![
                    text_field("name", true),
                    keyword_field("org_number"),
                    keyword_field("country_code"),
                ],
            },
            sources,
            mappings: HashMap::new(),
            store,
            dashboard: DashboardConfig::default(),
        })
    }

    fn write_csv(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    const CSV: &str = "\
org_number,company_name,city,secret_extra
100,Acme Rockets,Oslo,alpha
200,Beta Bakery,Bergen,bravo
300,Gamma Gruppen,Oslo,charlie
";

    fn source(dir: &Path, sidecar: Option<PathBuf>) -> SourceConfig {
        SourceConfig {
            path: dir.join("data.csv"),
            defaults: HashMap::from([("country_code".to_string(), "NO".to_string())]),
            mapping: HashMap::from([
                ("name".to_string(), "company_name".to_string()),
                ("org_number".to_string(), "org_number".to_string()),
            ]),
            use_mapping: None,
            sidecar,
        }
    }

    /// The index-v2 behaviors, through a real import: fuzzy text folds
    /// (diacritics, ligatures, NFC/NFD) and 1–2 character queries match
    /// word prefixes instead of nothing.
    #[test]
    fn folding_and_typeahead_work_end_to_end() {
        let dir = temp_dir("fold");
        write_csv(
            &dir.join("data.csv"),
            "org_number,company_name,city,secret_extra\n\
             100,Sørlandet Café AS,Kristiansand,a\n\
             200,Sæter Gård DA,Ås,b\n\
             300,Müller Bygg AS,Oslo,c\n\
             400,Berg Kraft AS,Bergen,d\n",
        );
        let config = test_config(&dir, vec![source(&dir, None)], StoreConfig::default());
        run_import(&config).unwrap();
        let engine = SearchEngine::open(config).unwrap();

        let count = |q: &str| {
            engine
                .search(
                    q,
                    &HashMap::new(),
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
                .count
                .unwrap()
        };

        // Diacritic folding, both directions, any casing.
        assert_eq!(count("sorlandet"), 1, "ø folds to o");
        assert_eq!(count("SØRLANDET"), 1, "folded query side too");
        assert_eq!(count("cafe"), 1, "é folds to e");
        assert_eq!(count("saeter"), 1, "æ expands to ae");
        assert_eq!(count("muller"), 1, "ü folds to u");
        // NFD input: "é" as e + combining acute folds like the precomposed.
        assert_eq!(count("cafe\u{0301}"), 1);

        // Typeahead: too short for a trigram, matched through word prefixes.
        assert_eq!(count("so"), 1, "prefix of sørlandet, folded");
        assert_eq!(count("mü"), 1, "prefix folds too");
        assert_eq!(count("b"), 2, "bygg and berg");
        assert_eq!(count("zz"), 0, "no word starts with zz");

        // A mixed query: trigrams for the long word, a prefix for the short
        // one — the doc matching both ranks first.
        let mixed = engine
            .search(
                "berg k",
                &HashMap::new(),
                &[],
                &SortOrder::Relevance,
                10,
                0,
                false,
                true,
                false,
            )
            .unwrap();
        assert_eq!(mixed.results[0]["name"], "Berg Kraft AS");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A failed import must not cost the previous index. It used to: the
    /// old index was deleted before the first row was read, so any import
    /// error — a renamed CSV, a full disk, ctrl-C — left nothing to serve.
    #[test]
    fn failed_import_leaves_the_previous_index_intact() {
        let dir = temp_dir("atomic");
        write_csv(&dir.join("data.csv"), CSV);
        let good = test_config(
            &dir,
            vec![source(&dir, None)],
            StoreConfig {
                enabled: true,
                ..StoreConfig::default()
            },
        );
        run_import(&good).unwrap();

        // Same config, but the source file is gone: the import fails.
        let mut broken_source = source(&dir, None);
        broken_source.path = dir.join("no-such-file.csv");
        let broken = test_config(
            &dir,
            vec![broken_source],
            StoreConfig {
                enabled: true,
                ..StoreConfig::default()
            },
        );
        run_import(&broken).expect_err("import of a missing file must fail");

        // The previous index and its store still serve, unharmed.
        let engine = SearchEngine::open(good.clone()).unwrap();
        assert_eq!(engine.store_status, StoreStatus::Ok);
        let result = engine
            .search(
                "acme",
                &HashMap::new(),
                &[],
                &SortOrder::Relevance,
                10,
                0,
                false,
                true,
                false,
            )
            .unwrap();
        assert_eq!(result.total, 1, "old data still searchable");

        // And the staging leftovers do not trip the next successful import.
        drop(engine);
        run_import(&good).unwrap();
        let engine = SearchEngine::open(good).unwrap();
        assert_eq!(engine.store_status, StoreStatus::Ok);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A numeric cell that is empty or unparseable used to be stored as 0.0,
    /// and a genuine 0 used to render as null. Missing must be null and must
    /// stay out of ranges and sorts; zero must be a first-class value.
    #[test]
    fn missing_numbers_are_null_and_zero_is_a_value() {
        let dir = temp_dir("numbers");
        write_csv(
            &dir.join("data.csv"),
            "org_number,company_name,revenue\n\
             1,Zero Corp,0\n\
             2,Missing Corp,\n\
             3,Garbage Corp,abc\n\
             4,Rich Corp,500\n\
             5,Poor Corp,10\n",
        );
        let config = Arc::new(Config {
            server: ServerConfig {
                port: 0,
                index_path: dir.join("index"),
                bind: "0.0.0.0".to_string(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
            },
            schema: SchemaConfig {
                fields: vec![
                    keyword_field("org_number"),
                    FieldConfig {
                        name: "revenue".to_string(),
                        field_type: FieldType::Number,
                        search: None,
                        values: None,
                        max_values: None,
                        case_sensitive: false,
                        multi: false,
                        separator: None,
                        description: None,
                    },
                ],
            },
            sources: vec![SourceConfig {
                path: dir.join("data.csv"),
                defaults: HashMap::new(),
                mapping: HashMap::from([
                    ("org_number".to_string(), "org_number".to_string()),
                    ("revenue".to_string(), "revenue".to_string()),
                ]),
                use_mapping: None,
                sidecar: None,
            }],
            mappings: HashMap::new(),
            store: StoreConfig::default(),
            dashboard: DashboardConfig::default(),
        });

        run_import(&config).unwrap();
        let engine = SearchEngine::open(config).unwrap();
        let search =
            |filters: &[(&str, &str)], ranges: &[crate::search::RangeFilter], sort: SortOrder| {
                let filters: HashMap<String, String> = filters
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                engine
                    .search("", &filters, ranges, &sort, 10, 0, false, true, false)
                    .unwrap()
            };

        // A stored zero is a value, a missing or garbage cell is null.
        let by_org = |org: &str| {
            let result = search(&[("org_number", org)], &[], SortOrder::Relevance);
            result.results[0]["revenue"].clone()
        };
        assert_eq!(by_org("1"), serde_json::json!(0.0), "zero is not null");
        assert_eq!(by_org("2"), serde_json::Value::Null, "missing is null");
        assert_eq!(by_org("3"), serde_json::Value::Null, "garbage is null");

        // Ranges exclude missing values instead of treating them as 0.
        let ranged = search(
            &[],
            &[crate::search::RangeFilter {
                field: "revenue".to_string(),
                min: Some(0.0),
                max: None,
            }],
            SortOrder::Relevance,
        );
        assert_eq!(ranged.count, Some(3), "0, 10 and 500 — not missing rows");

        // An exact zero filter matches only the true zero.
        let zero = search(&[("revenue", "0")], &[], SortOrder::Relevance);
        assert_eq!(zero.count, Some(1));
        assert_eq!(zero.results[0]["org_number"], "1");

        // Sorting puts value-less docs last in both directions.
        let revenues = |sort: SortOrder| -> Vec<serde_json::Value> {
            search(&[], &[], sort)
                .results
                .iter()
                .map(|r| r["revenue"].clone())
                .collect()
        };
        assert_eq!(
            revenues(SortOrder::FieldAsc("revenue".to_string())),
            vec![
                serde_json::json!(0.0),
                serde_json::json!(10.0),
                serde_json::json!(500.0),
                serde_json::Value::Null,
                serde_json::Value::Null
            ]
        );
        assert_eq!(
            revenues(SortOrder::FieldDesc("revenue".to_string())),
            vec![
                serde_json::json!(500.0),
                serde_json::json!(10.0),
                serde_json::json!(0.0),
                serde_json::Value::Null,
                serde_json::Value::Null
            ]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn row_mode_end_to_end() {
        let dir = temp_dir("row");
        write_csv(&dir.join("data.csv"), CSV);
        let config = test_config(
            &dir,
            vec![source(&dir, None)],
            StoreConfig {
                enabled: true,
                ..StoreConfig::default()
            },
        );

        let stats = run_import(&config).unwrap();
        assert_eq!(stats.total_rows, 3);

        let engine = SearchEngine::open(config).unwrap();
        assert_eq!(engine.store_status, StoreStatus::Ok);

        // Fuzzy search carries _ref; full=true hydrates with unmapped columns
        let result = engine
            .search(
                "acme",
                &HashMap::new(),
                &[],
                &SortOrder::Relevance,
                10,
                0,
                false,
                true,
                true,
            )
            .unwrap();
        assert_eq!(result.total, 1);
        let hit = &result.results[0];
        assert_eq!(hit["name"], "Acme Rockets");
        let doc_ref = hit["_ref"].as_u64().expect("_ref present");
        let full = &hit["_full"];
        // Row mode preserves unmapped CSV columns and applies source defaults
        assert_eq!(full["secret_extra"], "alpha");
        assert_eq!(full["city"], "Oslo");
        assert_eq!(full["company_name"], "Acme Rockets");
        assert_eq!(full["country_code"], "NO");

        // Direct fetch by ref matches
        let raw = engine.get_full(doc_ref).unwrap().expect("doc in store");
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["secret_extra"], "alpha");

        // Out-of-range ref is None
        assert!(engine.get_full(999).unwrap().is_none());

        // Exact-match resolve → full doc
        let filters = HashMap::from([
            ("org_number".to_string(), "200".to_string()),
            ("country_code".to_string(), "NO".to_string()),
        ]);
        let (matched, found) = engine.resolve_full(&filters).unwrap();
        assert_eq!(matched, 1);
        let (_, full) = found.expect("resolved");
        let parsed: serde_json::Value = serde_json::from_str(&full).unwrap();
        assert_eq!(parsed["company_name"], "Beta Bakery");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sidecar_mode_stores_verbatim_nested_docs() {
        let dir = temp_dir("sidecar");
        write_csv(&dir.join("data.csv"), CSV);
        let sidecar_path = dir.join("full.jsonl");
        std::fs::write(
            &sidecar_path,
            "{\"org\":100,\"accounts\":[{\"year\":2024,\"revenue\":50}]}\n\
             {\"org\":200,\"accounts\":[]}\n\
             null\n",
        )
        .unwrap();

        let config = test_config(
            &dir,
            vec![source(&dir, Some(sidecar_path))],
            StoreConfig {
                enabled: true,
                source: crate::config::StoreSource::Sidecar,
                ..StoreConfig::default()
            },
        );

        run_import(&config).unwrap();
        let engine = SearchEngine::open(config).unwrap();
        assert_eq!(engine.store_status, StoreStatus::Ok);

        // Verbatim bytes, nested structure intact
        let raw = engine.get_full(0).unwrap().unwrap();
        assert_eq!(
            raw,
            "{\"org\":100,\"accounts\":[{\"year\":2024,\"revenue\":50}]}"
        );
        // null is a valid "no document" marker
        assert_eq!(engine.get_full(2).unwrap().unwrap(), "null");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sidecar_line_count_mismatch_fails_import() {
        let dir = temp_dir("sidecar-short");
        write_csv(&dir.join("data.csv"), CSV);
        let sidecar_path = dir.join("full.jsonl");
        std::fs::write(&sidecar_path, "{\"org\":100}\n{\"org\":200}\n").unwrap(); // 2 lines, 3 rows

        let config = test_config(
            &dir,
            vec![source(&dir, Some(sidecar_path.clone()))],
            StoreConfig {
                enabled: true,
                source: crate::config::StoreSource::Sidecar,
                ..StoreConfig::default()
            },
        );
        let err = run_import(&config).unwrap_err().to_string();
        assert!(err.contains("fewer lines"), "got: {err}");

        // Too many lines fails too
        std::fs::write(
            &sidecar_path,
            "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n{\"a\":4}\n",
        )
        .unwrap();
        let config = test_config(
            &dir,
            vec![source(&dir, Some(sidecar_path))],
            StoreConfig {
                enabled: true,
                source: crate::config::StoreSource::Sidecar,
                ..StoreConfig::default()
            },
        );
        let err = run_import(&config).unwrap_err().to_string();
        assert!(err.contains("more lines"), "got: {err}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn generation_mismatch_disables_store_but_not_search() {
        let dir = temp_dir("genmismatch");
        write_csv(&dir.join("data.csv"), CSV);
        let config = test_config(
            &dir,
            vec![source(&dir, None)],
            StoreConfig {
                enabled: true,
                ..StoreConfig::default()
            },
        );
        run_import(&config).unwrap();

        // Tamper with the pairing marker
        crate::store::write_index_pairing(&config.server.index_path, "bogus-generation").unwrap();

        let engine = SearchEngine::open(config).unwrap();
        assert!(matches!(engine.store_status, StoreStatus::Error(_)));
        assert!(engine.store.is_none());

        // Search still works, hits still carry _ref (it's in the index)
        let result = engine
            .search(
                "bakery",
                &HashMap::new(),
                &[],
                &SortOrder::Relevance,
                10,
                0,
                false,
                true,
                false,
            )
            .unwrap();
        assert_eq!(result.total, 1);

        // But full access errors
        assert!(engine.get_full(0).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn store_disabled_leaves_results_clean_and_removes_stale_store() {
        let dir = temp_dir("disabled");
        write_csv(&dir.join("data.csv"), CSV);

        // First import WITH store
        let config = test_config(
            &dir,
            vec![source(&dir, None)],
            StoreConfig {
                enabled: true,
                ..StoreConfig::default()
            },
        );
        run_import(&config).unwrap();
        let store_path = config.store_path();
        assert!(store_path.join(crate::store::META_FILE).exists());

        // Re-import WITHOUT store: stale store dir must be cleaned up
        let config = test_config(&dir, vec![source(&dir, None)], StoreConfig::default());
        run_import(&config).unwrap();
        assert!(!store_path.exists(), "stale store should be removed");

        let engine = SearchEngine::open(config).unwrap();
        assert_eq!(engine.store_status, StoreStatus::Disabled);
        let result = engine
            .search(
                "acme",
                &HashMap::new(),
                &[],
                &SortOrder::Relevance,
                10,
                0,
                false,
                true,
                false,
            )
            .unwrap();
        assert_eq!(result.total, 1);
        assert!(result.results[0].get("_ref").is_none());
        assert!(result.results[0].get("_full").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }
}
