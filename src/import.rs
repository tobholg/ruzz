use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tantivy::schema::Schema;
use tantivy::{IndexWriter, TantivyDocument};

use crate::config::{Config, FieldType, SourceFormat, StoreSource};
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
    logged(config, "gc", &[], |_event| collect_garbage_inner(config))
}

fn collect_garbage_inner(config: &Config) -> anyhow::Result<()> {
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

/// Merge every segment into one, in place. What a full import does at its
/// end — repeated here for an index whose merge failed (or one grown to
/// many segments by deltas). A running server keeps serving throughout and
/// picks the merged segment up through its reader; the superseded segment
/// files are collected afterwards. Needs roughly one index of free disk.
pub fn merge_segments(config: &Config) -> anyhow::Result<()> {
    logged(config, "merge", &[], |_event| merge_segments_inner(config))
}

fn merge_segments_inner(config: &Config) -> anyhow::Result<()> {
    let index_path = &config.server.index_path;
    let index = tantivy::Index::open_in_dir(index_path)
        .with_context(|| format!("opening index at {}", index_path.display()))?;
    register_trigram_tokenizer_pub(&index);
    let segment_ids = index.searchable_segment_ids()?;
    if segment_ids.len() < 2 {
        println!("✓ index already has a single segment — nothing to merge");
        return Ok(());
    }
    let before = dir_bytes(index_path);
    println!(
        "  merging {} segments ({} docs) — this rewrites the index once; the server keeps serving",
        segment_ids.len(),
        index.reader()?.searcher().num_docs()
    );
    let start = Instant::now();
    merge_all(&index, &segment_ids)?;
    let collector: IndexWriter = index.writer(15_000_000)?;
    let outcome = collector
        .garbage_collect_files()
        .wait()
        .map_err(|e| anyhow::anyhow!("garbage collection failed: {}", e))?;
    let after = dir_bytes(index_path);
    println!(
        "✓ merged into one segment in {:.0}s; removed {} superseded files, {} → {} on disk",
        start.elapsed().as_secs_f64(),
        outcome.deleted_files.len(),
        human_bytes(before),
        human_bytes(after),
    );
    println!("  a running server picks the merged segment up within a moment — no restart needed");
    Ok(())
}

/// Merge the given segments into one, with a writer whose merge policy is
/// disabled so nothing else touches the segment set meanwhile. Returns
/// once the merged segment is committed to meta.json.
fn merge_all(index: &tantivy::Index, segment_ids: &[tantivy::SegmentId]) -> anyhow::Result<()> {
    let mut merger: IndexWriter = index.writer(50_000_000)?;
    merger.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
    merger
        .merge(segment_ids)
        .wait()
        .map_err(|e| anyhow::anyhow!("merge failed: {}", e))?;
    merger.wait_merging_threads()?;
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

/// Everything the per-source import loop needs to feed the document store.
struct StoreImport {
    writer: store::StoreWriter,
    mode: StoreSource,
    next_ref: u64,
}

/// Run `op`, then append an activity event describing what happened —
/// success or failure — before returning the result. The closure fills the
/// event's counters on success; timing and error capture live here so every
/// operation reports the same way.
fn logged<T>(
    config: &Config,
    op: &str,
    sources: &[&Path],
    body: impl FnOnce(&mut crate::activity::ActivityEvent) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let start = Instant::now();
    let mut event = crate::activity::ActivityEvent::new(op);
    event.sources = sources
        .iter()
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();
    let result = body(&mut event);
    event.duration_ms = start.elapsed().as_millis() as u64;
    if let Err(e) = &result {
        event = event.failed(e);
    }
    crate::activity::log(config, &event);
    result
}

pub fn run_import(config: &Config) -> anyhow::Result<ImportStats> {
    let sources: Vec<&Path> = config.sources.iter().map(|s| s.path.as_path()).collect();
    logged(config, "import", &sources, |event| {
        let stats = run_import_inner(config)?;
        event.rows = stats.total_rows;
        Ok(stats)
    })
}

#[derive(Debug, Default)]
pub struct CheckReport {
    pub rows: u64,
    /// Rows the import would reject outright (e.g. an invalid boolean).
    pub bad_rows: u64,
    pub bad_number_cells: u64,
    pub empty_keys: u64,
    pub duplicate_keys: u64,
    /// Human-readable findings, worst first-ish.
    pub problems: Vec<String>,
}

impl CheckReport {
    /// Nothing to fix before importing. Duplicate and empty primary keys
    /// count as problems: a full import keeps every row while an update by
    /// key collapses them, so the two paths would disagree on the data.
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty() && self.bad_rows == 0
    }
}

/// Dry-run every configured source: parse all rows, exercise the same
/// canonicalization the import would, and report what it finds — without
/// writing a byte. This is also where duplicate primary keys get caught:
/// a full import deliberately does not pay a per-row dedup at 20M docs,
/// so the check is the tool that answers "is my source unique by key?".
pub fn run_check(config: &Config) -> anyhow::Result<CheckReport> {
    const MAX_ROW_PROBLEMS: usize = 5;

    let (schema, field_map) = build_schema(&config.schema, config.store.enabled);
    let pk = config.schema.primary_key.as_ref().map(|name| {
        let fc = config
            .schema
            .fields
            .iter()
            .find(|f| &f.name == name)
            .expect("validated by config");
        (name.clone(), !fc.case_sensitive)
    });

    let mut report = CheckReport::default();
    // Key hashes, not keys: bounds memory at 8 bytes per row. A hash
    // collision would report one duplicate too many — irrelevant at the
    // rates this reports on.
    let mut seen_keys: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut metadata_collector = ImportFieldMetadataCollector::new(&config.schema.fields);

    for source in &config.sources {
        let label = source.path.display();
        let mapping = source.resolved_mapping(&config.mappings);
        let format = source.resolved_format();
        let (mut reader, headers, _bytes) = RecordReader::open(&source.path, format)?;

        if format == SourceFormat::Csv {
            for (field, column) in &mapping {
                if !headers.iter().any(|h| h == column) {
                    report.problems.push(format!(
                        "{}: mapping {} = \"{}\" names a column the file does not have — the field will be empty",
                        label, field, column
                    ));
                }
            }
        }

        let mut sidecar = match &source.sidecar {
            Some(path) if config.store.enabled && config.store.source == StoreSource::Sidecar => {
                Some(SidecarReader::open(path)?)
            }
            _ => None,
        };

        let mut indexer = RowIndexer::new(
            format,
            headers,
            &mapping,
            &source.defaults,
            &schema,
            &field_map,
            &config.schema.fields,
        );
        let mut rows = 0u64;
        let mut row_problems = 0usize;
        loop {
            let record = match reader.next() {
                Ok(Some(record)) => record,
                Ok(None) => break,
                Err(e) => {
                    // A stream-level parse error (bad JSON line, broken CSV)
                    // is where the import would die too.
                    report.bad_rows += 1;
                    report.problems.push(format!("{}: {:#}", label, e));
                    break;
                }
            };
            rows += 1;

            if let Some((pk_name, fold)) = &pk {
                let key = indexer.raw_value(record, pk_name).unwrap_or_default();
                let key = key.trim();
                if key.is_empty() {
                    report.empty_keys += 1;
                } else {
                    let folded = if *fold {
                        key.to_lowercase()
                    } else {
                        key.to_string()
                    };
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    std::hash::Hash::hash(&folded, &mut hasher);
                    if !seen_keys.insert(std::hash::Hasher::finish(&hasher)) {
                        report.duplicate_keys += 1;
                    }
                }
            }

            match indexer.build_doc(record, &mut metadata_collector, None, None) {
                Ok(_) => {}
                Err(e) => {
                    report.bad_rows += 1;
                    row_problems += 1;
                    if row_problems <= MAX_ROW_PROBLEMS {
                        report
                            .problems
                            .push(format!("{}: row {}: {:#}", label, rows, e));
                    }
                }
            }
            let mut sidecar_dead = false;
            if let Some(sidecar) = sidecar.as_mut() {
                if let Err(e) = sidecar.next_line() {
                    report.problems.push(format!("{:#}", e));
                    sidecar_dead = true;
                }
            }
            if sidecar_dead {
                sidecar = None; // one alignment finding per sidecar is enough
            }
        }
        if row_problems > MAX_ROW_PROBLEMS {
            report.problems.push(format!(
                "{}: …and {} more rejected row(s)",
                label,
                row_problems - MAX_ROW_PROBLEMS
            ));
        }
        if let Some(sidecar) = sidecar.as_mut() {
            if let Err(e) = sidecar.expect_exhausted(rows) {
                report.problems.push(format!("{:#}", e));
            }
        }
        report.rows += rows;
        report.bad_number_cells += indexer.bad_number_cells;
        report.problems.extend(indexer.array_problems(&source.path));
        if indexer.bad_number_cells > 0 {
            eprintln!(
                "  note: {} skipped {} non-numeric cell(s) in numeric fields — those values are null, not 0",
                source.path.display(),
                indexer.bad_number_cells
            );
        }
    }

    println!(
        "✓ checked {} row(s) across {} source(s)",
        report.rows,
        config.sources.len()
    );
    if let Some((pk_name, _)) = &pk {
        println!(
            "  primary key '{}': {} empty, {} duplicate",
            pk_name, report.empty_keys, report.duplicate_keys
        );
        // Counted above, but a verdict that ignored them said "no problems
        // found" over 3.1M duplicates in a 5.7M-row source. A rate that
        // high means the key does not identify a row in that source.
        if report.duplicate_keys > 0 {
            let rate = report.duplicate_keys as f64 / report.rows.max(1) as f64;
            report.problems.push(format!(
                "primary key '{}': {} duplicate value(s) ({:.1}% of rows) — a full import keeps \
                 every row, an update by key collapses them{}",
                pk_name,
                report.duplicate_keys,
                rate * 100.0,
                if rate >= 0.05 {
                    "; at this rate the key does not identify a row in this source"
                } else {
                    ""
                }
            ));
        }
        if report.empty_keys > 0 {
            report.problems.push(format!(
                "primary key '{}': {} row(s) with an empty value — a full import indexes them, \
                 an update skips them",
                pk_name, report.empty_keys
            ));
        }
    }
    if report.is_clean() {
        println!("  no problems found");
    }
    for problem in &report.problems {
        println!("  ⚠ {}", problem);
    }
    Ok(report)
}

fn run_import_inner(config: &Config) -> anyhow::Result<ImportStats> {
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
        "{prefix:<30} {bar:30.cyan/dim} {bytes:>10}/{total_bytes:10} {bytes_per_sec:>12} ETA {eta}",
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

        // Progress by bytes: the file size is free, where counting lines
        // meant reading every CSV twice.
        let total_bytes = std::fs::metadata(&source.path)
            .map(|m| m.len())
            .unwrap_or(0);
        let pb = multi.add(ProgressBar::new(total_bytes));
        pb.set_style(style.clone());
        pb.set_prefix(file_name.clone());

        let rows = import_source(
            &source.path,
            source.resolved_format(),
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

    // Merge segments for faster queries. The store writer thread keeps
    // compressing in parallel with this.
    let merge_pb = multi.add(ProgressBar::new_spinner());
    merge_pb.set_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap());
    merge_pb.set_message("Merging segments (this improves query speed)...");
    // The commit hands the merge policy's background merges to the thread
    // pool; a forced merge issued on top of them used to race them — its
    // segment ids vanished under it ("could not be found in the
    // SegmentManager") and it failed, with the result dropped unread. The
    // index then kept whatever the policy's merges produced: eight
    // segments on a 62M-doc import, every query paying per segment. So:
    // drain the policy's merges first, then merge what is left with a
    // writer whose policy cannot interfere.
    writer.wait_merging_threads()?;
    let segment_ids = index.searchable_segment_ids()?;
    println!("  segments after indexing: {}", segment_ids.len());
    if segment_ids.len() > 1 {
        match merge_all(&index, &segment_ids) {
            Ok(()) => println!("  merged {} segments into one", segment_ids.len()),
            Err(e) => eprintln!(
                "  ⚠ final merge failed: {} — the index serves from {} segments, slower than one; \
                 run `ruzz merge` to retry in place",
                e,
                segment_ids.len()
            ),
        }

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
        store::write_index_pairing(&index_path, &generation, outcome.doc_count)?;
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

#[derive(Debug)]
pub struct UpdateStats {
    pub rows: u64,
    pub skipped_no_key: u64,
    pub duration_secs: f64,
}

/// Everything an incremental operation needs from the existing index, checked
/// for compatibility with the current config before anything is written.
struct OpenedIndex {
    index: tantivy::Index,
    schema: Schema,
    field_map: HashMap<String, tantivy::schema::Field>,
    pk_field: tantivy::schema::Field,
    /// The primary key's indexed terms are lowercase; fold lookup values the
    /// same way (mirrors how search folds filters for this field).
    fold_pk: bool,
}

/// Open the live index for an incremental update/delete and verify the parts
/// delete-by-term depends on: a configured primary key, an index built with
/// exactly this schema, and agreement on how the key's terms were folded.
fn open_for_incremental(config: &Config) -> anyhow::Result<(OpenedIndex, String)> {
    let pk_name = config.schema.primary_key.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "incremental updates need a primary key — set primary_key = \"<field>\" under [schema]"
        )
    })?;
    let pk_config = config
        .schema
        .fields
        .iter()
        .find(|fc| fc.name == pk_name)
        .expect("validated by config");

    let index_path = &config.server.index_path;
    if !index_path.join("meta.json").exists() {
        bail!(
            "no index at {} — run a full import first",
            index_path.display()
        );
    }
    let index = tantivy::Index::open_in_dir(index_path)
        .with_context(|| format!("opening index at {}", index_path.display()))?;
    register_trigram_tokenizer_pub(&index);

    let (schema, field_map) = build_schema(&config.schema, config.store.enabled);
    if index.schema() != schema {
        bail!(
            "the index at {} was built with a different schema — run a full import",
            index_path.display()
        );
    }

    // The schema check above guarantees an index-v2 layout (prefix fields and
    // all), but term folding for the key is recorded per index, not per
    // schema — disagreement would make deletes silently miss.
    let stored = crate::field_meta::load_stored_field_metadata(index_path)?;
    let index_folds_pk = stored.case_insensitive_fields.contains(&pk_name);
    if index_folds_pk == pk_config.case_sensitive {
        bail!(
            "the index disagrees with the config on case-sensitivity of primary key '{}' — run a full import",
            pk_name
        );
    }

    let pk_field = *field_map.get(&pk_name).expect("pk is a schema field");
    Ok((
        OpenedIndex {
            index,
            schema,
            field_map,
            pk_field,
            fold_pk: index_folds_pk,
        },
        pk_name,
    ))
}

