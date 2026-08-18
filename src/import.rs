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
    const UNITS: [&str; 4] = ["B", "MB", "GB", "TB"];
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

    // Create or open index
    let index_path = &config.server.index_path;
    if index_path.exists() {
        std::fs::remove_dir_all(index_path)?;
    }
    std::fs::create_dir_all(index_path)?;

    // Prepare the store directory (or clear a stale store from a previous
    // store-enabled import so it can't outlive the index it belonged to).
    let mut store_import = if store_enabled {
        if store_path.exists() {
            std::fs::remove_dir_all(&store_path)?;
        }
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
        // Clear both locations: a store left behind at either path would
        // otherwise outlive the index it was built for, and the legacy one
        // would still be picked up when serving.
        for stale in [&store_path, &legacy_path] {
            if store::looks_like_store_dir(stale) {
                std::fs::remove_dir_all(stale)?;
                println!("  removed stale document store at {}", stale.display());
            }
        }
        None
    };

    let index = tantivy::Index::create_in_dir(index_path, schema.clone())?;

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

    write_stored_field_metadata(&config.server.index_path, &metadata_collector.into_stored())?;

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
        store::write_index_pairing(&config.server.index_path, &generation)?;
        println!(
            "✓ document store: {} docs, {} raw → {} on disk ({:.1}x) → {}",
            outcome.doc_count,
            format_bytes(outcome.raw_bytes),
            format_bytes(outcome.compressed_bytes),
            outcome.raw_bytes.max(1) as f64 / outcome.compressed_bytes.max(1) as f64,
            store_path.display()
        );
    }

    stats.total_duration_secs = start.elapsed().as_secs_f64();

    println!(
        "\n✓ {} rows indexed in {:.1}s → {}",
        stats.total_rows,
        stats.total_duration_secs,
        config.server.index_path.display()
    );

    Ok(stats)
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
    let mut sidecar = match (&store_import, sidecar_path) {
        (Some(si), Some(sp)) if si.mode == StoreSource::Sidecar => Some(SidecarReader::open(sp)?),
        _ => None,
    };

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
                        }
                    }
                }
                FieldType::Number => {
                    // Parse to f64, store 0.0 for empty/invalid
                    let num = value.parse::<f64>().unwrap_or(0.0);
                    doc.add_f64(field, num);
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

    let tokenizer = TextAnalyzer::builder(NgramTokenizer::new(3, 3, false).unwrap())
        .filter(LowerCaser)
        .build();

    index.tokenizers().register("trigram", tokenizer);

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
        Config, FieldConfig, SchemaConfig, ServerConfig, SourceConfig, StoreConfig,
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
