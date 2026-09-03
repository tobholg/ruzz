use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub schema: SchemaConfig,
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub mappings: HashMap<String, HashMap<String, String>>,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
}

/// Presentation defaults for the built-in web dashboard. Purely cosmetic —
/// nothing here changes what the API serves.
#[derive(Debug, Default, Deserialize)]
pub struct DashboardConfig {
    /// Human label for this deployment, shown in the dashboard header
    /// ("Norwegian companies"). Unset shows nothing — a filesystem path
    /// would only confuse the people the dashboard is for.
    #[serde(default)]
    pub name: Option<String>,
    /// Columns the results table shows by default, in order. Unset means
    /// the dashboard picks: the fuzzy field first, then schema order.
    /// Users can still override this per browser from the Columns menu.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    /// Filters offered up-front in the dashboard's filter strip, in order.
    /// On a wide schema "the first N fields" is rarely what anyone wants to
    /// filter by; this names the hot ones. The rest stay reachable through
    /// the add-filter search and the settings modal.
    #[serde(default)]
    pub filters: Option<Vec<String>>,
}

/// Optional on-disk document store for full records behind the compact
/// search rows. Disabled unless `[store] enabled = true`.
#[derive(Debug, Deserialize)]
pub struct StoreConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Where full documents come from:
    /// "row" — every CSV column (original names) + source defaults, as JSON
    /// "sidecar" — an aligned JSONL file per source, stored verbatim
    #[serde(default)]
    pub source: StoreSource,
    /// zstd compression level (1 = fastest, good ratio on record data)
    #[serde(default = "default_compression_level")]
    pub compression_level: i32,
    /// Raw bytes per compressed block — bounds per-lookup decompress cost
    #[serde(default = "default_block_size")]
    pub block_size: String,
    /// LRU cache of decompressed blocks held in memory
    #[serde(default = "default_store_cache")]
    pub cache: String,
    /// Store directory. Default: `<index_path>-store`, so two indexes in one
    /// directory never share a store. Set this to place it elsewhere.
    pub path: Option<PathBuf>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source: StoreSource::default(),
            compression_level: default_compression_level(),
            block_size: default_block_size(),
            cache: default_store_cache(),
            path: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum StoreSource {
    #[default]
    Row,
    Sidecar,
}

impl SearchMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SearchMode::Fuzzy => "fuzzy",
            SearchMode::Substring => "substring",
        }
    }
}

impl StoreSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            StoreSource::Row => "row",
            StoreSource::Sidecar => "sidecar",
        }
    }
}

fn default_compression_level() -> i32 {
    1
}

fn default_block_size() -> String {
    "256KB".to_string()
}

fn default_store_cache() -> String {
    "64MB".to_string()
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    pub index_path: PathBuf,
    /// Address to listen on. Defaults to "0.0.0.0" — every interface — which
    /// is what a standalone deployment wants. Set "127.0.0.1" when a reverse
    /// proxy terminates TLS in front, so the plain-HTTP port is unreachable
    /// from outside the host and the proxy is the only way in.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Memory budget for index pages. Examples: "512MB", "2GB", "100%"
    /// Default: "100%" (no limit, keep everything warm)
    #[serde(default = "default_memory_budget")]
    pub memory_budget: String,
    /// Optional auth token. When set, all API requests (except /health)
    /// require Authorization: Bearer <token> header or ?token=<token> param.
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Reject requests carrying unknown query parameters with HTTP 400
    /// instead of silently ignoring them. When false (the default), unknown
    /// parameters are still reported in the response as
    /// `ignored_parameters` so mistakes surface either way.
    #[serde(default)]
    pub strict_params: bool,
    /// Memory for decompressed stored-document blocks, kept in the process
    /// (not the page cache). The rerank stage fetches 50–1000 stored
    /// documents per fuzzy query and each costs an LZ4 decode of its 16KB
    /// block; this keeps the hot blocks decoded. Default "128MB". tantivy's
    /// own default is 100 blocks (~1.6MB).
    #[serde(default)]
    pub doc_cache: Option<String>,
    /// Whether /search computes the exact `count` when the caller does not
    /// say. An exact count traverses every candidate — tens of milliseconds
    /// per query on tens of millions of rows — so a large instance may
    /// default it off while smaller ones keep it. The `count` parameter
    /// always overrides; the dashboard follows this until a user chooses.
    #[serde(default = "default_default_count")]
    pub default_count: bool,
}

fn default_default_count() -> bool {
    true
}

impl ServerConfig {
    /// Stored-document block cache size in tantivy blocks (16KB each).
    pub fn doc_cache_blocks(&self) -> usize {
        const BLOCK: u64 = 16 * 1024;
        let bytes = self
            .doc_cache
            .as_deref()
            .and_then(crate::store::parse_size)
            .unwrap_or(128 * 1024 * 1024);
        (bytes / BLOCK).max(1) as usize
    }
}

fn default_memory_budget() -> String {
    "100%".to_string()
}

/// Every interface. Unchanged from before the option existed, so upgrading
/// the binary never makes a running deployment unreachable.
fn default_bind() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8888
}