/// The indexed term for a primary key value, folded the way this index
/// indexed it.
fn pk_term(opened: &OpenedIndex, value: &str) -> tantivy::Term {
    let trimmed = value.trim();
    let folded = if opened.fold_pk {
        trimmed.to_lowercase()
    } else {
        trimmed.to_string()
    };
    tantivy::Term::from_field_text(opened.pk_field, &folded)
}

/// Upsert rows from delta CSV files into the live index: for each row, any
/// existing document with the same primary key is deleted and the row is
/// added, in one commit. The document store (when enabled) is append-only —
/// superseded versions stay on disk as unreachable refs until the next full
/// import rewrites the store.
///
/// Unlike a full import there is no staging swap: tantivy segments are
/// immutable and the store only grows, so a running server keeps serving a
/// consistent view throughout and picks the commit up through its own reader.
pub fn run_update(
    config: &Config,
    files: &[std::path::PathBuf],
    like: Option<&Path>,
    sidecar: Option<&Path>,
    format: Option<SourceFormat>,
) -> anyhow::Result<UpdateStats> {
    let sources: Vec<&Path> = files.iter().map(|p| p.as_path()).collect();
    logged(config, "update", &sources, |event| {
        let stats = run_update_inner(config, files, like, sidecar, format)?;
        event.rows = stats.rows;
        event.skipped = stats.skipped_no_key;
        Ok(stats)
    })
}

