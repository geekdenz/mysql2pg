use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::error::MiddlewareError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub postgres: PostgresConfig,
    #[serde(default)]
    pub translator: TranslatorConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresConfig {
    #[serde(default = "default_driver")]
    pub driver: String,
    pub connection_string: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslatorConfig {
    #[serde(default = "default_rewrite_limit_comma")]
    pub rewrite_limit_comma: bool,
    #[serde(default = "default_mysql_backticks")]
    pub normalize_mysql_backticks: bool,
    #[serde(default = "default_boolean_literals")]
    pub normalize_boolean_literals: bool,
    #[serde(default = "default_mysql_functions")]
    pub rewrite_mysql_functions: bool,
    #[serde(default = "default_json_operators")]
    pub rewrite_json_operators: bool,
    #[serde(default = "default_strip_engine_clauses")]
    pub strip_mysql_table_options: bool,
}

fn default_driver() -> String {
    "tokio-postgres".to_string()
}

fn default_rewrite_limit_comma() -> bool { true }
fn default_mysql_backticks() -> bool { true }
fn default_boolean_literals() -> bool { true }
fn default_mysql_functions() -> bool { true }
fn default_json_operators() -> bool { true }
fn default_strip_engine_clauses() -> bool { true }

impl Default for TranslatorConfig {
    fn default() -> Self {
        Self {
            rewrite_limit_comma: true,
            normalize_mysql_backticks: true,
            normalize_boolean_literals: true,
            rewrite_mysql_functions: true,
            rewrite_json_operators: true,
            strip_mysql_table_options: true,
        }
    }
}

impl AppConfig {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, MiddlewareError> {
        let content = fs::read_to_string(path.as_ref())
            .map_err(|e| MiddlewareError::Config(format!("failed to read config: {e}")))?;
        toml::from_str(&content)
            .map_err(|e| MiddlewareError::Config(format!("failed to parse TOML config: {e}")))
    }
}
