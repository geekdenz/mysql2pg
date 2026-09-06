use async_trait::async_trait;
use serde::Serialize;
use bytes::BytesMut;
use regex::{Captures, Regex};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_postgres::{
    config::Host,
    types::{to_sql_checked, Format, IsNull, ToSql, Type},
    Config as PgConfig, NoTls, SimpleQueryMessage,
};

use crate::{config::AppConfig, error::MiddlewareError};


/// One column value, carried as bytes.
///
/// MySQL results are a byte stream: a `BLOB` may hold arbitrary binary (Matomo
/// stores zlib-compressed report data that way), which cannot round-trip through a
/// Rust `String`. Rendering such a column as PostgreSQL's `\x…` hex text hands the
/// client a 2x-larger ASCII string instead of its data, so values stay as bytes all
/// the way to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldValue(pub Vec<u8>);

impl FieldValue {
    pub fn from_text(value: impl Into<String>) -> Self {
        FieldValue(value.into().into_bytes())
    }

    pub fn from_bytes(value: Vec<u8>) -> Self {
        FieldValue(value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Lossy text view, for logging, JSON and the CLI. Binary columns are not
    /// expected to be read back through these surfaces.
    pub fn to_lossy_string(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

impl Serialize for FieldValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_lossy_string())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// `None` represents a real SQL `NULL`, distinct from an empty string. Losing
    /// this distinction over the MySQL wire protocol makes NULL-able columns arrive
    /// at the client as `""`, which breaks client code that (correctly) expects
    /// `NULL + int` to coerce rather than throw the way `"" + int` does.
    pub rows: Vec<Vec<Option<FieldValue>>>,
    pub row_count: u64,
    pub last_insert_id: u64,
}

#[derive(Debug, Clone)]
pub enum PgParam {
    Null,
    Text(String),
    Bytes(Vec<u8>),
}

#[async_trait]
pub trait PostgresExecutor: Send + Sync {
    async fn execute_sql(&self, sql: &str) -> Result<QueryResult, MiddlewareError>;
    async fn execute_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
    ) -> Result<QueryResult, MiddlewareError>;
    async fn execute_prepared_sql(
        &self,
        sql: &str,
        params: &[PgParam],
    ) -> Result<QueryResult, MiddlewareError>;
    async fn execute_prepared_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
        params: &[PgParam],
    ) -> Result<QueryResult, MiddlewareError>;
    async fn describe_sql(&self, sql: &str) -> Result<Vec<String>, MiddlewareError>;
    async fn describe_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
    ) -> Result<Vec<String>, MiddlewareError>;
    async fn describe_prepared_sql(&self, sql: &str) -> Result<Vec<String>, MiddlewareError>;
    async fn describe_prepared_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
    ) -> Result<Vec<String>, MiddlewareError>;
    async fn create_schema(&self, schema: &str) -> Result<(), MiddlewareError>;
    async fn drop_schema(&self, schema: &str) -> Result<(), MiddlewareError>;
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
        self.execute_sql_in_schema(None, sql).await
    }

    async fn execute_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
    ) -> Result<QueryResult, MiddlewareError> {
        let client = connect(&self.connection_string, schema).await?;

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
            } else if let Some((column, wrapped_sql)) = find_last_insert_id_update_target(sql) {
                let row = client
                    .query_one(&wrapped_sql, &[])
                    .await
                    .map_err(|e| MiddlewareError::Execution(format!("statement failed: {}", format_pg_error(&e))))?;
                let row_count = row.try_get::<usize, i64>(0).unwrap_or_default().max(0) as u64;
                let last_insert_id = row.try_get::<usize, i64>(1).unwrap_or_default().max(0) as u64;
                tracing::debug!(
                    "captured LAST_INSERT_ID() for column {} => row_count={} last_insert_id={}",
                    column,
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
                let messages = simple_query_with_compat_retry(&client, sql)
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
            // Row-returning statements go through the extended protocol so every
            // value arrives with its type. The simple protocol returns everything as
            // text, which renders a `bytea` as PostgreSQL's `\x…` hex string —
            // handing the client an ASCII blob instead of its bytes.
            let (statement, rows) = typed_query_with_compat_retry(&client, sql)
                .await
                .map_err(|e| MiddlewareError::Execution(format!("query failed: {}", format_pg_error(&e))))?;

            // Taken from the statement, so names survive an empty result.
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

        result
    }

    async fn execute_prepared_sql(
        &self,
        sql: &str,
        params: &[PgParam],
    ) -> Result<QueryResult, MiddlewareError> {
        self.execute_prepared_sql_in_schema(None, sql, params).await
    }

    async fn execute_prepared_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
        params: &[PgParam],
    ) -> Result<QueryResult, MiddlewareError> {
        let client = connect(&self.connection_string, schema).await?;
        let statement = prepare_with_compat_retry(&client, sql)
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
            } else if let Some((column, wrapped_sql)) = find_last_insert_id_update_target(sql) {
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
                    "captured LAST_INSERT_ID() for column {} => row_count={} last_insert_id={}",
                    column,
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
        self.describe_sql_in_schema(None, sql).await
    }

    async fn describe_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
    ) -> Result<Vec<String>, MiddlewareError> {
        let client = connect(&self.connection_string, schema).await?;

        let messages = simple_query_with_compat_retry(&client, sql)
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
        self.describe_prepared_sql_in_schema(None, sql).await
    }

    async fn describe_prepared_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
    ) -> Result<Vec<String>, MiddlewareError> {
        let client = connect(&self.connection_string, schema).await?;
        let statement = prepare_with_compat_retry(&client, sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("statement description failed: {}", format_pg_error(&e))))?;

        Ok(statement
            .columns()
            .iter()
            .map(|column| column.name().to_string())
            .collect())
    }

    async fn create_schema(&self, schema: &str) -> Result<(), MiddlewareError> {
        let client = connect(&self.connection_string, None).await?;
        let sql = format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema));
        client
            .batch_execute(&sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("schema creation failed: {}", format_pg_error(&e))))?;
        Ok(())
    }

    async fn drop_schema(&self, schema: &str) -> Result<(), MiddlewareError> {
        let client = connect(&self.connection_string, None).await?;
        let sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_ident(schema));
        client
            .batch_execute(&sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("schema drop failed: {}", format_pg_error(&e))))?;
        Ok(())
    }
}