/// With several sources configured and no --like, a delta usually matches
/// exactly one source's mapping — every mapped column (or top-level JSON
/// key) present in the delta. Anything ambiguous keeps requiring the flag:
/// guessing between mappings is how rows land in the wrong fields.
fn infer_template<'a>(
    config: &'a Config,
    first: &Path,
) -> anyhow::Result<&'a crate::config::SourceConfig> {
    let ask = || {
        anyhow::anyhow!(
            "config has {} sources — pass --like <source path> to say which mapping the delta uses",
            config.sources.len()
        )
    };
    if first == Path::new("-") {
        return Err(ask()); // stdin cannot be peeked and then re-read
    }
    let format = crate::config::detect_format(first);
    let (mut reader, headers, _bytes) = RecordReader::open(first, format)?;
    let observed: std::collections::HashSet<String> = match format {
        SourceFormat::Csv => headers.into_iter().collect(),
        SourceFormat::Jsonl => match reader.next()? {
            Some(RecordRef::Json(serde_json::Value::Object(obj))) => obj.keys().cloned().collect(),
            _ => Default::default(),
        },
    };

    let candidates: Vec<&crate::config::SourceConfig> = config
        .sources
        .iter()
        .filter(|source| {
            let mapping = source.resolved_mapping(&config.mappings);
            !mapping.is_empty()
                && mapping.values().all(|col| {
                    // For a JSONL delta only the top-level key is observable.
                    let key = col.split('.').next().unwrap_or(col);
                    observed.contains(key)
                })
        })
        .collect();

    match candidates.as_slice() {
        [] => Err(ask()),
        [one] => {
            println!(
                "  delta matches the mapping of {} — using its mapping and defaults (pass --like to override)",
                one.path.display()
            );
            Ok(one)
        }
        [head, rest @ ..] => {
            // Several matches are still unambiguous when they would behave
            // identically.
            let same = rest.iter().all(|c| {
                c.resolved_mapping(&config.mappings) == head.resolved_mapping(&config.mappings)
                    && c.defaults == head.defaults
            });
            if same {
                Ok(head)
            } else {
                Err(ask())
            }
        }
    }
}

fn run_update_inner(
    config: &Config,
    files: &[std::path::PathBuf],
    like: Option<&Path>,
    sidecar: Option<&Path>,
    format_override: Option<SourceFormat>,
) -> anyhow::Result<UpdateStats> {
    if files.is_empty() {
        bail!("no delta files given");
    }
    let (opened, pk_name) = open_for_incremental(config)?;

    // Which configured source's mapping and defaults the delta rows follow.
    let template = match like {
        Some(like) => config
            .sources
            .iter()
            .find(|s| s.path == like)
            .or_else(|| {
                config
                    .sources
                    .iter()
                    .find(|s| s.path.file_name() == like.file_name())
            })
            .ok_or_else(|| {
                anyhow::anyhow!("--like {} matches no [[sources]] entry", like.display())
            })?,
        None => match config.sources.as_slice() {
            [only] => only,
            _ => infer_template(config, &files[0])?,
        },
    };
    let mapping = template.resolved_mapping(&config.mappings);

    let mut store_import = if config.store.enabled {
        if config.store.source == StoreSource::Sidecar {
            if sidecar.is_none() {
                bail!("[store] source = \"sidecar\" — pass --sidecar <file.jsonl> for the delta");
            }
            if files.len() != 1 {
                bail!("--sidecar pairs with exactly one delta file");
            }
        }
        let store_path = config.resolve_store_path();
        let store_meta = store::read_meta(&store_path)
            .with_context(|| "no document store to append to — run a full import")?;
        let pairing = store::read_index_pairing(&config.server.index_path).ok_or_else(|| {
            anyhow::anyhow!("index has no store pairing file — re-run import with [store] enabled")
        })?;
        if pairing.generation != store_meta.generation {
            bail!(
                "store generation {} does not match index generation {} — re-run import",
                store_meta.generation,
                pairing.generation
            );
        }
        let block_size = store::parse_size(&config.store.block_size)
            .with_context(|| format!("invalid store block_size '{}'", config.store.block_size))?;
        let writer =
            store::StoreWriter::append_to(&store_path, block_size, config.store.compression_level)?;
        let next_ref = writer.next_ref_start();
        Some(StoreImport {
            writer,
            mode: config.store.source,
            next_ref,
        })
    } else {
        None
    };

    let mut writer: IndexWriter = opened.index.writer(256_000_000)?;
    let existing_metadata =
        crate::field_meta::load_stored_field_metadata(&config.server.index_path)?;
    let mut metadata_collector =
        ImportFieldMetadataCollector::seeded(&config.schema.fields, &existing_metadata);

    let start = Instant::now();
    let mut stats = UpdateStats {
        rows: 0,
        skipped_no_key: 0,
        duration_secs: 0.0,
    };

    for path in files {
        // The delta's own extension decides its encoding; the template only
        // lends its mapping and defaults. A JSONL delta against a CSV source
        // works when its keys match the mapping's column names. Stdin has no
        // extension, so it follows the template unless --format says.
        let format = format_override.unwrap_or_else(|| {
            if path == Path::new("-") {
                template.resolved_format()
            } else {
                crate::config::detect_format(path)
            }
        });
        let (mut reader, headers, _bytes) = RecordReader::open(path, format)?;
        let mut indexer = RowIndexer::new(
            format,
            headers,
            &mapping,
            &template.defaults,
            &opened.schema,
            &opened.field_map,
            &config.schema.fields,
        );
        let mut sidecar_reader = match (&store_import, sidecar) {
            (Some(si), Some(sp)) if si.mode == StoreSource::Sidecar => {
                Some(SidecarReader::open(sp)?)
            }
            _ => None,
        };

        let mut count = 0u64;
        while let Some(record) = reader.next()? {
            let key = indexer.raw_value(record, &pk_name).unwrap_or_default();
            if key.trim().is_empty() {
                stats.skipped_no_key += 1;
                continue;
            }
            writer.delete_term(pk_term(&opened, &key));
            let doc = indexer.build_doc(
                record,
                &mut metadata_collector,
                store_import.as_mut(),
                sidecar_reader.as_mut(),
            )?;
            writer.add_document(doc)?;
            count += 1;
        }
        if let Some(sidecar_reader) = sidecar_reader.as_mut() {
            sidecar_reader.expect_exhausted(count)?;
        }
        indexer.report_bad_number_cells(path);
        stats.rows += count;
    }

    // Store first, index commit last: a crash in between leaves appended
    // store docs nothing references — wasted bytes, never a dangling ref.
    if let Some(store_import) = store_import.take() {
        let outcome = store_import.writer.finish()?;
        let store_path = config.resolve_store_path();
        let mut meta = store::read_meta(&store_path)?;
        meta.doc_count = outcome.doc_count;
        meta.block_count = outcome.block_count;
        meta.raw_bytes = outcome.raw_bytes;
        meta.compressed_bytes = outcome.compressed_bytes;
        store::write_meta(&store_path, &meta)?;
        store::write_index_pairing(&config.server.index_path, &meta.generation, meta.doc_count)?;
    }
    writer.commit()?;
    write_stored_field_metadata(&config.server.index_path, &metadata_collector.into_stored())?;

    // Let the merge policy fold small delta segments in, then drop whatever
    // files that superseded. Skipping the forced full merge keeps small
    // updates cheap on a large index.
    writer.wait_merging_threads()?;
    let collector: IndexWriter = opened.index.writer(15_000_000)?;
    if let Err(e) = collector.garbage_collect_files().wait() {
        eprintln!("  note: could not collect superseded segments: {}", e);
    }

    stats.duration_secs = start.elapsed().as_secs_f64();
    if stats.skipped_no_key > 0 {
        eprintln!(
            "  note: skipped {} row(s) with an empty '{}' — a delta row needs a key to upsert by",
            stats.skipped_no_key, pk_name
        );
    }
    println!(
        "✓ {} row(s) upserted in {:.1}s → {}",
        stats.rows,
        stats.duration_secs,
        config.server.index_path.display()
    );
    println!("  a running server picks the change up within a moment — no restart needed");
    Ok(stats)
}

