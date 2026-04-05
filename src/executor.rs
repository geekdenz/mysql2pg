use async_trait::async_trait;
use serde::Serialize;
use tokio_postgres::{types::Type, NoTls};

use crate::{config::AppConfig, error::MiddlewareError};

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub row_count: u64,
}

#[async_trait]
pub trait PostgresExecutor: Send + Sync {
    async fn execute_sql(&self, sql: &str) -> Result<QueryResult, MiddlewareError>;
}

pub struct TokioPostgresExecutor {
    connection_string: String,
}

impl TokioPostgresExecutor {
    pub fn new(connection_string: String) -> Self {
        Self { connection_string }
    }
}

#[async_trait]
impl PostgresExecutor for TokioPostgresExecutor {
    async fn execute_sql(&self, sql: &str) -> Result<QueryResult, MiddlewareError> {
        let (client, connection) = tokio_postgres::connect(&self.connection_string, NoTls)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("failed to connect to PostgreSQL: {e}")))?;

        tokio::spawn(async move {
            if let Err(err) = connection.await {
                eprintln!("postgres connection error: {err}");
            }
        });

        let sql_upper = sql.trim_start().to_uppercase();
        let returns_rows = sql_upper.starts_with("SELECT")
            || sql_upper.starts_with("WITH")
            || sql_upper.starts_with("SHOW")
            || sql_upper.starts_with("VALUES");

        if !returns_rows {
            let affected = client
                .execute(sql, &[])
                .await
                .map_err(|e| MiddlewareError::Execution(format!("statement failed: {e}")))?;
            return Ok(QueryResult {
                columns: vec![],
                rows: vec![],
                row_count: affected,
            });
        }

        let rows = client
            .query(sql, &[])
            .await
            .map_err(|e| MiddlewareError::Execution(format!("query failed: {e}")))?;

        if rows.is_empty() {
            return Ok(QueryResult {
                columns: vec![],
                rows: vec![],
                row_count: 0,
            });
        }

        let columns = rows[0]
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect::<Vec<_>>();

        let row_count = rows.len() as u64;
        let rendered_rows = rows
            .iter()
            .map(|row| {
                row.columns()
                    .iter()
                    .enumerate()
                    .map(|(idx, col)| value_to_string(row, idx, col.type_()))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        Ok(QueryResult {
            columns,
            rows: rendered_rows,
            row_count,
        })
    }
}

fn value_to_string(row: &tokio_postgres::Row, idx: usize, ty: &Type) -> String {
    match *ty {
        Type::BOOL => row.try_get::<usize, Option<bool>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::INT2 => row.try_get::<usize, Option<i16>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::INT4 => row.try_get::<usize, Option<i32>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::INT8 => row.try_get::<usize, Option<i64>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::FLOAT4 => row.try_get::<usize, Option<f32>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::FLOAT8 => row.try_get::<usize, Option<f64>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => row.try_get::<usize, Option<String>>(idx).ok().flatten().unwrap_or_default(),
        Type::JSON | Type::JSONB => row.try_get::<usize, Option<serde_json::Value>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        _ => row.try_get::<usize, Option<String>>(idx).ok().flatten().unwrap_or_else(|| "<unrendered>".to_string()),
    }
}

pub fn build_executor(cfg: &AppConfig) -> Result<Box<dyn PostgresExecutor>, MiddlewareError> {
    match cfg.postgres.driver.as_str() {
        "tokio-postgres" => Ok(Box::new(TokioPostgresExecutor::new(
            cfg.postgres.connection_string.clone(),
        ))),
        other => Err(MiddlewareError::Config(format!(
            "unsupported postgres driver `{other}`; currently supported: tokio-postgres"
        ))),
    }
}