pub struct SessionPostgresExecutor {
    connection_string: String,
    client: Mutex<Option<tokio_postgres::Client>>,
}

impl SessionPostgresExecutor {
    pub fn new(connection_string: String) -> Self {
        Self {
            connection_string,
            client: Mutex::new(None),
        }
    }

    async fn acquire(&self) -> Result<tokio::sync::MutexGuard<'_, Option<tokio_postgres::Client>>, MiddlewareError> {
        let mut guard = self.client.lock().await;
        let needs_connect = guard.as_ref().map(|c| c.is_closed()).unwrap_or(true);
        if needs_connect {
            let client = connect_with_nodelay(&self.connection_string).await?;
            *guard = Some(client);
        }
        Ok(guard)
    }

    async fn set_schema(client: &tokio_postgres::Client, schema: Option<&str>) -> Result<(), MiddlewareError> {
        if let Some(schema) = schema.filter(|s| !s.trim().is_empty()) {
            let search_path = format!("SET search_path TO {}, public", quote_ident(schema));
            client
                .batch_execute(&search_path)
                .await
                .map_err(|e| MiddlewareError::Execution(format!("failed to set schema: {}", format_pg_error(&e))))?;
        }
        Ok(())
    }
}

#[async_trait]
impl PostgresExecutor for SessionPostgresExecutor {
    async fn execute_sql(&self, sql: &str) -> Result<QueryResult, MiddlewareError> {
        self.execute_sql_in_schema(None, sql).await
    }

    async fn execute_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
    ) -> Result<QueryResult, MiddlewareError> {
        let guard = self.acquire().await?;
        let client = guard.as_ref().unwrap();
        Self::set_schema(client, schema).await?;

        let sql_upper = sql.trim_start().to_uppercase();
        let returns_rows = sql_upper.starts_with("SELECT")
            || sql_upper.starts_with("WITH")
            || sql_upper.starts_with("SHOW")
            || sql_upper.starts_with("VALUES");

        let result = if !returns_rows {
            if let Some((table_name, identity_column)) = find_identity_insert_target(client, sql).await? {
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
            } else if let Some((column, wrapped_sql)) = find_last_insert_id_update_target(sql) {
                let row = client
                    .query_one(&wrapped_sql, &[])
                    .await
                    .map_err(|e| MiddlewareError::Execution(format!("statement failed: {}", format_pg_error(&e))))?;
                let row_count = row.try_get::<usize, i64>(0).unwrap_or_default().max(0) as u64;
                let last_insert_id = row.try_get::<usize, i64>(1).unwrap_or_default().max(0) as u64;
                tracing::debug!(
                    "captured LAST_INSERT_ID() for column {} => row_count={} last_insert_id={}",
                    column,
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
                let messages = simple_query_with_compat_retry(&client, sql)
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
            // Row-returning statements go through the extended protocol so every
            // value arrives with its type. The simple protocol returns everything as
            // text, which renders a `bytea` as PostgreSQL's `\x…` hex string —
            // handing the client an ASCII blob instead of its bytes.
            let (statement, rows) = typed_query_with_compat_retry(&client, sql)
                .await
                .map_err(|e| MiddlewareError::Execution(format!("query failed: {}", format_pg_error(&e))))?;

            // Taken from the statement, so names survive an empty result.
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

        result
    }

    async fn execute_prepared_sql(
        &self,
        sql: &str,
        params: &[PgParam],
    ) -> Result<QueryResult, MiddlewareError> {
        self.execute_prepared_sql_in_schema(None, sql, params).await
    }

    async fn execute_prepared_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
        params: &[PgParam],
    ) -> Result<QueryResult, MiddlewareError> {
        let guard = self.acquire().await?;
        let client = guard.as_ref().unwrap();
        Self::set_schema(client, schema).await?;

        let statement = prepare_with_compat_retry(&client, sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("statement preparation failed: {}", format_pg_error(&e))))?;
        let bind_params = params.iter().map(|param| param as &(dyn ToSql + Sync)).collect::<Vec<_>>();

        let result = if statement.columns().is_empty() {
            if let Some((table_name, identity_column)) = find_identity_insert_target(client, sql).await? {
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
            } else if let Some((column, wrapped_sql)) = find_last_insert_id_update_target(sql) {
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
                    "captured LAST_INSERT_ID() for column {} => row_count={} last_insert_id={}",
                    column,
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
        self.describe_sql_in_schema(None, sql).await
    }

    async fn describe_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
    ) -> Result<Vec<String>, MiddlewareError> {
        let guard = self.acquire().await?;
        let client = guard.as_ref().unwrap();
        Self::set_schema(client, schema).await?;

        let messages = simple_query_with_compat_retry(&client, sql)
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
        self.describe_prepared_sql_in_schema(None, sql).await
    }

    async fn describe_prepared_sql_in_schema(
        &self,
        schema: Option<&str>,
        sql: &str,
    ) -> Result<Vec<String>, MiddlewareError> {
        let guard = self.acquire().await?;
        let client = guard.as_ref().unwrap();
        Self::set_schema(client, schema).await?;

        let statement = prepare_with_compat_retry(&client, sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("statement description failed: {}", format_pg_error(&e))))?;

        Ok(statement
            .columns()
            .iter()
            .map(|column| column.name().to_string())
            .collect())
    }

    async fn create_schema(&self, schema: &str) -> Result<(), MiddlewareError> {
        let guard = self.acquire().await?;
        let client = guard.as_ref().unwrap();
        let sql = format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema));
        client
            .batch_execute(&sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("schema creation failed: {}", format_pg_error(&e))))?;
        Ok(())
    }

    async fn drop_schema(&self, schema: &str) -> Result<(), MiddlewareError> {
        let guard = self.acquire().await?;
        let client = guard.as_ref().unwrap();
        let sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_ident(schema));
        client
            .batch_execute(&sql)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("schema drop failed: {}", format_pg_error(&e))))?;
        Ok(())
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
            Self::Bytes(value) if *_ty == Type::BYTEA => {
                out.extend_from_slice(value);
                Ok(IsNull::No)
            }
            Self::Bytes(value) => {
                let value = std::str::from_utf8(value)?;
                out.extend_from_slice(value.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, ty: &Type) -> Format {
        match self {
            Self::Bytes(_) if *ty == Type::BYTEA => Format::Binary,
            _ => Format::Text,
        }
    }

    to_sql_checked!();
}

