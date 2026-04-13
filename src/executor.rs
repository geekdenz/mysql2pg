use async_trait::async_trait;
use serde::Serialize;
use bytes::BytesMut;
use tokio_postgres::{
    types::{to_sql_checked, Format, IsNull, ToSql, Type},
    NoTls, SimpleQueryMessage,
};

use crate::{config::AppConfig, error::MiddlewareError};

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub row_count: u64,
    pub last_insert_id: u64,
}

#[derive(Debug, Clone)]
pub enum PgParam {
    Null,
    Text(String),
}

#[async_trait]
pub trait PostgresExecutor: Send + Sync {
    async fn execute_sql(&self, sql: &str) -> Result<QueryResult, MiddlewareError>;
    async fn execute_prepared_sql(
        &self,
        sql: &str,
        params: &[PgParam],
    ) -> Result<QueryResult, MiddlewareError>;
    async fn describe_sql(&self, sql: &str) -> Result<Vec<String>, MiddlewareError>;
    async fn describe_prepared_sql(&self, sql: &str) -> Result<Vec<String>, MiddlewareError>;
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
        let client = connect(&self.connection_string).await?;

        let sql_upper = sql.trim_start().to_uppercase();
        let returns_rows = sql_upper.starts_with("SELECT")
            || sql_upper.starts_with("WITH")
            || sql_upper.starts_with("SHOW")
            || sql_upper.starts_with("VALUES");

        let result = if !returns_rows {
            if let Some((table_name, identity_column)) = find_identity_insert_target(&client, sql).await? {
                let wrapped_sql = wrap_insert_returning_sql(sql, &identity_column);
                let row = client
                    .query_one(&wrapped_sql, &[])
                    .await
                    .map_err(|e| MiddlewareError::Execution(format!("statement failed: {}", format_pg_error(&e))))?;
                let row_count = row.try_get::<usize, i64>(0).unwrap_or_default().max(0) as u64;
                let last_insert_id = row.try_get::<usize, i64>(1).unwrap_or_default().max(0) as u64;
                tracing::debug!(
                    "captured insert id for {}.{} => row_count={} last_insert_id={}",
                    table_name,
                    identity_column,
                    row_count,
                    last_insert_id
                );
                Ok(QueryResult {
                    columns: vec![],
                    rows: vec![],
                    row_count,
                    last_insert_id,
                })
            } else {
                let messages = client
                    .simple_query(sql)
                    .await
                    .map_err(|e| MiddlewareError::Execution(format!("statement failed: {}", format_pg_error(&e))))?;
                let affected = messages
                    .into_iter()
                    .filter_map(|message| match message {
                        SimpleQueryMessage::CommandComplete(rows) => Some(rows),
                        _ => None,
                    })
                    .sum();
                Ok(QueryResult {
                    columns: vec![],
                    rows: vec![],
                    row_count: affected,
                    last_insert_id: 0,
                })
            }
        } else {
            let messages = client
                .simple_query(sql)
                .await
                .map_err(|e| MiddlewareError::Execution(format!("query failed: {}", format_pg_error(&e))))?;

            let mut columns = Vec::new();
            let mut rendered_rows = Vec::new();
            for message in messages {
                match message {
                    SimpleQueryMessage::RowDescription(description) if columns.is_empty() => {
                        columns = description.iter().map(|column| column.name().to_string()).collect();
                    }
                    SimpleQueryMessage::Row(row) => {
                        if columns.is_empty() {
                            columns = row.columns().iter().map(|column| column.name().to_string()).collect();
                        }
                        rendered_rows.push(
                            row.columns()
                                .iter()
                                .enumerate()
                                .map(|(idx, _)| row.get(idx).unwrap_or_default().to_string())
                                .collect::<Vec<_>>(),
                        );
                    }
                    _ => {}
                }
            }

            Ok(QueryResult {
                row_count: rendered_rows.len() as u64,
                columns,
                rows: rendered_rows,
                last_insert_id: 0,
            })
        };

