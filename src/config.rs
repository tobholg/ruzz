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
    /// Store directory. Default: sibling "store" of index_path.
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
    /// Memory budget for index pages. Examples: "512MB", "2GB", "100%"
    /// Default: "100%" (no limit, keep everything warm)
    #[serde(default = "default_memory_budget")]
    pub memory_budget: String,
    /// Optional auth token. When set, all API requests (except /health)
    /// require Authorization: Bearer <token> header or ?token=<token> param.
    #[serde(default)]
    pub auth_token: Option<String>,
}

fn default_memory_budget() -> String {
    "100%".to_string()
}

fn default_port() -> u16 {
    8888
}

#[derive(Debug, Deserialize)]
pub struct SchemaConfig {
    pub fields: Vec<FieldConfig>,
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

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Fuzzy,
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
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
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

    /// Store directory: explicit [store] path, or sibling "store" of index_path
    pub fn store_path(&self) -> PathBuf {
        if let Some(ref p) = self.store.path {
            return p.clone();
        }
        self.server
            .index_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("store")
    }
}