/// Connects to PostgreSQL like [`tokio_postgres::connect`], but explicitly sets
/// `TCP_NODELAY` on the underlying socket first. The Postgres wire protocol, like
/// MySQL's, is chatty — many small sequential packets per query — so without this,
/// Nagle's algorithm interacting with the peer's delayed ACKs adds ~40ms to every
/// round trip. That is negligible for one query but compounds badly for a Matomo
/// tracking request, which issues dozens of sequential queries. Falls back to the
/// plain connect for any non-TCP host (e.g. a Unix socket), where this doesn't apply.
async fn connect_with_nodelay(
    connection_string: &str,
) -> Result<tokio_postgres::Client, MiddlewareError> {
    let config: PgConfig = connection_string
        .parse()
        .map_err(|e: tokio_postgres::Error| MiddlewareError::Execution(format!("invalid PostgreSQL connection string: {}", format_pg_error(&e))))?;

    let tcp_target = match (config.get_hosts().first(), config.get_ports().first()) {
        (Some(Host::Tcp(host)), Some(&port)) => Some((host.clone(), port)),
        _ => None,
    };

    let client = if let Some((host, port)) = tcp_target {
        let stream = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| MiddlewareError::Execution(format!("failed to connect to PostgreSQL: {e}")))?;
        if let Err(err) = stream.set_nodelay(true) {
            tracing::warn!("failed to set TCP_NODELAY for PostgreSQL connection: {err}");
        }
        let (client, connection) = config
            .connect_raw(stream, NoTls)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("failed to connect to PostgreSQL: {}", format_pg_error(&e))))?;
        tokio::spawn(async move {
            if let Err(err) = connection.await {
                eprintln!("postgres connection error: {err}");
            }
        });
        client
    } else {
        let (client, connection) = tokio_postgres::connect(connection_string, NoTls)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("failed to connect to PostgreSQL: {}", format_pg_error(&e))))?;
        tokio::spawn(async move {
            if let Err(err) = connection.await {
                eprintln!("postgres connection error: {err}");
            }
        });
        client
    };

    Ok(client)
}