        result
    }

    async fn execute_prepared_sql(
        &self,
        sql: &str,
        params: &[PgParam],
    ) -> Result<QueryResult, MiddlewareError> {
        let client = connect(&self.connection_string).await?;
        let statement = client
            .prepare(sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("statement preparation failed: {}", format_pg_error(&e))))?;
        let bind_params = params.iter().map(|param| param as &(dyn ToSql + Sync)).collect::<Vec<_>>();

        let result = if statement.columns().is_empty() {
            if let Some((table_name, identity_column)) = find_identity_insert_target(&client, sql).await? {
                let wrapped_sql = wrap_insert_returning_sql(sql, &identity_column);
                let wrapped_statement = client
                    .prepare(&wrapped_sql)
                    .await
                    .map_err(|e| MiddlewareError::Execution(format!("statement preparation failed: {}", format_pg_error(&e))))?;
                let row = client
                    .query_one(&wrapped_statement, &bind_params)
                    .await
                    .map_err(|e| MiddlewareError::Execution(format!("statement failed: {}", format_pg_error(&e))))?;
                let row_count = row.try_get::<usize, i64>(0).unwrap_or_default().max(0) as u64;
                let last_insert_id = row.try_get::<usize, i64>(1).unwrap_or_default().max(0) as u64;
                tracing::debug!(
                    "captured insert id for {}.{} => row_count={} last_insert_id={}",
                    table_name,
                    identity_column,
                    row_count,
                    last_insert_id
                );
                Ok(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    row_count,
                    last_insert_id,
                })
            } else {
                let affected = client
                    .execute(&statement, &bind_params)
                    .await
                    .map_err(|e| MiddlewareError::Execution(format!("statement failed: {}", format_pg_error(&e))))?;
                Ok(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    row_count: affected,
                    last_insert_id: 0,
                })
            }
        } else {
            let rows = client
                .query(&statement, &bind_params)
                .await
                .map_err(|e| MiddlewareError::Execution(format!("query failed: {}", format_pg_error(&e))))?;
            let columns = statement
                .columns()
                .iter()
                .map(|column| column.name().to_string())
                .collect::<Vec<_>>();
            let rendered_rows = rows
                .iter()
                .map(|row| {
                    row.columns()
                        .iter()
                        .enumerate()
                        .map(|(idx, column)| value_to_string(row, idx, column.type_()))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();

            Ok(QueryResult {
                row_count: rendered_rows.len() as u64,
                columns,
                rows: rendered_rows,
                last_insert_id: 0,
            })
        };

        drop(statement);
        result
    }

    async fn describe_sql(&self, sql: &str) -> Result<Vec<String>, MiddlewareError> {
        let client = connect(&self.connection_string).await?;

        let messages = client
            .simple_query(sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("query description failed: {}", format_pg_error(&e))))?;

        for message in messages {
            match message {
                SimpleQueryMessage::RowDescription(description) => {
                    return Ok(description.iter().map(|column| column.name().to_string()).collect())
                }
                SimpleQueryMessage::Row(row) => {
                    return Ok(row.columns().iter().map(|column| column.name().to_string()).collect())
                }
                _ => {}
            }
        }

        Ok(Vec::new())
    }

    async fn describe_prepared_sql(&self, sql: &str) -> Result<Vec<String>, MiddlewareError> {
        let client = connect(&self.connection_string).await?;
        let statement = client
            .prepare(sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("statement description failed: {}", format_pg_error(&e))))?;

        Ok(statement
            .columns()
            .iter()
            .map(|column| column.name().to_string())
            .collect())
    }
}