#[derive(Debug, Deserialize)]
pub struct SchemaConfig {
    pub fields: Vec<FieldConfig>,
    /// Keyword field that uniquely identifies a document. Setting it enables
    /// incremental updates: `ruzz update` upserts rows by this key and
    /// `ruzz delete` removes them, without a full re-import.
    #[serde(default)]
    pub primary_key: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FieldConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub search: Option<SearchMode>,
    #[serde(default)]
    pub values: Option<EnumValuesConfig>,
    #[serde(default)]
    pub max_values: Option<usize>,
    /// Keyword fields match case-insensitively by default. Set true to
    /// require exact casing (identifiers, codes with meaningful case).
    #[serde(default)]
    pub case_sensitive: bool,
    /// Treat the source value as a list: "LEDE,DAGL" indexes two terms, so
    /// one document can match a filter for either. Applies to keyword and
    /// enum fields; values are split on `separator`.
    #[serde(default)]
    pub multi: bool,
    /// Separator for `multi` fields. Defaults to a comma.
    #[serde(default)]
    pub separator: Option<String>,
    /// What this field means, in your terms — units, currency, how it was
    /// derived. Appended to the generated description in `/fields`, `/docs`
    /// and `/openapi.json`. The engine cannot infer that a number is in
    /// thousands, and a caller cannot guess it.
    #[serde(default)]
    pub description: Option<String>,
}

impl FieldConfig {
    /// Values this cell contributes to the index. A single value for ordinary
    /// fields; each element for multi fields.
    pub fn split_values<'a>(&self, value: &'a str) -> Vec<&'a str> {
        if !self.multi {
            return vec![value];
        }
        let separator = self.separator.as_deref().unwrap_or(",");
        value
            .split(separator)
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect()
    }
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Text,
    Keyword,
    Number,
    Enum,
    Boolean,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum EnumValuesConfig {
    Auto(String),
    List(Vec<String>),
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    /// Trigram-indexed and searched by the global `q` parameter.
    Fuzzy,
    /// Trigram-indexed but excluded from `q`; queried through its own
    /// parameter, where every trigram of the value must be present. Gives
    /// case-insensitive substring matching over free text such as street
    /// addresses without diluting name relevance.
    Substring,
}

/// How a source file's records are encoded.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SourceFormat {
    Csv,
    /// One JSON document per line (NDJSON). Mapping values are dotted paths
    /// into the document; a schema field with no mapping entry uses its own
    /// name as the path.
    Jsonl,
}

/// Format implied by a file name: `.jsonl` / `.ndjson` (optionally behind
/// `.gz`) is JSONL, everything else is CSV.
pub fn detect_format(path: &std::path::Path) -> SourceFormat {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name = name.strip_suffix(".gz").unwrap_or(&name);
    if name.ends_with(".jsonl") || name.ends_with(".ndjson") {
        SourceFormat::Jsonl
    } else {
        SourceFormat::Csv
    }
}

#[derive(Debug, Deserialize)]
pub struct SourceConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub defaults: HashMap<String, String>,
    #[serde(default)]
    pub mapping: HashMap<String, String>,
    /// Reference a named mapping from [mappings.*]
    pub use_mapping: Option<String>,
    /// Aligned JSONL file with one full document per CSV row.
    /// Required for every source when [store] source = "sidecar".
    pub sidecar: Option<PathBuf>,
    /// Record encoding. Defaults to what the file extension implies.
    #[serde(default)]
    pub format: Option<SourceFormat>,
}

