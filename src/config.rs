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

#[derive(Debug, Deserialize)]
pub struct FieldConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub search: Option<SearchMode>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Text,
    Keyword,
    Number,
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
}

impl SourceConfig {
    /// Resolve the effective column mapping (inline or referenced)
    pub fn resolved_mapping<'a>(&'a self, named: &'a HashMap<String, HashMap<String, String>>) -> HashMap<String, String> {
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
        Ok(config)
    }
}
