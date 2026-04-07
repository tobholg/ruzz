use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use crate::config::{EnumValuesConfig, FieldConfig, FieldType};

pub const DEFAULT_AUTO_ENUM_MAX_VALUES: usize = 128;
pub const BOOLEAN_TRUE: &str = "TRUE";
pub const BOOLEAN_FALSE: &str = "FALSE";
const FIELD_METADATA_FILE: &str = "ruzz_field_metadata.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct StoredFieldMetadata {
    #[serde(default)]
    pub fields: HashMap<String, StoredFieldMetadataEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct StoredFieldMetadataEntry {
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeFieldMetadata {
    pub values: Vec<String>,
    pub truncated: bool,
}

pub fn metadata_path(index_path: &Path) -> PathBuf {
    index_path.join(FIELD_METADATA_FILE)
}

pub fn canonicalize_enum_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_uppercase())
    }
}

pub fn canonicalize_boolean_value(value: &str) -> Option<String> {
    let normalized = value.trim().to_uppercase();
    match normalized.as_str() {
        "" => None,
        "TRUE" | "T" | "YES" | "Y" | "1" => Some(BOOLEAN_TRUE.to_string()),
        "FALSE" | "F" | "NO" | "N" | "0" => Some(BOOLEAN_FALSE.to_string()),
        _ => Some(normalized),
    }
}

pub fn canonicalize_stored_value(
    field: &FieldConfig,
    value: &str,
) -> anyhow::Result<Option<String>> {
    match field.field_type {
        FieldType::Text | FieldType::Keyword => {
            if value.is_empty() {
                Ok(None)
            } else {
                Ok(Some(value.to_string()))
            }
        }
        FieldType::Enum => Ok(canonicalize_enum_value(value)),
        FieldType::Boolean => {
            let Some(normalized) = canonicalize_boolean_value(value) else {
                return Ok(None);
            };
            if normalized == BOOLEAN_TRUE || normalized == BOOLEAN_FALSE {
                Ok(Some(normalized))
            } else {
                bail!(
                    "invalid boolean value '{}' for field '{}'",
                    value,
                    field.name
                );
            }
        }
        FieldType::Number => Ok(None),
    }
}

pub fn canonicalize_filter_value(field_type: &FieldType, value: &str) -> Option<String> {
    match field_type {
        FieldType::Text | FieldType::Keyword => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        FieldType::Enum => canonicalize_enum_value(value),
        FieldType::Boolean => canonicalize_boolean_value(value),
        FieldType::Number => None,
    }
}

pub fn json_boolean_value(value: &str) -> serde_json::Value {
    match value {
        BOOLEAN_TRUE => serde_json::Value::Bool(true),
        BOOLEAN_FALSE => serde_json::Value::Bool(false),
        other => serde_json::Value::String(other.to_string()),
    }
}

pub fn runtime_metadata_for_field(
    field: &FieldConfig,
    stored: Option<&StoredFieldMetadataEntry>,
) -> RuntimeFieldMetadata {
    match field.field_type {
        FieldType::Boolean => RuntimeFieldMetadata {
            values: vec![BOOLEAN_TRUE.to_string(), BOOLEAN_FALSE.to_string()],
            truncated: false,
        },
        FieldType::Enum => {
            if let Some(values) = explicit_enum_values(field) {
                return RuntimeFieldMetadata {
                    values,
                    truncated: false,
                };
            }

            if let Some(stored) = stored {
                return RuntimeFieldMetadata {
                    values: stored.values.clone(),
                    truncated: stored.truncated,
                };
            }

            RuntimeFieldMetadata::default()
        }
        _ => RuntimeFieldMetadata::default(),
    }
}

pub fn load_stored_field_metadata(index_path: &Path) -> anyhow::Result<StoredFieldMetadata> {
    let path = metadata_path(index_path);
    if !path.exists() {
        return Ok(StoredFieldMetadata::default());
    }

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed reading field metadata from {}", path.display()))?;
    let metadata = serde_json::from_str(&raw)
        .with_context(|| format!("failed parsing field metadata from {}", path.display()))?;
    Ok(metadata)
}