/// Delete documents by primary key. Store entries (when enabled) become
/// unreachable rather than being rewritten; the next full import reclaims
/// them.
pub fn run_delete(config: &Config, keys: &[String]) -> anyhow::Result<u64> {
    logged(config, "delete", &[], |event| {
        let deleted = run_delete_inner(config, keys)?;
        event.deleted = deleted;
        Ok(deleted)
    })
}

fn run_delete_inner(config: &Config, keys: &[String]) -> anyhow::Result<u64> {
    if keys.is_empty() {
        bail!("no keys given");
    }
    let (opened, pk_name) = open_for_incremental(config)?;

    // Count what the keys match first, so the outcome is reportable —
    // tantivy's delete_term does not say how many documents it removed.
    let reader = opened.index.reader()?;
    let searcher = reader.searcher();
    let mut matched = 0u64;
    for key in keys {
        let query = tantivy::query::TermQuery::new(
            pk_term(&opened, key),
            tantivy::schema::IndexRecordOption::Basic,
        );
        matched += searcher.search(&query, &tantivy::collector::Count)? as u64;
    }

    let mut writer: IndexWriter = opened.index.writer(50_000_000)?;
    for key in keys {
        writer.delete_term(pk_term(&opened, key));
    }
    writer.commit()?;

    // Older pairing files carry no doc_count and require store == index doc
    // counts, which a delete breaks. Re-stamp the pairing with the store's
    // ref total so the engine keeps accepting the pair.
    if config.store.enabled {
        let store_path = config.resolve_store_path();
        if let Ok(meta) = store::read_meta(&store_path) {
            let pairing = store::read_index_pairing(&config.server.index_path);
            if pairing.is_some_and(|p| p.generation == meta.generation) {
                store::write_index_pairing(
                    &config.server.index_path,
                    &meta.generation,
                    meta.doc_count,
                )?;
            }
        }
    }

    println!(
        "✓ deleted {} document(s) for {} key(s) on '{}'",
        matched,
        keys.len(),
        pk_name
    );
    Ok(matched)
}

/// One source record, whichever encoding it arrived in. `Copy` so it can be
/// handed to several helpers within one loop iteration.
#[derive(Clone, Copy)]
enum RecordRef<'a> {
    Csv(&'a csv::StringRecord),
    Json(&'a serde_json::Value),
}

/// Counts raw bytes read from the underlying file — before any decompression
/// or parsing — so progress bars track the on-disk file whatever the format.
struct ByteCounter<R> {
    inner: R,
    count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<R: std::io::Read> std::io::Read for ByteCounter<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

/// Streams records out of one source file, CSV or JSONL.
enum RecordReader {
    Csv {
        rdr: csv::Reader<Box<dyn std::io::Read>>,
        record: csv::StringRecord,
    },
    Jsonl {
        reader: std::io::BufReader<Box<dyn std::io::Read>>,
        line: String,
        value: serde_json::Value,
        line_no: u64,
        label: String,
    },
}

impl RecordReader {
    /// Open a source. Returns the reader, the CSV header row (empty for
    /// JSONL, which addresses values by path instead), and the byte counter
    /// driving the progress bar.
    ///
    /// The path `-` reads standard input, and a `.gz` suffix streams through
    /// a gzip decoder. The counter always tracks raw input bytes — before
    /// decompression — so progress and ETA follow the on-disk file.
    fn open(
        path: &Path,
        format: SourceFormat,
    ) -> anyhow::Result<(
        Self,
        Vec<String>,
        std::sync::Arc<std::sync::atomic::AtomicU64>,
    )> {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (input, label): (Box<dyn std::io::Read>, String) = if path == Path::new("-") {
            (Box::new(std::io::stdin()), "stdin".to_string())
        } else {
            let file =
                std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            (Box::new(file), path.display().to_string())
        };
        let counted: Box<dyn std::io::Read> = Box::new(ByteCounter {
            inner: input,
            count: std::sync::Arc::clone(&count),
        });
        let decoded: Box<dyn std::io::Read> = if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("gz"))
        {
            Box::new(flate2::read::MultiGzDecoder::new(counted))
        } else {
            counted
        };
        Self::from_reader(decoded, format, &label, count)
    }

    fn from_reader(
        raw: Box<dyn std::io::Read>,
        format: SourceFormat,
        label: &str,
        count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> anyhow::Result<(
        Self,
        Vec<String>,
        std::sync::Arc<std::sync::atomic::AtomicU64>,
    )> {
        match format {
            SourceFormat::Csv => {
                let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(raw);
                let headers: Vec<String> = rdr
                    .headers()
                    .with_context(|| format!("reading CSV header of {}", label))?
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                Ok((
                    Self::Csv {
                        rdr,
                        record: csv::StringRecord::new(),
                    },
                    headers,
                    count,
                ))
            }
            SourceFormat::Jsonl => Ok((
                Self::Jsonl {
                    reader: std::io::BufReader::with_capacity(256 * 1024, raw),
                    line: String::new(),
                    value: serde_json::Value::Null,
                    line_no: 0,
                    label: label.to_string(),
                },
                Vec::new(),
                count,
            )),
        }
    }

    fn next(&mut self) -> anyhow::Result<Option<RecordRef<'_>>> {
        match self {
            Self::Csv { rdr, record } => Ok(if rdr.read_record(record)? {
                Some(RecordRef::Csv(record))
            } else {
                None
            }),
            Self::Jsonl {
                reader,
                line,
                value,
                line_no,
                label,
            } => loop {
                line.clear();
                if std::io::BufRead::read_line(reader, line)? == 0 {
                    return Ok(None);
                }
                *line_no += 1;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue; // blank lines are tolerated, not records
                }
                *value = serde_json::from_str(trimmed)
                    .with_context(|| format!("{} line {} is not valid JSON", label, line_no))?;
                return Ok(Some(RecordRef::Json(value)));
            },
        }
    }
}

/// A JSON scalar as the string the rest of the pipeline works in. Arrays and
/// objects have no single-value form and return None; so does null, which
/// means "missing", not "the string null".
fn json_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Per-source machinery shared by full imports and delta updates: turns one
/// source record into an index document (and, with the store enabled, the
/// full document behind it).
struct RowIndexer<'a> {
    headers: Vec<String>,
    /// CSV: schema_field_name → csv_column_index
    col_indices: HashMap<String, usize>,
    /// JSONL: schema_field_name → path segments into the document. Every
    /// schema field has an entry — unmapped fields default to their own name.
    json_paths: HashMap<String, Vec<String>>,
    defaults: &'a HashMap<String, String>,
    field_configs: &'a [crate::config::FieldConfig],
    field_map: &'a HashMap<String, tantivy::schema::Field>,
    ref_field: Option<tantivy::schema::Field>,
    prefix_fields: HashMap<String, tantivy::schema::Field>,
    bad_number_cells: u64,
    /// JSON arrays met in fields not declared `multi`, per field. Only the
    /// first element is indexed for those — a scalar field renders one
    /// value, and a document must never match on a value it does not show.
    arrays_in_scalar_fields: HashMap<String, u64>,
}

