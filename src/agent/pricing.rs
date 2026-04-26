// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;

pub const DEFAULT_PRICING_CONFIG_DISPLAY_PATH: &str = "~/.gosh/agent/model_pricing.toml";

#[derive(Debug, Clone, PartialEq)]
pub struct ModelPricing {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
    pub reasoning_per_1k: f64,
    pub cache_read_per_1k: f64,
    pub cache_write_per_1k: f64,
}

#[derive(Debug, Clone, Default)]
pub struct PricingCatalog {
    models: BTreeMap<String, ModelPricing>,
}

impl PricingCatalog {
    pub fn load_default() -> Result<Self> {
        let path = default_pricing_config_path()?;
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading pricing config {}", path.display()))?;
        Self::from_toml_str(&raw)
    }

    pub fn from_toml_str(raw: &str) -> Result<Self> {
        let parsed: PricingConfigFile = toml::from_str(raw).context("parsing pricing config")?;
        let mut models = BTreeMap::new();

        for (model_id, entry) in parsed.models {
            let pricing = ModelPricing {
                input_per_1k: entry.input_per_1k,
                output_per_1k: entry.output_per_1k,
                reasoning_per_1k: entry.reasoning_per_1k,
                cache_read_per_1k: entry.cache_read_per_1k,
                cache_write_per_1k: entry.cache_write_per_1k,
            };
            validate_non_negative(&model_id, "input_per_1k", pricing.input_per_1k)?;
            validate_non_negative(&model_id, "output_per_1k", pricing.output_per_1k)?;
            validate_non_negative(&model_id, "reasoning_per_1k", pricing.reasoning_per_1k)?;
            validate_non_negative(&model_id, "cache_read_per_1k", pricing.cache_read_per_1k)?;
            validate_non_negative(&model_id, "cache_write_per_1k", pricing.cache_write_per_1k)?;
            models.insert(model_id, pricing);
        }

        Ok(Self { models })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn for_model(&self, model_id: &str) -> Result<&ModelPricing> {
        self.models.get(model_id).with_context(|| {
            format!(
                "missing pricing for model {model_id}; add it to {DEFAULT_PRICING_CONFIG_DISPLAY_PATH}"
            )
        })
    }

    pub fn override_for_model(&self, model_id: &str) -> Option<&ModelPricing> {
        self.models.get(model_id)
    }
}

pub fn default_pricing_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory for pricing config")?;
    Ok(home.join(".gosh").join("agent").join("model_pricing.toml"))
}

fn validate_non_negative(model_id: &str, field: &str, value: f64) -> Result<()> {
    if value.is_nan() || value < 0.0 {
        bail!("pricing for model {model_id} has invalid {field}: {value}");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct PricingConfigFile {
    #[serde(default)]
    models: BTreeMap<String, PricingEntry>,
}

#[derive(Debug, Deserialize)]
struct PricingEntry {
    input_per_1k: f64,
    output_per_1k: f64,
    #[serde(default)]
    reasoning_per_1k: f64,
    #[serde(default)]
    cache_read_per_1k: f64,
    #[serde(default)]
    cache_write_per_1k: f64,
}

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;

    use super::ModelPricing;
    use super::PricingCatalog;
    use super::DEFAULT_PRICING_CONFIG_DISPLAY_PATH;

    #[test]
    fn pricing_config_loads_exact_model_entry() {
        let catalog = PricingCatalog::from_toml_str(
            r#"
                [models."openai/o3"]
                input_per_1k = 2.0
                output_per_1k = 8.0
                reasoning_per_1k = 8.0
            "#,
        )
        .unwrap();

        assert_eq!(
            catalog.for_model("openai/o3").unwrap(),
            &ModelPricing {
                input_per_1k: 2.0,
                output_per_1k: 8.0,
                reasoning_per_1k: 8.0,
                cache_read_per_1k: 0.0,
                cache_write_per_1k: 0.0,
            }
        );
    }

    #[test]
    fn missing_model_pricing_returns_explicit_error() {
        let catalog = PricingCatalog::from_toml_str(
            r#"
                [models."openai/gpt-4.1"]
                input_per_1k = 1.0
                output_per_1k = 2.0
            "#,
        )
        .unwrap();

        let err = catalog.for_model("openai/o3").unwrap_err().to_string();
        assert!(err.contains("missing pricing for model openai/o3"));
        assert!(err.contains(DEFAULT_PRICING_CONFIG_DISPLAY_PATH));
    }

    #[test]
    fn negative_pricing_field_is_rejected() {
        let err = PricingCatalog::from_toml_str(
            r#"
                [models."openai/o3"]
                input_per_1k = -1.0
                output_per_1k = 8.0
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("invalid input_per_1k"));
    }

    #[test]
    fn missing_file_loads_empty_catalog() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().with_extension("missing");
        let catalog = PricingCatalog::load_from_path(&path).unwrap();
        let err = catalog.for_model("openai/o3").unwrap_err().to_string();
        assert!(err.contains("missing pricing for model openai/o3"));
    }

    #[test]
    fn nan_pricing_field_is_rejected() {
        let err = PricingCatalog::from_toml_str(
            r#"
                [models."openai/o3"]
                input_per_1k = nan
                output_per_1k = 8.0
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("invalid input_per_1k"));
    }

    #[test]
    fn empty_catalog_is_allowed_for_optional_override_loader() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path();
        std::fs::write(path, "").unwrap();

        let catalog = PricingCatalog::load_from_path(path).unwrap();
        let err = catalog.for_model("openai/o3").unwrap_err().to_string();
        assert!(err.contains("missing pricing for model openai/o3"));
    }
}