pub fn write_stored_field_metadata(
    index_path: &Path,
    metadata: &StoredFieldMetadata,
) -> anyhow::Result<()> {
    let path = metadata_path(index_path);
    let raw = serde_json::to_string_pretty(metadata)?;
    std::fs::write(&path, raw)
        .with_context(|| format!("failed writing field metadata to {}", path.display()))?;
    Ok(())
}

pub fn explicit_enum_values(field: &FieldConfig) -> Option<Vec<String>> {
    match field.values.as_ref() {
        Some(EnumValuesConfig::List(values)) => {
            let mut out: Vec<String> = values
                .iter()
                .filter_map(|value| canonicalize_enum_value(value))
                .collect();
            out.sort();
            out.dedup();
            Some(out)
        }
        _ => None,
    }
}

pub fn enum_auto_max(field: &FieldConfig) -> usize {
    field.max_values.unwrap_or(DEFAULT_AUTO_ENUM_MAX_VALUES)
}

pub struct ImportFieldMetadataCollector {
    fields: HashMap<String, EnumCollector>,
}

impl ImportFieldMetadataCollector {
    pub fn new(field_configs: &[FieldConfig]) -> Self {
        let mut fields = HashMap::new();

        for field in field_configs {
            if field.field_type != FieldType::Enum {
                continue;
            }

            let collector = if let Some(values) = explicit_enum_values(field) {
                EnumCollector::fixed(values)
            } else {
                EnumCollector::auto(enum_auto_max(field))
            };

            fields.insert(field.name.clone(), collector);
        }

        Self { fields }
    }

    pub fn observe(&mut self, field: &FieldConfig, value: &str) {
        if field.field_type != FieldType::Enum {
            return;
        }

        let Some(collector) = self.fields.get_mut(&field.name) else {
            return;
        };
        collector.observe(value);
    }

    pub fn into_stored(self) -> StoredFieldMetadata {
        let fields = self
            .fields
            .into_iter()
            .map(|(name, collector)| (name, collector.into_stored()))
            .collect();

        StoredFieldMetadata { fields }
    }
}

struct EnumCollector {
    mode: EnumCollectorMode,
    values: BTreeSet<String>,
    truncated: bool,
}

enum EnumCollectorMode {
    Fixed,
    Auto { max_values: usize },
}

impl EnumCollector {
    fn fixed(values: Vec<String>) -> Self {
        Self {
            mode: EnumCollectorMode::Fixed,
            values: values.into_iter().collect(),
            truncated: false,
        }
    }

    fn auto(max_values: usize) -> Self {
        Self {
            mode: EnumCollectorMode::Auto { max_values },
            values: BTreeSet::new(),
            truncated: false,
        }
    }

    fn observe(&mut self, value: &str) {
        if self.truncated || value.is_empty() {
            return;
        }

        match self.mode {
            EnumCollectorMode::Fixed => {}
            EnumCollectorMode::Auto { max_values } => {
                self.values.insert(value.to_string());
                if self.values.len() > max_values {
                    self.values.clear();
                    self.truncated = true;
                }
            }
        }
    }

    fn into_stored(self) -> StoredFieldMetadataEntry {
        StoredFieldMetadataEntry {
            values: self.values.into_iter().collect(),
            truncated: self.truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_enum_to_uppercase() {
        assert_eq!(
            canonicalize_enum_value("  board chair "),
            Some("BOARD CHAIR".to_string())
        );
    }

    #[test]
    fn canonicalizes_boolean_aliases() {
        assert_eq!(
            canonicalize_boolean_value("yes"),
            Some(BOOLEAN_TRUE.to_string())
        );
        assert_eq!(
            canonicalize_boolean_value("0"),
            Some(BOOLEAN_FALSE.to_string())
        );
    }
}