impl<'a> RowIndexer<'a> {
    fn new(
        format: SourceFormat,
        headers: Vec<String>,
        mapping: &HashMap<String, String>,
        defaults: &'a HashMap<String, String>,
        schema: &Schema,
        field_map: &'a HashMap<String, tantivy::schema::Field>,
        field_configs: &'a [crate::config::FieldConfig],
    ) -> Self {
        let mut col_indices: HashMap<String, usize> = HashMap::new();
        let mut json_paths: HashMap<String, Vec<String>> = HashMap::new();
        match format {
            SourceFormat::Csv => {
                for (schema_name, csv_col_name) in mapping {
                    if let Some(idx) = headers.iter().position(|h| h == csv_col_name) {
                        col_indices.insert(schema_name.clone(), idx);
                    }
                }
            }
            SourceFormat::Jsonl => {
                for fc in field_configs {
                    let path = mapping
                        .get(&fc.name)
                        .map(String::as_str)
                        .unwrap_or(&fc.name);
                    json_paths.insert(fc.name.clone(), path.split('.').map(String::from).collect());
                }
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

        Self {
            headers,
            col_indices,
            json_paths,
            defaults,
            field_configs,
            field_map,
            ref_field,
            prefix_fields,
            bad_number_cells: 0,
            arrays_in_scalar_fields: HashMap::new(),
        }
    }

    /// Navigate a JSON document to a schema field's value. None for missing
    /// and for null — both mean "fall back to the source default".
    fn json_value<'v>(
        &self,
        root: &'v serde_json::Value,
        name: &str,
    ) -> Option<&'v serde_json::Value> {
        let mut current = root;
        for segment in self.json_paths.get(name)? {
            current = current.get(segment)?;
        }
        if current.is_null() {
            None
        } else {
            Some(current)
        }
    }

    /// The raw scalar for a schema field: record value first, then source
    /// default. What primary keys and numeric fields read.
    fn raw_value(&self, record: RecordRef, name: &str) -> Option<String> {
        match record {
            RecordRef::Csv(rec) => self
                .col_indices
                .get(name)
                .and_then(|&idx| rec.get(idx))
                .map(|s| s.to_string()),
            RecordRef::Json(root) => self.json_value(root, name).and_then(json_scalar),
        }
        .or_else(|| self.defaults.get(name).cloned())
    }