impl ToSql for PgParam {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            Self::Null => Ok(IsNull::Yes),
            Self::Text(value) => {
                out.extend_from_slice(value.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

async fn connect(connection_string: &str) -> Result<tokio_postgres::Client, MiddlewareError> {
    let (client, connection) = tokio_postgres::connect(connection_string, NoTls)
        .await
        .map_err(|e| MiddlewareError::Execution(format!("failed to connect to PostgreSQL: {}", format_pg_error(&e))))?;

    tokio::spawn(async move {
        if let Err(err) = connection.await {
            eprintln!("postgres connection error: {err}");
        }
    });

    Ok(client)
}

fn format_pg_error(err: &tokio_postgres::Error) -> String {
    if let Some(db_err) = err.as_db_error() {
        let mut parts = vec![db_err.message().to_string()];

        if let Some(detail) = db_err.detail() {
            parts.push(format!("detail: {detail}"));
        }
        if let Some(hint) = db_err.hint() {
            parts.push(format!("hint: {hint}"));
        }
        if let Some(schema) = db_err.schema() {
            parts.push(format!("schema: {schema}"));
        }
        if let Some(table) = db_err.table() {
            parts.push(format!("table: {table}"));
        }
        if let Some(column) = db_err.column() {
            parts.push(format!("column: {column}"));
        }
        if let Some(constraint) = db_err.constraint() {
            parts.push(format!("constraint: {constraint}"));
        }

        return parts.join(" | ");
    }

    err.to_string()
}

async fn find_identity_insert_target(
    client: &tokio_postgres::Client,
    sql: &str,
) -> Result<Option<(String, String)>, MiddlewareError> {
    let Some(table_name) = extract_insert_table_name(sql) else {
        return Ok(None);
    };

    let row = client
        .query_opt(
            "SELECT a.attname \
             FROM pg_class cls \
             JOIN pg_namespace ns ON ns.oid = cls.relnamespace \
             JOIN pg_attribute a ON a.attrelid = cls.oid AND a.attnum > 0 AND NOT a.attisdropped \
             LEFT JOIN pg_attrdef ad ON ad.adrelid = cls.oid AND ad.adnum = a.attnum \
             WHERE ns.nspname = current_schema() \
               AND cls.relname = $1 \
               AND (a.attidentity IN ('a','d') OR coalesce(pg_get_expr(ad.adbin, ad.adrelid), '') LIKE 'nextval(%') \
             ORDER BY a.attnum \
             LIMIT 1",
            &[&table_name],
        )
        .await
        .map_err(|e| MiddlewareError::Execution(format!("identity column lookup failed: {}", format_pg_error(&e))))?;

    Ok(row.map(|row| (table_name, row.get::<usize, String>(0))))
}

fn extract_insert_table_name(sql: &str) -> Option<String> {
    let pattern = regex::Regex::new(
        r#"(?is)^\s*INSERT\s+INTO\s+(?:"([^"]+)"|([A-Za-z_][A-Za-z0-9_]*))(?:\s|\(|$)"#,
    )
    .expect("valid insert table regex");
    let caps = pattern.captures(sql)?;
    caps.get(1)
        .or_else(|| caps.get(2))
        .map(|m| m.as_str().to_string())
}

fn wrap_insert_returning_sql(sql: &str, identity_column: &str) -> String {
    format!(
        "WITH inserted_rows AS ({sql} RETURNING \"{identity_column}\") \
         SELECT COUNT(*)::BIGINT AS __mw_row_count__, COALESCE(MAX(\"{identity_column}\"), 0)::BIGINT AS __mw_last_insert_id__ \
         FROM inserted_rows"
    )
}

#[allow(dead_code)]
fn value_to_string(row: &tokio_postgres::Row, idx: usize, ty: &Type) -> String {
    match *ty {
        Type::BOOL => row.try_get::<usize, Option<bool>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::INT2 => row.try_get::<usize, Option<i16>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::INT4 => row.try_get::<usize, Option<i32>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::INT8 => row.try_get::<usize, Option<i64>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::FLOAT4 => row.try_get::<usize, Option<f32>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::FLOAT8 => row.try_get::<usize, Option<f64>>(idx).ok().flatten().map(|v| v.to_string()).unwrap_or_default(),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => row.try_get::<usize, Option<String>>(idx).ok().flatten().unwrap_or_default(),
        Type::JSON | Type::JSONB => row
            .try_get::<usize, Option<serde_json::Value>>(idx)
            .ok()
            .flatten()
            .map(|v| v.to_string())
            .unwrap_or_default(),
        Type::BYTEA => row
            .try_get::<usize, Option<Vec<u8>>>(idx)
            .ok()
            .flatten()
            .map(|bytes| format!("\\x{}", bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>()))
            .unwrap_or_default(),
        _ => row
            .try_get::<usize, Option<String>>(idx)
            .ok()
            .flatten()
            .unwrap_or_else(|| "<unrendered>".to_string()),
    }
}

pub fn build_executor(cfg: &AppConfig) -> Result<std::sync::Arc<dyn PostgresExecutor>, MiddlewareError> {
    match cfg.postgres.driver.as_str() {
        "tokio-postgres" => Ok(std::sync::Arc::new(TokioPostgresExecutor::new(
            cfg.postgres.connection_string.clone(),
        ))),
        other => Err(MiddlewareError::Config(format!(
            "unsupported postgres driver `{other}`; currently supported: tokio-postgres"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::extract_insert_table_name;

    #[test]
    fn extract_insert_table_name_supports_quoted_identifiers() {
        assert_eq!(
            extract_insert_table_name(r#"INSERT INTO "site" ("name") VALUES ($1)"#).as_deref(),
            Some("site")
        );
    }

    #[test]
    fn extract_insert_table_name_supports_unquoted_identifiers() {
        assert_eq!(
            extract_insert_table_name("INSERT INTO site (name) VALUES ($1)").as_deref(),
            Some("site")
        );
    }
}