impl SourceConfig {
    /// Resolve the effective column mapping (inline or referenced)
    pub fn resolved_mapping<'a>(
        &'a self,
        named: &'a HashMap<String, HashMap<String, String>>,
    ) -> HashMap<String, String> {
        if let Some(ref name) = self.use_mapping {
            if let Some(m) = named.get(name) {
                return m.clone();
            }
        }
        self.mapping.clone()
    }

    pub fn resolved_format(&self) -> SourceFormat {
        self.format.unwrap_or_else(|| detect_format(&self.path))
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if let Some(ref pk) = self.schema.primary_key {
            let Some(fc) = self.schema.fields.iter().find(|f| &f.name == pk) else {
                anyhow::bail!("primary_key '{}' is not a schema field", pk);
            };
            // Delete-by-term needs one exact indexed term per document, so
            // the key must be a whole-value field and must not fan out.
            if fc.field_type != FieldType::Keyword {
                anyhow::bail!(
                    "primary_key '{}' must be a keyword field, not {:?}",
                    pk,
                    fc.field_type
                );
            }
            if fc.multi {
                anyhow::bail!("primary_key '{}' cannot be a multi field", pk);
            }
        }
        if self.store.enabled {
            for fc in &self.schema.fields {
                if fc.name.starts_with('_') {
                    anyhow::bail!(
                        "schema field '{}' conflicts with reserved names (fields starting with '_') when the document store is enabled",
                        fc.name
                    );
                }
            }
            if self.store.source == StoreSource::Sidecar {
                for source in &self.sources {
                    if source.sidecar.is_none() {
                        anyhow::bail!(
                            "[store] source = \"sidecar\" requires a sidecar path on every source; missing for {}",
                            source.path.display()
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Store directory for this index.
    ///
    /// Defaults to a sibling named after the index — `data/index` gets
    /// `data/index-store` — so two indexes in one directory cannot collide.
    /// The old default was a fixed `store` sibling, which meant building
    /// `index_v2` next to a live `index` quietly overwrote the store the live
    /// index was still serving from. An explicit `[store] path` always wins.
    pub fn store_path(&self) -> PathBuf {
        if let Some(ref p) = self.store.path {
            return p.clone();
        }
        self.default_store_path()
    }

    /// Where a store for this index is created.
    pub fn default_store_path(&self) -> PathBuf {
        let index = &self.server.index_path;
        let mut name = index
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("index"))
            .to_os_string();
        name.push("-store");
        Self::parent_of(index).join(name)
    }

    /// The pre-0.2 location: a fixed `store` sibling of the index. Still read
    /// when it is the only store present, so upgrading the binary under an
    /// existing deployment does not lose the store.
    pub fn legacy_store_path(&self) -> PathBuf {
        Self::parent_of(&self.server.index_path).join("store")
    }

    /// Store to read: the configured path, else the current default, else the
    /// legacy location if that is where the store actually lives.
    pub fn resolve_store_path(&self) -> PathBuf {
        if let Some(ref p) = self.store.path {
            return p.clone();
        }
        let default = self.default_store_path();
        if default.exists() {
            return default;
        }
        let legacy = self.legacy_store_path();
        if crate::store::looks_like_store_dir(&legacy) {
            return legacy;
        }
        default
    }

    fn parent_of(path: &std::path::Path) -> &std::path::Path {
        match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => std::path::Path::new("."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config_for(index_path: &str, store_path: Option<&str>) -> Config {
        Config {
            dashboard: DashboardConfig::default(),
            server: ServerConfig {
                port: 8888,
                index_path: PathBuf::from(index_path),
                bind: default_bind(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
                doc_cache: None,
                default_count: true,
            },
            schema: SchemaConfig {
                fields: Vec::new(),
                primary_key: None,
            },
            sources: Vec::new(),
            mappings: HashMap::new(),
            store: StoreConfig {
                enabled: true,
                path: store_path.map(PathBuf::from),
                ..StoreConfig::default()
            },
        }
    }

    /// The bug this default exists to prevent: building `index_v2` beside a
    /// live `index` used to resolve to the same `store` directory, so the
    /// import wiped the store the live index was still serving from.
    #[test]
    fn two_indexes_in_one_directory_get_separate_stores() {
        let live = config_for("data/output/index", None);
        let next = config_for("data/output/index_v2", None);
        assert_eq!(live.store_path(), PathBuf::from("data/output/index-store"));
        assert_eq!(
            next.store_path(),
            PathBuf::from("data/output/index_v2-store")
        );
        assert_ne!(live.store_path(), next.store_path());
        // Both used to land here, which is what made the collision silent
        assert_eq!(live.legacy_store_path(), next.legacy_store_path());
    }

    #[test]
    fn explicit_path_always_wins() {
        let config = config_for("data/output/index", Some("/mnt/big/store"));
        assert_eq!(config.store_path(), PathBuf::from("/mnt/big/store"));
        assert_eq!(config.resolve_store_path(), PathBuf::from("/mnt/big/store"));
    }

    /// Upgrading the binary under a deployment whose store sits at the old
    /// path must keep serving it rather than reporting the store missing.
    #[test]
    fn falls_back_to_the_legacy_store_when_that_is_where_it_lives() {
        let dir = std::env::temp_dir().join(format!(
            "ruzz-store-path-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let index = dir.join("index");
        std::fs::create_dir_all(&index).unwrap();
        let legacy = dir.join("store");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(crate::store::META_FILE), "{}").unwrap();

        let config = config_for(index.to_str().unwrap(), None);
        assert_eq!(
            config.resolve_store_path(),
            legacy,
            "legacy store must be found"
        );

        // Once a store exists at the new path, that is the one used.
        let current = config.default_store_path();
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join(crate::store::META_FILE), "{}").unwrap();
        assert_eq!(config.resolve_store_path(), current);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_follows_the_extension_through_gz() {
        use std::path::Path;
        assert_eq!(detect_format(Path::new("data/a.csv")), SourceFormat::Csv);
        assert_eq!(
            detect_format(Path::new("data/a.jsonl")),
            SourceFormat::Jsonl
        );
        assert_eq!(detect_format(Path::new("a.NDJSON")), SourceFormat::Jsonl);
        assert_eq!(detect_format(Path::new("a.jsonl.gz")), SourceFormat::Jsonl);
        assert_eq!(detect_format(Path::new("a.csv.gz")), SourceFormat::Csv);
        assert_eq!(detect_format(Path::new("weird.txt")), SourceFormat::Csv);
    }

    /// A bare index name has no parent; the store must not end up at the
    /// filesystem root or as a sibling of the working directory.
    #[test]
    fn bare_index_name_stays_in_the_working_directory() {
        let config = config_for("index", None);
        assert_eq!(config.store_path(), PathBuf::from("./index-store"));
    }
}