    /// The indexable values a record contributes to a text-like field. A
    /// JSON array feeds a multi field its elements directly; a scalar (from
    /// either format) is split by the field's separator when `multi`.
    fn text_values(&mut self, record: RecordRef, fc: &crate::config::FieldConfig) -> Vec<String> {
        if let RecordRef::Json(root) = record {
            if let Some(serde_json::Value::Array(items)) = self.json_value(root, &fc.name) {
                if fc.multi {
                    return items.iter().filter_map(json_scalar).collect();
                }
                // Not a multi field: index what will be rendered — the first
                // element — and count the mismatch for the import notes and
                // --check. Indexing every element here made a document match
                // `tags=blue` while showing `tags: "red"`.
                *self
                    .arrays_in_scalar_fields
                    .entry(fc.name.clone())
                    .or_insert(0) += 1;
                return items.iter().filter_map(json_scalar).take(1).collect();
            }
        }
        let value = self.raw_value(record, &fc.name).unwrap_or_default();
        fc.split_values(&value)
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn build_doc(
        &mut self,
        record: RecordRef,
        metadata_collector: &mut ImportFieldMetadataCollector,
        store_import: Option<&mut StoreImport>,
        sidecar: Option<&mut SidecarReader>,
    ) -> anyhow::Result<TantivyDocument> {
        let mut doc = TantivyDocument::new();

        for fc in self.field_configs {
            let field = match self.field_map.get(&fc.name) {
                Some(f) => *f,
                None => continue,
            };

            match fc.field_type {
                FieldType::Text | FieldType::Keyword | FieldType::Enum | FieldType::Boolean => {
                    // A multi field contributes one term per element, so a
                    // document can match a filter for any of them.
                    for part in self.text_values(record, fc) {
                        if let Some(normalized) = canonicalize_stored_value(fc, &part)? {
                            if fc.field_type == FieldType::Enum {
                                metadata_collector.observe(fc, &normalized);
                            }
                            doc.add_text(field, &normalized);
                            // Fuzzy fields feed their typeahead shadow field
                            // the same value; the edge_prefix tokenizer does
                            // the rest.
                            if let Some(&prefix_field) = self.prefix_fields.get(&fc.name) {
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
                    let value = self.raw_value(record, &fc.name).unwrap_or_default();
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        match trimmed.parse::<f64>() {
                            Ok(num) if num.is_finite() => doc.add_f64(field, num),
                            _ => self.bad_number_cells += 1,
                        }
                    }
                }
            }
        }

        if let Some(store_import) = store_import {
            let doc_ref = store_import.next_ref;
            if let Some(rf) = self.ref_field {
                doc.add_u64(rf, doc_ref);
            }
            let full = match store_import.mode {
                StoreSource::Row => match record {
                    RecordRef::Csv(rec) => build_row_doc(&self.headers, rec, self.defaults),
                    RecordRef::Json(root) => build_json_row_doc(root, self.defaults),
                },
                StoreSource::Sidecar => sidecar
                    .expect("sidecar reader opened for sidecar mode")
                    .next_line()?,
            };
            store_import.writer.add(full)?;
            store_import.next_ref += 1;
        }

        Ok(doc)
    }

    fn report_bad_number_cells(&self, path: &Path) {
        if self.bad_number_cells > 0 {
            eprintln!(
                "  note: {} skipped {} non-numeric cell(s) in numeric fields — those values are null, not 0",
                path.display(),
                self.bad_number_cells
            );
        }
        for problem in self.array_problems(path) {
            eprintln!("  note: {}", problem);
        }
    }

    /// One line per field that received JSON arrays without being `multi`.
    fn array_problems(&self, path: &Path) -> Vec<String> {
        let mut fields: Vec<(&String, &u64)> = self.arrays_in_scalar_fields.iter().collect();
        fields.sort();
        fields
            .into_iter()
            .map(|(field, count)| {
                format!(
                    "{}: {} array value(s) in field '{}', which is not multi — only the first \
                     element is indexed; set multi = true to index and return every element",
                    path.display(),
                    count,
                    field
                )
            })
            .collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn import_source(
    path: &Path,
    format: SourceFormat,
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
    let (mut reader, headers, bytes_read) = RecordReader::open(path, format)?;
    let mut indexer = RowIndexer::new(
        format,
        headers,
        mapping,
        defaults,
        schema,
        field_map,
        field_configs,
    );

    let mut sidecar = match (&store_import, sidecar_path) {
        (Some(si), Some(sp)) if si.mode == StoreSource::Sidecar => Some(SidecarReader::open(sp)?),
        _ => None,
    };

    let mut count = 0u64;
    while let Some(record) = reader.next()? {
        let doc = indexer.build_doc(
            record,
            metadata_collector,
            store_import.as_deref_mut(),
            sidecar.as_mut(),
        )?;
        writer.add_document(doc)?;
        count += 1;

        if count.is_multiple_of(10_000) {
            pb.set_position(bytes_read.load(std::sync::atomic::Ordering::Relaxed));
        }
    }

    if let Some(sidecar) = sidecar.as_mut() {
        sidecar.expect_exhausted(count)?;
    }

    indexer.report_bad_number_cells(path);

    pb.set_position(bytes_read.load(std::sync::atomic::Ordering::Relaxed));
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

/// Row-mode full document for a JSONL record: the document itself, with
/// source defaults merged at the top level where absent. Nested structure
/// survives — this is what makes a JSONL source its own sidecar.
fn build_json_row_doc(root: &serde_json::Value, defaults: &HashMap<String, String>) -> Vec<u8> {
    match root {
        serde_json::Value::Object(obj) => {
            let mut merged = obj.clone();
            for (key, value) in defaults {
                if !merged.contains_key(key) {
                    merged.insert(key.clone(), serde_json::Value::String(value.clone()));
                }
            }
            serde_json::to_vec(&serde_json::Value::Object(merged)).expect("object serializes")
        }
        other => serde_json::to_vec(other).expect("value serializes"),
    }
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
        test_config_pk(dir, sources, store, None)
    }

    fn test_config_pk(
        dir: &Path,
        sources: Vec<SourceConfig>,
        store: StoreConfig,
        primary_key: Option<&str>,
    ) -> Arc<Config> {
        Arc::new(Config {
            server: ServerConfig {
                port: 0,
                index_path: dir.join("index"),
                bind: "0.0.0.0".to_string(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
                doc_cache: None,
            },
            schema: SchemaConfig {
                primary_key: primary_key.map(String::from),
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
            format: None,
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
                doc_cache: None,
            },
            schema: SchemaConfig {
                primary_key: None,
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
                format: None,
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
        crate::store::write_index_pairing(&config.server.index_path, "bogus-generation", 3)
            .unwrap();

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

    fn count_of(engine: &SearchEngine, q: &str) -> usize {
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
    }

    /// The incremental path end to end: a delta CSV replaces documents by
    /// primary key and adds new ones in place — no staging swap — with the
    /// store appending fresh refs for the new versions, and `ruzz delete`
    /// removing by key afterwards.
    #[test]
    fn incremental_upsert_and_delete_with_store() {
        let dir = temp_dir("upsert");
        write_csv(&dir.join("data.csv"), CSV); // orgs 100, 200, 300
        let config = test_config_pk(
            &dir,
            vec![source(&dir, None)],
            StoreConfig {
                enabled: true,
                ..StoreConfig::default()
            },
            Some("org_number"),
        );
        run_import(&config).unwrap();

        // Delta: new version of org 100, brand-new org 400, and a keyless
        // row that must be skipped rather than indexed unreachable.
        write_csv(
            &dir.join("delta.csv"),
            "org_number,company_name,city,secret_extra\n\
             100,Acme Rockets International,Oslo,alpha2\n\
             400,Delta Diner,Trondheim,delta\n\
             ,Keyless Corp,Nowhere,x\n",
        );
        let stats = run_update(&config, &[dir.join("delta.csv")], None, None, None).unwrap();
        assert_eq!(stats.rows, 2);
        assert_eq!(stats.skipped_no_key, 1);

        let engine = SearchEngine::open(config.clone()).unwrap();
        assert_eq!(engine.store_status, StoreStatus::Ok);
        assert_eq!(count_of(&engine, ""), 4, "3 originals - 1 replaced + 2");
        assert_eq!(count_of(&engine, "keyless"), 0);

        // The upserted doc: one match, the new version, hydrating to the
        // new full record through a ref at the store's appended tail.
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
        assert_eq!(result.count, Some(1));
        assert_eq!(result.results[0]["name"], "Acme Rockets International");
        let new_ref = result.results[0]["_ref"].as_u64().unwrap();
        assert!(new_ref >= 3, "fresh ref, not the superseded 0");
        let full: serde_json::Value =
            serde_json::from_str(&engine.get_full(new_ref).unwrap().unwrap()).unwrap();
        assert_eq!(full["secret_extra"], "alpha2");

        // The added doc hydrates too; untouched docs keep their old refs.
        assert_eq!(count_of(&engine, "delta diner"), 1);
        let bakery = engine
            .search(
                "bakery",
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
        assert_eq!(bakery.results[0]["_ref"], 1);
        assert_eq!(bakery.results[0]["_full"]["secret_extra"], "bravo");

        // Delete by key: gone from search, everything else intact.
        drop(engine);
        assert_eq!(run_delete(&config, &["200".to_string()]).unwrap(), 1);
        let engine = SearchEngine::open(config.clone()).unwrap();
        assert_eq!(engine.store_status, StoreStatus::Ok);
        assert_eq!(count_of(&engine, "bakery"), 0);
        assert_eq!(count_of(&engine, ""), 3);

        // A second update on the already-updated index keeps working.
        drop(engine);
        write_csv(
            &dir.join("delta2.csv"),
            "org_number,company_name,city,secret_extra\n\
             400,Delta Diner Deluxe,Trondheim,delta2\n",
        );
        run_update(&config, &[dir.join("delta2.csv")], None, None, None).unwrap();
        let engine = SearchEngine::open(config).unwrap();
        assert_eq!(engine.store_status, StoreStatus::Ok);
        assert_eq!(count_of(&engine, "deluxe"), 1);
        assert_eq!(count_of(&engine, ""), 3);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// JSONL as a first-class source: dotted-path mapping, unmapped fields
    /// defaulting to their own name, arrays feeding multi fields, real JSON
    /// number/bool/null types, and — with the store in row mode — the record
    /// itself as the full document, nested structure intact, defaults merged.
    #[test]
    fn jsonl_source_end_to_end() {
        let dir = temp_dir("jsonl");
        std::fs::write(
            dir.join("data.jsonl"),
            r#"{"navn":"Acme Rockets","org":"100","roles":["LEDE","DAGL"],"revenue":1234.5,"address":{"city":"Oslo"},"history":[{"year":2024,"event":"founded"}]}
{"navn":"Beta Bakery","org":"200","roles":"CHAIR, CEO","revenue":null,"address":{"city":"Bergen"}}

{"navn":"Gamma Gruppen","org":"300","revenue":0}
"#,
        )
        .unwrap();

        let field = |name: &str, ft: FieldType, multi: bool| FieldConfig {
            name: name.to_string(),
            field_type: ft,
            search: None,
            values: None,
            max_values: None,
            case_sensitive: false,
            multi,
            separator: None,
            description: None,
        };
        let config = Arc::new(Config {
            server: ServerConfig {
                port: 0,
                index_path: dir.join("index"),
                bind: "0.0.0.0".to_string(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
                doc_cache: None,
            },
            schema: SchemaConfig {
                primary_key: Some("org_number".to_string()),
                fields: vec![
                    text_field("name", true),
                    field("org_number", FieldType::Keyword, false),
                    field("country_code", FieldType::Keyword, false),
                    field("city", FieldType::Keyword, false),
                    field("roles", FieldType::Keyword, true),
                    field("revenue", FieldType::Number, false),
                ],
            },
            sources: vec![SourceConfig {
                path: dir.join("data.jsonl"),
                defaults: HashMap::from([("country_code".to_string(), "NO".to_string())]),
                mapping: HashMap::from([
                    ("name".to_string(), "navn".to_string()),
                    ("org_number".to_string(), "org".to_string()),
                    ("city".to_string(), "address.city".to_string()),
                ]),
                use_mapping: None,
                sidecar: None,
                format: None, // detected from the extension
            }],
            mappings: HashMap::new(),
            store: StoreConfig {
                enabled: true,
                ..StoreConfig::default()
            },
            dashboard: DashboardConfig::default(),
        });

        let stats = run_import(&config).unwrap();
        assert_eq!(stats.total_rows, 3, "blank line is not a record");

        let engine = SearchEngine::open(config.clone()).unwrap();
        assert_eq!(engine.store_status, StoreStatus::Ok);
        let filtered = |key: &str, value: &str| {
            engine
                .search(
                    "",
                    &HashMap::from([(key.to_string(), value.to_string())]),
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
        };

        // Dotted-path mapping reaches into nested objects.
        assert_eq!(filtered("city", "Bergen").count, Some(1));
        // A JSON array feeds a multi field its elements; a comma string in a
        // multi field still splits like CSV.
        assert_eq!(filtered("roles", "DAGL").count, Some(1));
        assert_eq!(filtered("roles", "CEO").count, Some(1));
        // Top-level defaults apply to every record.
        assert_eq!(filtered("country_code", "NO").count, Some(3));
        // JSON null is a missing number; 0 is a value.
        assert_eq!(filtered("revenue", "0").count, Some(1));
        let acme = filtered("org_number", "100");
        assert_eq!(acme.results[0]["revenue"], serde_json::json!(1234.5));
        let beta = filtered("org_number", "200");
        assert_eq!(beta.results[0]["revenue"], serde_json::Value::Null);

        // Row-mode store: the record is its own full document — nested
        // structure survives, defaults merged at the top level.
        let doc_ref = acme.results[0]["_ref"].as_u64().unwrap();
        let full: serde_json::Value =
            serde_json::from_str(&engine.get_full(doc_ref).unwrap().unwrap()).unwrap();
        assert_eq!(full["history"][0]["event"], "founded");
        assert_eq!(full["address"]["city"], "Oslo");
        assert_eq!(full["country_code"], "NO");

        // And a JSONL delta upserts against it.
        drop(engine);
        std::fs::write(
            dir.join("delta.jsonl"),
            "{\"navn\":\"Acme Rockets International\",\"org\":\"100\",\"address\":{\"city\":\"Oslo\"}}\n",
        )
        .unwrap();
        run_update(&config, &[dir.join("delta.jsonl")], None, None, None).unwrap();
        let engine = SearchEngine::open(config).unwrap();
        assert_eq!(count_of(&engine, "international"), 1);
        assert_eq!(count_of(&engine, ""), 3);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Enum values an update discovers are written to the metadata file;
    /// a running engine must see them without a restart.
    #[test]
    fn enum_values_from_a_delta_show_without_restart() {
        let dir = temp_dir("enum-refresh");
        write_csv(
            &dir.join("data.csv"),
            "org_number,company_name,region\n100,Acme,Nord\n200,Beta,Vest\n",
        );
        let mut region_source = source(&dir, None);
        region_source
            .mapping
            .insert("region".to_string(), "region".to_string());
        let config = Arc::new(Config {
            server: ServerConfig {
                port: 0,
                index_path: dir.join("index"),
                bind: "0.0.0.0".to_string(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
                doc_cache: None,
            },
            schema: SchemaConfig {
                primary_key: Some("org_number".to_string()),
                fields: vec![
                    text_field("name", true),
                    keyword_field("org_number"),
                    keyword_field("country_code"),
                    FieldConfig {
                        name: "region".to_string(),
                        field_type: FieldType::Enum,
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
            sources: vec![region_source],
            mappings: HashMap::new(),
            store: StoreConfig::default(),
            dashboard: DashboardConfig::default(),
        });
        run_import(&config).unwrap();
        let engine = SearchEngine::open(config.clone()).unwrap();
        assert_eq!(
            engine.field_values("region").unwrap().values,
            vec!["Nord", "Vest"]
        );

        write_csv(
            &dir.join("delta.csv"),
            "org_number,company_name,region\n300,Gamma,Sør\n",
        );
        run_update(&config, &[dir.join("delta.csv")], None, None, None).unwrap();
        engine.reader.reload().unwrap();
        assert_eq!(
            engine.field_values("region").unwrap().values,
            vec!["Nord", "Sør", "Vest"],
            "reloaded on the new generation"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A JSON array in a field that is not `multi` used to index every
    /// element while rendering only the first, so `tags=blue` matched a
    /// document that showed `tags: "red"`. Indexed and shown must agree;
    /// the mismatch is reported by --check.
    #[test]
    fn json_arrays_in_scalar_fields_index_only_what_is_shown() {
        let dir = temp_dir("json-array-scalar");
        std::fs::write(
            dir.join("data.jsonl"),
            "{\"org_number\":\"1\",\"company_name\":\"Tagged\",\"tags\":[\"red\",\"blue\"]}\n\
             {\"org_number\":\"2\",\"company_name\":\"Green\",\"tags\":[\"green\"]}\n",
        )
        .unwrap();
        let mut src = source(&dir, None);
        src.path = dir.join("data.jsonl");
        src.mapping.insert("tags".to_string(), "tags".to_string());
        let config = Arc::new(Config {
            server: ServerConfig {
                port: 0,
                index_path: dir.join("index"),
                bind: "0.0.0.0".to_string(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
                doc_cache: None,
            },
            schema: SchemaConfig {
                primary_key: Some("org_number".to_string()),
                fields: vec![
                    text_field("name", true),
                    keyword_field("org_number"),
                    keyword_field("country_code"),
                    keyword_field("tags"), // not multi
                ],
            },
            sources: vec![src],
            mappings: HashMap::new(),
            store: StoreConfig::default(),
            dashboard: DashboardConfig::default(),
        });

        let report = run_check(&config).unwrap();
        assert!(!report.is_clean());
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("field 'tags'") && p.contains("multi = true")),
            "{:?}",
            report.problems
        );

        run_import(&config).unwrap();
        let engine = SearchEngine::open(config).unwrap();
        let by_tag = |tag: &str| {
            engine
                .search(
                    "",
                    &HashMap::from([("tags".to_string(), tag.to_string())]),
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
        };
        assert_eq!(by_tag("blue").count, Some(0), "not shown, so not matched");
        let red = by_tag("red");
        assert_eq!(red.count, Some(1));
        assert_eq!(red.results[0]["tags"], "red");
        assert_eq!(by_tag("green").count, Some(1));

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A full import ends in one segment, and `ruzz merge` brings an index
    /// that deltas have grown to several back to one — in place.
    #[test]
    fn import_ends_in_one_segment_and_merge_restores_it() {
        let dir = temp_dir("merge");
        write_csv(&dir.join("data.csv"), CSV);
        let config = test_config_pk(
            &dir,
            vec![source(&dir, None)],
            StoreConfig::default(),
            Some("org_number"),
        );
        run_import(&config).unwrap();
        let segments = |config: &Config| {
            tantivy::Index::open_in_dir(&config.server.index_path)
                .unwrap()
                .searchable_segment_ids()
                .unwrap()
                .len()
        };
        assert_eq!(segments(&config), 1, "a full import merges to one segment");

        // Every delta commits a segment of its own.
        for (i, name) in ["Delta One", "Delta Two", "Delta Three"].iter().enumerate() {
            let path = dir.join(format!("delta{i}.csv"));
            write_csv(
                &path,
                &format!(
                    "org_number,company_name,city,secret_extra\n{},{},Oslo,x\n",
                    500 + i,
                    name
                ),
            );
            run_update(&config, &[path], None, None, None).unwrap();
        }
        assert!(segments(&config) > 1, "deltas add segments");

        merge_segments(&config).unwrap();
        assert_eq!(segments(&config), 1);
        let engine = SearchEngine::open(config.clone()).unwrap();
        assert_eq!(count_of(&engine, ""), 6, "nothing lost in the merge");
        assert_eq!(count_of(&engine, "delta"), 3);

        // Idempotent: nothing to do is not an error.
        merge_segments(&config).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Gzipped sources stream through a decoder — no manual gunzip step.
    #[test]
    fn gzipped_sources_import_transparently() {
        let dir = temp_dir("gz");
        let gz_path = dir.join("data.csv.gz");
        let mut encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&gz_path).unwrap(),
            flate2::Compression::default(),
        );
        std::io::Write::write_all(&mut encoder, CSV.as_bytes()).unwrap();
        encoder.finish().unwrap();

        let mut gz_source = source(&dir, None);
        gz_source.path = gz_path;
        let config = test_config(&dir, vec![gz_source], StoreConfig::default());

        let stats = run_import(&config).unwrap();
        assert_eq!(stats.total_rows, 3);
        let engine = SearchEngine::open(config).unwrap();
        assert_eq!(count_of(&engine, "acme"), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The dry run reports what an import would find — duplicate and empty
    /// primary keys, bad numeric cells, mapping entries naming no column —
    /// without writing anything.
    #[test]
    fn check_reports_without_writing() {
        let dir = temp_dir("check");
        write_csv(
            &dir.join("data.csv"),
            "org_number,company_name,city,secret_extra\n\
             100,Acme Rockets,Oslo,a\n\
             100,Acme Duplicate,Oslo,b\n\
             ,Keyless Corp,Bergen,c\n\
             300,Gamma Gruppen,Oslo,d\n",
        );
        let mut bad_mapping_source = source(&dir, None);
        bad_mapping_source
            .mapping
            .insert("country_code".to_string(), "no_such_column".to_string());
        let config = test_config_pk(
            &dir,
            vec![bad_mapping_source],
            StoreConfig::default(),
            Some("org_number"),
        );

        let report = run_check(&config).unwrap();
        assert_eq!(report.rows, 4);
        assert_eq!(report.bad_rows, 0);
        assert_eq!(report.duplicate_keys, 1);
        assert_eq!(report.empty_keys, 1);
        assert!(
            report.problems.iter().any(|p| p.contains("no_such_column")),
            "unmapped column reported: {:?}",
            report.problems
        );
        assert!(!config.server.index_path.exists(), "check writes nothing");
        // Duplicate and empty keys are problems, not footnotes: the verdict
        // used to say "no problems found" right under a duplicate count.
        assert!(!report.is_clean());
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("1 duplicate value")),
            "{:?}",
            report.problems
        );
        assert!(report
            .problems
            .iter()
            .any(|p| p.contains("1 row(s) with an empty value")));

        let _ = std::fs::remove_dir_all(dir);
    }

    /// With several sources configured, an unambiguous delta is matched to
    /// its source by column set; a delta matching nothing still asks for
    /// --like.
    #[test]
    fn delta_source_is_inferred_from_its_columns() {
        let dir = temp_dir("infer");
        write_csv(&dir.join("data.csv"), CSV);
        write_csv(
            &dir.join("other.csv"),
            "orgnr,navn\n900,Norsk Bedrift\n", // different column names
        );
        let mut other = source(&dir, None);
        other.path = dir.join("other.csv");
        other.mapping = HashMap::from([
            ("name".to_string(), "navn".to_string()),
            ("org_number".to_string(), "orgnr".to_string()),
        ]);
        let config = test_config_pk(
            &dir,
            vec![source(&dir, None), other],
            StoreConfig::default(),
            Some("org_number"),
        );
        run_import(&config).unwrap();

        // Columns match the second source only — inferred, no --like needed.
        write_csv(
            &dir.join("delta.csv"),
            "orgnr,navn\n900,Norsk Bedrift ASA\n",
        );
        run_update(&config, &[dir.join("delta.csv")], None, None, None).unwrap();
        let engine = SearchEngine::open(config.clone()).unwrap();
        assert_eq!(count_of(&engine, "asa"), 1);
        assert_eq!(count_of(&engine, ""), 4, "upsert into the right mapping");
        drop(engine);

        // A delta matching neither source still requires the flag.
        write_csv(&dir.join("mystery.csv"), "id,label\n1,x\n");
        let err = run_update(&config, &[dir.join("mystery.csv")], None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--like"), "got: {err}");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A JSONL delta against a CSV source: the template's mapping values
    /// double as top-level keys in the delta's documents.
    #[test]
    fn jsonl_delta_against_csv_source() {
        let dir = temp_dir("jsonl-delta");
        write_csv(&dir.join("data.csv"), CSV);
        let config = test_config_pk(
            &dir,
            vec![source(&dir, None)],
            StoreConfig::default(),
            Some("org_number"),
        );
        run_import(&config).unwrap();

        std::fs::write(
            dir.join("delta.jsonl"),
            "{\"company_name\":\"Acme via JSON\",\"org_number\":\"100\"}\n",
        )
        .unwrap();
        run_update(&config, &[dir.join("delta.jsonl")], None, None, None).unwrap();

        let engine = SearchEngine::open(config).unwrap();
        assert_eq!(count_of(&engine, "via json"), 1);
        assert_eq!(count_of(&engine, ""), 3, "upsert, not append");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn incremental_update_without_store() {
        let dir = temp_dir("upsert-nostore");
        write_csv(&dir.join("data.csv"), CSV);
        let config = test_config_pk(
            &dir,
            vec![source(&dir, None)],
            StoreConfig::default(),
            Some("org_number"),
        );
        run_import(&config).unwrap();

        write_csv(
            &dir.join("delta.csv"),
            "org_number,company_name,city,secret_extra\n\
             300,Gamma Group Global,Oslo,c2\n",
        );
        run_update(&config, &[dir.join("delta.csv")], None, None, None).unwrap();

        let engine = SearchEngine::open(config).unwrap();
        assert_eq!(count_of(&engine, ""), 3);
        assert_eq!(count_of(&engine, "gruppen"), 0, "old version replaced");
        assert_eq!(count_of(&engine, "global"), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Updates refuse to run against the wrong foundation: no primary key,
    /// no index yet, or an index built with a different schema.
    #[test]
    fn incremental_update_guardrails() {
        let dir = temp_dir("guard");
        write_csv(&dir.join("data.csv"), CSV);

        // No primary key configured.
        let no_pk = test_config(&dir, vec![source(&dir, None)], StoreConfig::default());
        let err = run_update(&no_pk, &[dir.join("data.csv")], None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("primary key"), "got: {err}");

        // No index built yet.
        let config = test_config_pk(
            &dir,
            vec![source(&dir, None)],
            StoreConfig::default(),
            Some("org_number"),
        );
        let err = run_update(&config, &[dir.join("data.csv")], None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("full import"), "got: {err}");

        // Schema drift since the index was built.
        run_import(&config).unwrap();
        let mut drifted_fields = vec![
            text_field("name", true),
            keyword_field("org_number"),
            keyword_field("country_code"),
        ];
        drifted_fields.push(keyword_field("city"));
        let drifted = Arc::new(Config {
            server: ServerConfig {
                port: 0,
                index_path: dir.join("index"),
                bind: "0.0.0.0".to_string(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
                doc_cache: None,
            },
            schema: SchemaConfig {
                primary_key: Some("org_number".to_string()),
                fields: drifted_fields,
            },
            sources: vec![source(&dir, None)],
            mappings: HashMap::new(),
            store: StoreConfig::default(),
            dashboard: DashboardConfig::default(),
        });
        let err = run_update(&drifted, &[dir.join("data.csv")], None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("different schema"), "got: {err}");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Primary keys go through the same folding as the index terms, so a
    /// differently-cased key still hits its document.
    #[test]
    fn incremental_keys_fold_like_the_index() {
        let dir = temp_dir("foldkey");
        write_csv(
            &dir.join("data.csv"),
            "org_number,company_name,city,secret_extra\n\
             A100,Acme Rockets,Oslo,alpha\n\
             B200,Beta Bakery,Bergen,bravo\n",
        );
        let config = test_config_pk(
            &dir,
            vec![source(&dir, None)],
            StoreConfig::default(),
            Some("org_number"),
        );
        run_import(&config).unwrap();

        // Upsert with a lowercased key replaces, not duplicates.
        write_csv(
            &dir.join("delta.csv"),
            "org_number,company_name,city,secret_extra\n\
             a100,Acme Rebuilt,Oslo,alpha2\n",
        );
        run_update(&config, &[dir.join("delta.csv")], None, None, None).unwrap();
        let engine = SearchEngine::open(config.clone()).unwrap();
        assert_eq!(count_of(&engine, ""), 2);
        assert_eq!(count_of(&engine, "rebuilt"), 1);
        drop(engine);

        // Delete with the other casing.
        assert_eq!(run_delete(&config, &["b200".to_string()]).unwrap(), 1);
        let engine = SearchEngine::open(config).unwrap();
        assert_eq!(count_of(&engine, ""), 1);

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