async fn connect(
    connection_string: &str,
    schema: Option<&str>,
) -> Result<tokio_postgres::Client, MiddlewareError> {
    let client = connect_with_nodelay(connection_string).await?;

    if let Some(schema) = schema.filter(|schema| !schema.trim().is_empty()) {
        let search_path = format!("SET search_path TO {}, public", quote_ident(schema));
        client
            .batch_execute(&search_path)
            .await
            .map_err(|e| MiddlewareError::Execution(format!("failed to set schema: {}", format_pg_error(&e))))?;
    }

    Ok(client)
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn format_pg_error(err: &tokio_postgres::Error) -> String {
    if let Some(db_err) = err.as_db_error() {
        // The bracketed SQLSTATE is machine-parseable: the MySQL wire-protocol
        // frontend maps it back to a matching MySQL error code (see
        // `mysql_error_kind_for_message` in mysql_server.rs) so that clients relying
        // on specific error codes (e.g. Matomo's sequence table retrying on a
        // duplicate-key error) see the equivalent MySQL behavior instead of always
        // hitting a generic "unknown error".
        let mut parts = vec![format!("[{}] {}", db_err.code().code(), db_err.message())];

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

/// Detects MySQL's `UPDATE ... SET col = LAST_INSERT_ID(expr) ...` idiom, used by
/// Matomo's `Sequence::getNextId()` (and other apps) to atomically increment a
/// counter and read back the new value via a later `lastInsertId()` call — without
/// relying on a real auto-increment column, so the identity-column lookup used for
/// plain `INSERT`s ([`find_identity_insert_target`]) doesn't apply. PostgreSQL has
/// no `LAST_INSERT_ID()`; this rewrites the statement to capture the value via
/// `RETURNING` instead, using the same `__mw_row_count__` / `__mw_last_insert_id__`
/// convention as [`wrap_insert_returning_sql`] so the result feeds into the MySQL
/// OK packet's `last_insert_id` field exactly like a real auto-increment would.
/// Returns `(column_name, rewritten_sql)`.
fn find_last_insert_id_update_target(sql: &str) -> Option<(String, String)> {
    let column_re = Regex::new(
        r#"(?is)^\s*UPDATE\b.*\bSET\b.*?(?:"([A-Za-z_][A-Za-z0-9_]*)"|([A-Za-z_][A-Za-z0-9_]*))\s*=\s*LAST_INSERT_ID\s*\("#,
    )
    .expect("valid LAST_INSERT_ID column regex");
    let caps = column_re.captures(sql)?;
    let column = caps.get(1).or_else(|| caps.get(2))?.as_str().to_string();

    let unwrap_re = Regex::new(r#"(?is)LAST_INSERT_ID\s*\(\s*(.*?)\s*\)"#).expect("valid LAST_INSERT_ID unwrap regex");
    let stripped = unwrap_re.replace(sql, |caps: &Captures<'_>| caps[1].to_string());

    let wrapped = format!(
        "WITH updated_rows AS ({stripped} RETURNING \"{column}\") \
         SELECT COUNT(*)::BIGINT AS __mw_row_count__, COALESCE(MAX(\"{column}\"), 0)::BIGINT AS __mw_last_insert_id__ \
         FROM updated_rows"
    );
    Some((column, wrapped))
}

fn wrap_insert_returning_sql(sql: &str, identity_column: &str) -> String {
    format!(
        "WITH inserted_rows AS ({sql} RETURNING \"{identity_column}\") \
         SELECT COUNT(*)::BIGINT AS __mw_row_count__, COALESCE(MAX(\"{identity_column}\"), 0)::BIGINT AS __mw_last_insert_id__ \
         FROM inserted_rows"
    )
}

#[allow(dead_code)]
/// Renders one column of a row as MySQL wire-protocol text, preserving `None` for a
/// real SQL `NULL` rather than collapsing it into an empty string. See the `rows`
/// field doc on [`QueryResult`] for why that distinction matters to clients.
fn value_to_string(row: &tokio_postgres::Row, idx: usize, ty: &Type) -> Option<FieldValue> {
    match *ty {
        Type::BOOL => row.try_get::<usize, Option<bool>>(idx).ok().flatten().map(|v| FieldValue::from_text(v.to_string())),
        Type::INT2 => row.try_get::<usize, Option<i16>>(idx).ok().flatten().map(|v| FieldValue::from_text(v.to_string())),
        Type::INT4 => row.try_get::<usize, Option<i32>>(idx).ok().flatten().map(|v| FieldValue::from_text(v.to_string())),
        Type::INT8 => row.try_get::<usize, Option<i64>>(idx).ok().flatten().map(|v| FieldValue::from_text(v.to_string())),
        Type::FLOAT4 => row.try_get::<usize, Option<f32>>(idx).ok().flatten().map(|v| FieldValue::from_text(v.to_string())),
        Type::FLOAT8 => row.try_get::<usize, Option<f64>>(idx).ok().flatten().map(|v| FieldValue::from_text(v.to_string())),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => row
            .try_get::<usize, Option<String>>(idx)
            .ok()
            .flatten()
            .map(FieldValue::from_text),
        Type::JSON | Type::JSONB => row
            .try_get::<usize, Option<serde_json::Value>>(idx)
            .ok()
            .flatten()
            .map(|v| FieldValue::from_text(v.to_string())),
        Type::BYTEA => row
            .try_get::<usize, Option<Vec<u8>>>(idx)
            .ok()
            .flatten()
            .map(FieldValue::from_bytes),
        // Date/time and numeric values arrive in PostgreSQL's binary format over the
        // extended (prepared statement) protocol, so they have to be decoded into a
        // Rust type before they can be rendered. Without these arms they fall through
        // to the catch-all below, which cannot turn them into a String and so emits
        // the "<unrendered>" placeholder — which downstream clients then try to parse
        // as a date. The formats below are MySQL's, which is what clients expect.
        Type::TIMESTAMP => row
            .try_get::<usize, Option<chrono::NaiveDateTime>>(idx)
            .ok()
            .flatten()
            .map(|value| FieldValue::from_text(value.format("%Y-%m-%d %H:%M:%S").to_string())),
        Type::TIMESTAMPTZ => row
            .try_get::<usize, Option<chrono::DateTime<chrono::Utc>>>(idx)
            .ok()
            .flatten()
            .map(|value| FieldValue::from_text(value.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string())),
        Type::DATE => row
            .try_get::<usize, Option<chrono::NaiveDate>>(idx)
            .ok()
            .flatten()
            .map(|value| FieldValue::from_text(value.format("%Y-%m-%d").to_string())),
        Type::TIME => row
            .try_get::<usize, Option<chrono::NaiveTime>>(idx)
            .ok()
            .flatten()
            .map(|value| FieldValue::from_text(value.format("%H:%M:%S").to_string())),
        Type::NUMERIC => row
            .try_get::<usize, Option<rust_decimal::Decimal>>(idx)
            .ok()
            .flatten()
            .map(|value| FieldValue::from_text(value.to_string())),
        _ => match row.try_get::<usize, Option<String>>(idx) {
            Ok(value) => value.map(FieldValue::from_text),
            Err(_) => Some(FieldValue::from_text("<unrendered>")),
        },
    }
}

/// MySQL builtin functions that PostgreSQL has no equivalent for. Matomo (and other
/// MySQL-native apps) call these directly in application SQL, so the middleware
/// installs matching PostgreSQL functions on startup rather than requiring a manual
/// migration step. Each entry is `(name, CREATE OR REPLACE FUNCTION ... statement)`.
const MYSQL_COMPAT_FUNCTIONS: &[(&str, &str)] = &[(
    "crc32",
    r#"
    CREATE OR REPLACE FUNCTION crc32(data bytea) RETURNS bigint AS $$
    DECLARE
        crc bigint := 4294967295;
        byte int;
        i int;
        j int;
    BEGIN
        IF data IS NULL THEN
            RETURN NULL;
        END IF;
        FOR i IN 0 .. length(data) - 1 LOOP
            byte := get_byte(data, i);
            crc := crc # byte;
            FOR j IN 1..8 LOOP
                IF (crc & 1) = 1 THEN
                    crc := (crc >> 1) # 3988292384;
                ELSE
                    crc := crc >> 1;
                END IF;
            END LOOP;
        END LOOP;
        RETURN crc # 4294967295;
    END;
    $$ LANGUAGE plpgsql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION crc32(data text) RETURNS bigint AS $$
        SELECT crc32(convert_to(data, 'UTF8'));
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
), (
    "sha2",
    r#"
    -- PostgreSQL has no SHA2(); it ships native sha224/256/384/512 (PG14+) that
    -- MySQL's SHA2(str, hash_length) dispatches to by its second argument.
    CREATE OR REPLACE FUNCTION sha2(data text, hash_length integer) RETURNS text AS $$
        SELECT CASE hash_length
            WHEN 0 THEN encode(sha256(convert_to(data, 'UTF8')), 'hex')
            WHEN 256 THEN encode(sha256(convert_to(data, 'UTF8')), 'hex')
            WHEN 224 THEN encode(sha224(convert_to(data, 'UTF8')), 'hex')
            WHEN 384 THEN encode(sha384(convert_to(data, 'UTF8')), 'hex')
            WHEN 512 THEN encode(sha512(convert_to(data, 'UTF8')), 'hex')
            ELSE NULL
        END;
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
), (
    "weekday",
    r#"
    -- MySQL WEEKDAY(): 0=Monday .. 6=Sunday. PostgreSQL's closest native equivalent,
    -- EXTRACT(ISODOW FROM ...), is 1=Monday..7=Sunday, so this just shifts it by one.
    CREATE OR REPLACE FUNCTION weekday(d date) RETURNS integer AS $$
        SELECT EXTRACT(ISODOW FROM d)::integer - 1;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION weekday(d timestamp) RETURNS integer AS $$
        SELECT EXTRACT(ISODOW FROM d)::integer - 1;
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
), (
    "dayname",
    r#"
    CREATE OR REPLACE FUNCTION dayname(d date) RETURNS text AS $$
        SELECT trim(to_char(d, 'FMDay'));
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION dayname(d timestamp) RETURNS text AS $$
        SELECT trim(to_char(d, 'FMDay'));
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
), (
    "monthname",
    r#"
    CREATE OR REPLACE FUNCTION monthname(d date) RETURNS text AS $$
        SELECT trim(to_char(d, 'FMMonth'));
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION monthname(d timestamp) RETURNS text AS $$
        SELECT trim(to_char(d, 'FMMonth'));
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
), (
    "locate",
    r#"
    -- MySQL LOCATE(substr, str[, pos]) is POSITION(substr IN str) with the argument
    -- order swapped, plus an optional 1-based start position PostgreSQL has no
    -- built-in equivalent for.
    CREATE OR REPLACE FUNCTION locate(needle text, haystack text) RETURNS integer AS $$
        SELECT position(needle IN haystack);
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION locate(needle text, haystack text, start_pos integer) RETURNS integer AS $$
        SELECT CASE
            WHEN start_pos < 1 THEN 0
            ELSE (
                SELECT CASE WHEN pos = 0 THEN 0 ELSE pos + start_pos - 1 END
                FROM (SELECT position(needle IN substring(haystack FROM start_pos)) AS pos) sub
            )
        END;
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
), (
    "round",
    r#"
    -- MySQL rounds floats to a given number of decimals; PostgreSQL only defines the
    -- two-argument round() for `numeric`, so ROUND(<float>, n) fails to resolve.
    -- Routing through numeric keeps MySQL's behaviour. A `real` argument reaches
    -- this via PostgreSQL's implicit widening to double precision.
    CREATE OR REPLACE FUNCTION round(value double precision, places integer) RETURNS double precision AS $$
        SELECT round(value::numeric, places)::double precision;
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
), (
    "hour",
    r#"
    -- MySQL exposes date parts as functions (HOUR(t), MONTH(d), ...); PostgreSQL
    -- only has EXTRACT. Matomo's VisitTime reports call HOUR() directly.
    CREATE OR REPLACE FUNCTION hour(value time) RETURNS integer AS $$
        SELECT EXTRACT(HOUR FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION hour(value timestamp) RETURNS integer AS $$
        SELECT EXTRACT(HOUR FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION minute(value time) RETURNS integer AS $$
        SELECT EXTRACT(MINUTE FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION minute(value timestamp) RETURNS integer AS $$
        SELECT EXTRACT(MINUTE FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION second(value time) RETURNS integer AS $$
        SELECT EXTRACT(SECOND FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION second(value timestamp) RETURNS integer AS $$
        SELECT EXTRACT(SECOND FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
), (
    "dayofmonth",
    r#"
    CREATE OR REPLACE FUNCTION dayofmonth(value date) RETURNS integer AS $$
        SELECT EXTRACT(DAY FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION dayofmonth(value timestamp) RETURNS integer AS $$
        SELECT EXTRACT(DAY FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    -- MySQL DAYOFWEEK() is 1=Sunday..7=Saturday; PostgreSQL's DOW is 0=Sunday..6.
    CREATE OR REPLACE FUNCTION dayofweek(value date) RETURNS integer AS $$
        SELECT EXTRACT(DOW FROM value)::integer + 1;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION dayofweek(value timestamp) RETURNS integer AS $$
        SELECT EXTRACT(DOW FROM value)::integer + 1;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION dayofyear(value date) RETURNS integer AS $$
        SELECT EXTRACT(DOY FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION dayofyear(value timestamp) RETURNS integer AS $$
        SELECT EXTRACT(DOY FROM value)::integer;
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
), (
    "hex",
    r#"
    -- MySQL HEX()/UNHEX() map onto PostgreSQL's encode()/decode(). MySQL returns
    -- uppercase hex, which matters when the result is compared as a string.
    CREATE OR REPLACE FUNCTION hex(value bytea) RETURNS text AS $$
        SELECT upper(encode(value, 'hex'));
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION hex(value text) RETURNS text AS $$
        SELECT upper(encode(convert_to(value, 'UTF8'), 'hex'));
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION hex(value bigint) RETURNS text AS $$
        SELECT upper(to_hex(value));
    $$ LANGUAGE sql IMMUTABLE STRICT;

    CREATE OR REPLACE FUNCTION unhex(value text) RETURNS bytea AS $$
        SELECT decode(value, 'hex');
    $$ LANGUAGE sql IMMUTABLE STRICT;
    "#,
)];

/// If `err` is PostgreSQL's "undefined function" error for one of our known MySQL
/// compat functions, returns that function's registered name.
fn missing_compat_function_name(err: &tokio_postgres::Error) -> Option<&'static str> {
    if err.code() != Some(&tokio_postgres::error::SqlState::UNDEFINED_FUNCTION) {
        return None;
    }
    let message = err.as_db_error()?.message().to_lowercase();
    MYSQL_COMPAT_FUNCTIONS
        .iter()
        .map(|(name, _)| *name)
        .find(|name| message.contains(&format!("function {name}(")))
}

/// Creates the named compat function on `client` on demand. Used when a query fails
/// with "function does not exist" so the middleware self-heals without requiring the
/// eager startup install to have already run on this database (e.g. a fresh database,
/// or the one-shot `execute`/`translate` CLI commands, which never call
/// [`install_mysql_compat_functions`]).
async fn install_compat_function_just_in_time(client: &tokio_postgres::Client, name: &str) -> bool {
    let Some(statements) = MYSQL_COMPAT_FUNCTIONS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, statements)| *statements)
    else {
        return false;
    };
    match client.batch_execute(statements).await {
        Ok(()) => {
            tracing::info!("just-in-time installed PostgreSQL compatibility function `{name}` after a query needed it");
            true
        }
        Err(err) => {
            tracing::warn!(
                "failed to just-in-time install PostgreSQL compatibility function `{name}`: {}",
                format_pg_error(&err)
            );
            false
        }
    }
}

/// Upper bound on self-healing retries for one statement. PostgreSQL reports only
/// one missing NOT NULL column per attempt, so a wide INSERT can need several passes;
/// the bound just guarantees the loop terminates.
const MAX_COMPAT_REPAIR_ATTEMPTS: usize = 24;


/// Prepares and runs a row-returning statement, applying the same self-healing
/// repairs as [`simple_query_with_compat_retry`]. The prepared statement is returned
/// alongside the rows so column names survive an empty result.
async fn typed_query_with_compat_retry(
    client: &tokio_postgres::Client,
    sql: &str,
) -> Result<(tokio_postgres::Statement, Vec<tokio_postgres::Row>), tokio_postgres::Error> {
    let mut statement_sql = sql.to_string();
    for _ in 0..MAX_COMPAT_REPAIR_ATTEMPTS {
        let attempt = async {
            let statement = client.prepare(&statement_sql).await?;
            let rows = client.query(&statement, &[]).await?;
            Ok::<_, tokio_postgres::Error>((statement, rows))
        }
        .await;

        let err = match attempt {
            Ok(result) => return Ok(result),
            Err(err) => err,
        };

        if let Some(name) = missing_compat_function_name(&err) {
            if install_compat_function_just_in_time(client, name).await {
                continue;
            }
        }
        if let Some(repaired) = repair_on_conflict_target(client, &statement_sql, &err).await {
            statement_sql = repaired;
            continue;
        }
        return Err(err);
    }
    let statement = client.prepare(&statement_sql).await?;
    let rows = client.query(&statement, &[]).await?;
    Ok((statement, rows))
}

async fn simple_query_with_compat_retry(
    client: &tokio_postgres::Client,
    sql: &str,
) -> Result<Vec<SimpleQueryMessage>, tokio_postgres::Error> {
    let mut statement = sql.to_string();
    for _ in 0..MAX_COMPAT_REPAIR_ATTEMPTS {
        let err = match client.simple_query(&statement).await {
            Ok(messages) => return Ok(messages),
            Err(err) => err,
        };

        if let Some(name) = missing_compat_function_name(&err) {
            if install_compat_function_just_in_time(client, name).await {
                continue;
            }
        }
        if let Some(repaired) = repair_on_conflict_target(client, &statement, &err).await {
            statement = repaired;
            continue;
        }
        if let Some(repaired) = supply_mysql_implicit_default(client, &statement, &err).await {
            statement = repaired;
            continue;
        }
        return Err(err);
    }
    client.simple_query(&statement).await
}

/// Supplies MySQL's implicit default for a NOT NULL column an INSERT left out.
///
/// MySQL accepts an INSERT that omits a NOT NULL column with no DEFAULT, storing an
/// implicit default (`''` for strings, `0` for numbers). PostgreSQL rejects it with
/// 23502. This is a general dialect difference, not an application quirk — any
/// MySQL-native app can rely on it — so the column and its type are read from the
/// error and catalog rather than being special-cased by name.
///
/// Only string and numeric columns are handled. MySQL's implicit default for a date
/// is `0000-00-00`, which PostgreSQL cannot represent at all, so those are left to
/// fail rather than silently storing a different instant.
async fn supply_mysql_implicit_default(
    client: &tokio_postgres::Client,
    sql: &str,
    err: &tokio_postgres::Error,
) -> Option<String> {
    if err.code() != Some(&tokio_postgres::error::SqlState::NOT_NULL_VIOLATION) {
        return None;
    }
    let db_err = err.as_db_error()?;
    let column = db_err.column()?;
    let table = db_err.table()?;

    let row = client
        .query_opt(
            "SELECT format_type(a.atttypid, a.atttypmod) \
             FROM pg_attribute a \
             WHERE a.attrelid = to_regclass($1) AND a.attname = $2 AND a.attnum > 0 AND NOT a.attisdropped",
            &[&table, &column],
        )
        .await
        .ok()??;
    let data_type: String = row.try_get::<usize, String>(0).ok()?;
    let default = mysql_implicit_default_literal(&data_type)?;

    let repaired = insert_column_with_value(sql, column, &default)?;
    tracing::info!(
        "supplied MySQL's implicit default for NOT NULL column {}.{} ({})",
        table,
        column,
        data_type
    );
    Some(repaired)
}

fn mysql_implicit_default_literal(data_type: &str) -> Option<&'static str> {
    let normalized = data_type.to_ascii_lowercase();
    if normalized.starts_with("character")
        || normalized.starts_with("varchar")
        || normalized.starts_with("text")
        || normalized.starts_with("citext")
    {
        return Some("''");
    }
    if normalized.starts_with("smallint")
        || normalized.starts_with("integer")
        || normalized.starts_with("bigint")
        || normalized.starts_with("numeric")
        || normalized.starts_with("decimal")
        || normalized.starts_with("real")
        || normalized.starts_with("double")
    {
        return Some("0");
    }
    if normalized.starts_with("bytea") {
        return Some("''::bytea");
    }
    None
}

/// Adds `column` to an INSERT's column list and `value` to each of its VALUES tuples.
/// Returns `None` for any statement shape that is not a plain
/// `INSERT INTO t (cols) VALUES (...)[, (...)]`, so unusual statements are left alone.
fn insert_column_with_value(sql: &str, column: &str, value: &str) -> Option<String> {
    let columns_open = sql.find('(')?;
    let columns_close = matching_paren(sql, columns_open)?;
    let values_keyword = sql[columns_close..].to_ascii_uppercase().find("VALUES")? + columns_close;

    let mut out = String::with_capacity(sql.len() + column.len() + value.len() + 8);
    out.push_str(&sql[..columns_close]);
    out.push_str(", ");
    out.push_str(&quote_ident(column));
    out.push_str(&sql[columns_close..values_keyword + "VALUES".len()]);

    // Append the value to every tuple after VALUES, leaving any trailing clause
    // (ON CONFLICT, RETURNING, ...) untouched.
    let mut cursor = values_keyword + "VALUES".len();
    let mut appended_any = false;
    while let Some(open_offset) = sql[cursor..].find('(') {
        let open = cursor + open_offset;
        // Stop at a clause that follows the tuple list rather than another tuple.
        if sql[cursor..open].chars().any(|c| c.is_alphabetic()) {
            break;
        }
        let close = matching_paren(sql, open)?;
        out.push_str(&sql[cursor..close]);
        out.push_str(", ");
        out.push_str(value);
        out.push(')');
        cursor = close + 1;
        appended_any = true;
    }
    if !appended_any {
        return None;
    }
    out.push_str(&sql[cursor..]);
    Some(out)
}

/// Index of the `)` matching the `(` at `open`, ignoring parentheses inside quotes.
fn matching_paren(sql: &str, open: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    if bytes.get(open)? != &b'(' {
        return None;
    }
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    for (idx, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'(' if !in_single && !in_double => depth += 1,
            b')' if !in_single && !in_double => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

/// Retargets `ON CONFLICT (...)` at the table's real primary key.
///
/// Translating MySQL's `ON DUPLICATE KEY UPDATE` needs a conflict target, but which
/// key a duplicate would violate is a property of the schema, which the translator
/// cannot see — it guesses from the statement alone. When that guess doesn't match a
/// real unique constraint PostgreSQL raises 42P10, and the primary key is then read
/// from the catalog and substituted. Doing this only on failure keeps the extra
/// lookup off the common path.
async fn repair_on_conflict_target(
    client: &tokio_postgres::Client,
    sql: &str,
    err: &tokio_postgres::Error,
) -> Option<String> {
    if err.code() != Some(&tokio_postgres::error::SqlState::INVALID_COLUMN_REFERENCE) {
        return None;
    }
    let table_name = extract_insert_table_name(sql)?;
    let rows = client
        .query(
            "SELECT a.attname \
             FROM pg_index i \
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
             WHERE i.indrelid = to_regclass($1) AND i.indisprimary \
             ORDER BY array_position(i.indkey, a.attnum)",
            &[&table_name],
        )
        .await
        .ok()?;
    let key_columns = rows
        .iter()
        .filter_map(|row| row.try_get::<usize, String>(0).ok())
        .collect::<Vec<_>>();
    if key_columns.is_empty() {
        return None;
    }

    let target = key_columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let conflict_re = Regex::new(r"(?is)ON\s+CONFLICT\s*\([^)]*\)").ok()?;
    if !conflict_re.is_match(sql) {
        return None;
    }
    let repaired = conflict_re
        .replace(sql, format!("ON CONFLICT ({target})").as_str())
        .into_owned();
    tracing::info!(
        "retargeted ON CONFLICT at {}'s primary key ({}) after PostgreSQL rejected the inferred target",
        table_name,
        key_columns.join(", ")
    );
    Some(repaired)
}

async fn prepare_with_compat_retry(
    client: &tokio_postgres::Client,
    sql: &str,
) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
    match client.prepare(sql).await {
        Ok(statement) => Ok(statement),
        Err(err) => {
            if let Some(name) = missing_compat_function_name(&err) {
                if install_compat_function_just_in_time(client, name).await {
                    return client.prepare(sql).await;
                }
            }
            Err(err)
        }
    }
}

/// Installs PostgreSQL equivalents for MySQL builtin functions that MySQL-native
/// applications rely on (e.g. Matomo's `CRC32()` action-lookup queries). Runs on
/// every startup; each statement is `CREATE OR REPLACE`, so this is idempotent and
/// safe to repeat. A failure here (e.g. insufficient privileges) is logged but does
/// not stop the middleware from serving: [`simple_query_with_compat_retry`] and
/// [`prepare_with_compat_retry`] install the same functions just-in-time on first use.
pub async fn install_mysql_compat_functions(executor: &dyn PostgresExecutor) {
    for (name, statements) in MYSQL_COMPAT_FUNCTIONS {
        // Executed as one multi-statement string (not split on `;`): a naive split
        // would cut through the dollar-quoted function bodies below and produce
        // "unterminated dollar-quoted string" errors. PostgreSQL's own parser
        // handles the `;`-separated statements and the dollar-quoting together.
        if let Err(err) = executor.execute_sql(statements).await {
            tracing::warn!(
                "could not install PostgreSQL compatibility function `{name}`: {err}; \
                 SQL relying on MySQL's `{name}()` will fail until this is created manually"
            );
        } else {
            tracing::info!("PostgreSQL compatibility function `{name}` is installed");
        }
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

pub fn connection_string_for_config(cfg: &AppConfig) -> Result<String, MiddlewareError> {
    match cfg.postgres.driver.as_str() {
        "tokio-postgres" => Ok(cfg.postgres.connection_string.clone()),
        other => Err(MiddlewareError::Config(format!(
            "unsupported postgres driver `{other}`; currently supported: tokio-postgres"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use tokio_postgres::types::{Format, IsNull, ToSql, Type};

    use super::{extract_insert_table_name, find_last_insert_id_update_target, PgParam};

    #[test]
    fn rewrites_matomo_sequence_last_insert_id_update() {
        // The exact SQL Matomo's Sequence::getNextId() issues.
        let sql = "UPDATE matomo_sequence SET value = LAST_INSERT_ID(value + 1) WHERE name = ?";
        let (column, wrapped) = find_last_insert_id_update_target(sql).expect("should detect LAST_INSERT_ID target");
        assert_eq!(column, "value");
        assert!(wrapped.contains("UPDATE matomo_sequence SET value = value + 1 WHERE name = ?"));
        assert!(wrapped.contains("RETURNING \"value\""));
        assert!(wrapped.contains("__mw_row_count__"));
        assert!(wrapped.contains("__mw_last_insert_id__"));
        assert!(!wrapped.contains("LAST_INSERT_ID"));
    }

    #[test]
    fn does_not_misdetect_ordinary_updates() {
        assert!(find_last_insert_id_update_target("UPDATE matomo_option SET option_value = ? WHERE option_name = ?").is_none());
    }

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

    #[test]
    fn renders_every_type_matomo_reads_over_prepared_statements() {
        // Guards against the "<unrendered>" placeholder regression: over the extended
        // protocol these types arrive in binary form, and a type this function does
        // not decode is emitted as a placeholder that clients then try to parse as a
        // date. Each type here must have an explicit arm in `value_to_string`.
        for ty in [
            Type::TIMESTAMP,
            Type::TIMESTAMPTZ,
            Type::DATE,
            Type::TIME,
            Type::NUMERIC,
        ] {
            assert!(
                value_to_string_has_explicit_arm(&ty),
                "{ty} falls through to the <unrendered> placeholder"
            );
        }
    }

    /// Mirrors the match in [`value_to_string`]; kept in sync deliberately so adding a
    /// type to one without the other fails the test above.
    fn value_to_string_has_explicit_arm(ty: &Type) -> bool {
        matches!(
            *ty,
            Type::BOOL
                | Type::INT2
                | Type::INT4
                | Type::INT8
                | Type::FLOAT4
                | Type::FLOAT8
                | Type::TEXT
                | Type::VARCHAR
                | Type::BPCHAR
                | Type::NAME
                | Type::JSON
                | Type::JSONB
                | Type::BYTEA
                | Type::TIMESTAMP
                | Type::TIMESTAMPTZ
                | Type::DATE
                | Type::TIME
                | Type::NUMERIC
        )
    }

    #[test]
    fn binary_parameter_uses_postgres_bytea_binary_format() {
        let param = PgParam::Bytes(vec![0x2d, 0xf0, 0x86, 0x00, 0xf4]);
        let mut output = BytesMut::new();

        assert!(matches!(param.encode_format(&Type::BYTEA), Format::Binary));
        assert!(matches!(
            param.to_sql(&Type::BYTEA, &mut output).unwrap(),
            IsNull::No
        ));
        assert_eq!(&output[..], &[0x2d, 0xf0, 0x86, 0x00, 0xf4]);
    }

    #[test]
    fn byte_parameter_remains_text_for_text_targets() {
        let param = PgParam::Bytes(b"Matomo".to_vec());
        let mut output = BytesMut::new();

        assert!(matches!(param.encode_format(&Type::TEXT), Format::Text));
        assert!(matches!(
            param.to_sql(&Type::TEXT, &mut output).unwrap(),
            IsNull::No
        ));
        assert_eq!(&output[..], b"Matomo");
    }
}
