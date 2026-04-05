use serde::{Deserialize, Serialize};
use std::{env, fs, path::Path};

use crate::error::MiddlewareError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    pub postgres: PostgresConfig,
    #[serde(default)]
    pub translator: TranslatorConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
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

fn default_bind_addr() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_driver() -> String {
    "tokio-postgres".to_string()
}

fn default_rewrite_limit_comma() -> bool {
    true
}
fn default_mysql_backticks() -> bool {
    true
}
fn default_boolean_literals() -> bool {
    true
}
fn default_mysql_functions() -> bool {
    true
}
fn default_json_operators() -> bool {
    true
}
fn default_strip_engine_clauses() -> bool {
    true
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
        }
    }
}

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
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MiddlewareError> {
        let mut cfg = if path.as_ref().exists() {
            let content = fs::read_to_string(path.as_ref())
                .map_err(|e| MiddlewareError::Config(format!("failed to read config: {e}")))?;
            toml::from_str(&content).map_err(|e| {
                MiddlewareError::Config(format!("failed to parse TOML config: {e}"))
            })?
        } else {
            Self::from_env()?
        };

        cfg.apply_env_overrides()?;
        Ok(cfg)
    }

    pub fn from_env() -> Result<Self, MiddlewareError> {
        let connection_string = env::var("MW_POSTGRES_CONNECTION_STRING")
            .map_err(|_| MiddlewareError::Config("MW_POSTGRES_CONNECTION_STRING is required when no config file is provided".to_string()))?;

        let driver = env::var("MW_POSTGRES_DRIVER").unwrap_or_else(|_| default_driver());
        let bind_addr = env::var("MW_SERVER_BIND_ADDR").unwrap_or_else(|_| default_bind_addr());

        Ok(Self {
            server: ServerConfig { bind_addr },
            postgres: PostgresConfig {
                driver,
                connection_string,
            },
            translator: TranslatorConfig {
                rewrite_limit_comma: env_bool("MW_TRANSLATOR_REWRITE_LIMIT_COMMA", default_rewrite_limit_comma()),
                normalize_mysql_backticks: env_bool(
                    "MW_TRANSLATOR_NORMALIZE_MYSQL_BACKTICKS",
                    default_mysql_backticks(),
                ),
                normalize_boolean_literals: env_bool(
                    "MW_TRANSLATOR_NORMALIZE_BOOLEAN_LITERALS",
                    default_boolean_literals(),
                ),
                rewrite_mysql_functions: env_bool(
                    "MW_TRANSLATOR_REWRITE_MYSQL_FUNCTIONS",
                    default_mysql_functions(),
                ),
                rewrite_json_operators: env_bool(
                    "MW_TRANSLATOR_REWRITE_JSON_OPERATORS",
                    default_json_operators(),
                ),
                strip_mysql_table_options: env_bool(
                    "MW_TRANSLATOR_STRIP_MYSQL_TABLE_OPTIONS",
                    default_strip_engine_clauses(),
                ),
            },
        })
    }

    pub fn apply_env_overrides(&mut self) -> Result<(), MiddlewareError> {
        if let Ok(bind_addr) = env::var("MW_SERVER_BIND_ADDR") {
            self.server.bind_addr = bind_addr;
        }
        if let Ok(driver) = env::var("MW_POSTGRES_DRIVER") {
            self.postgres.driver = driver;
        }
        if let Ok(connection_string) = env::var("MW_POSTGRES_CONNECTION_STRING") {
            self.postgres.connection_string = connection_string;
        }
        self.translator.rewrite_limit_comma = env_bool(
            "MW_TRANSLATOR_REWRITE_LIMIT_COMMA",
            self.translator.rewrite_limit_comma,
        );
        self.translator.normalize_mysql_backticks = env_bool(
            "MW_TRANSLATOR_NORMALIZE_MYSQL_BACKTICKS",
            self.translator.normalize_mysql_backticks,
        );
        self.translator.normalize_boolean_literals = env_bool(
            "MW_TRANSLATOR_NORMALIZE_BOOLEAN_LITERALS",
            self.translator.normalize_boolean_literals,
        );
        self.translator.rewrite_mysql_functions = env_bool(
            "MW_TRANSLATOR_REWRITE_MYSQL_FUNCTIONS",
            self.translator.rewrite_mysql_functions,
        );
        self.translator.rewrite_json_operators = env_bool(
            "MW_TRANSLATOR_REWRITE_JSON_OPERATORS",
            self.translator.rewrite_json_operators,
        );
        self.translator.strip_mysql_table_options = env_bool(
            "MW_TRANSLATOR_STRIP_MYSQL_TABLE_OPTIONS",
            self.translator.strip_mysql_table_options,
        );

        if self.postgres.connection_string.trim().is_empty() {
            return Err(MiddlewareError::Config(
                "PostgreSQL connection string cannot be empty".to_string(),
            ));
        }

        Ok(())
    }
}

fn env_bool(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(value) => matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
}
