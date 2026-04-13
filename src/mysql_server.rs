use std::{collections::HashMap, io, sync::Arc};

use async_trait::async_trait;
use opensrv_mysql::{
    AsyncMysqlIntermediary, AsyncMysqlShim, Column, ColumnFlags, ColumnType, ErrorKind, InitWriter,
    IntermediaryOptions, OkResponse, ParamParser, QueryResultWriter, StatementMetaWriter,
    ValueInner,
};
use sqlparser::ast::{Expr, Query, SelectItem, SetExpr, Statement};
use tokio::{io::split, net::TcpListener};

use crate::{
    config::AppConfig,
    error::MiddlewareError,
    executor::{PgParam, PostgresExecutor, QueryResult},
    parser::parse_mysql_sql,
    translator::{translate_sql, TranslationResult},
};

#[derive(Debug)]
pub enum MySqlServerError {
    Io(io::Error),
    Middleware(MiddlewareError),
}

impl std::fmt::Display for MySqlServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Middleware(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for MySqlServerError {}

impl From<io::Error> for MySqlServerError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<MiddlewareError> for MySqlServerError {
    fn from(value: MiddlewareError) -> Self {
        Self::Middleware(value)
    }
}

#[derive(Clone)]
pub struct MySqlFrontendFactory {
    config: Arc<AppConfig>,
    executor: Arc<dyn PostgresExecutor>,
}

impl MySqlFrontendFactory {
    pub fn new(config: Arc<AppConfig>, executor: Arc<dyn PostgresExecutor>) -> Self {
        Self { config, executor }
    }

    fn connection_backend(&self) -> MySqlBackend {
        MySqlBackend {
            config: self.config.clone(),
            executor: self.executor.clone(),
            next_statement_id: 1,
            prepared: HashMap::new(),
            current_db: None,
            last_insert_id: 0,
        }
    }
}

pub async fn serve_mysql(factory: MySqlFrontendFactory, bind_addr: String) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&bind_addr).await?;
    tracing::info!("mysql-compatible frontend listening on {}", bind_addr);

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let backend = factory.connection_backend();
        tokio::spawn(async move {
            let (reader, writer) = split(socket);
            if let Err(err) = AsyncMysqlIntermediary::run_with_options(
                backend,
                reader,
                writer,
                &IntermediaryOptions {
                    process_use_statement_on_query: true,
                    reject_connection_on_dbname_absence: false,
                },
            )
            .await
            {
                tracing::warn!("mysql frontend connection {} failed: {}", peer_addr, err);
            }
        });
    }
}

struct PreparedStatement {
    original_sql: String,
    postgres_sql: String,
    params: Vec<Column>,
    columns: Vec<Column>,
    canned_rows: Option<(Vec<String>, Vec<Vec<String>>)>,
}

struct MySqlBackend {
    config: Arc<AppConfig>,
    executor: Arc<dyn PostgresExecutor>,
    next_statement_id: u32,
    prepared: HashMap<u32, PreparedStatement>,
    current_db: Option<String>,
    last_insert_id: u64,
}

#[async_trait]
impl<W> AsyncMysqlShim<W> for MySqlBackend
where
    W: tokio::io::AsyncWrite + Send + Unpin,
{
    type Error = MySqlServerError;

    fn version(&self) -> String {
        "8.0.0-mysql2pg".to_string()
    }



    async fn on_init<'a>(
        &'a mut self,
        schema: &'a str,
        writer: InitWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        self.current_db = if schema.trim().is_empty() {
            None
        } else {
            Some(schema.trim().to_string())
        };
        writer.ok().await?;
        Ok(())
    }

    async fn on_prepare<'a>(
        &'a mut self,
        query: &'a str,
        info: StatementMetaWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        tracing::info!("mysql prepare: {}", query);
        let param_count = count_prepare_placeholders(query);
        let is_compat_noop = is_compat_noop_query(query);

        if is_compat_noop {
            let statement_id = self.next_statement_id;
            self.next_statement_id += 1;
            let params = (0..param_count)
                .map(|idx| make_param_column(idx + 1))
                .collect::<Vec<_>>();
            self.prepared.insert(
                statement_id,
                PreparedStatement {
                    original_sql: query.to_string(),
                    postgres_sql: String::new(),
                    params,
                    columns: Vec::new(),
                    canned_rows: None,
                },
            );

            let params: Vec<&Column> = self.prepared[&statement_id].params.iter().collect();
            let columns: Vec<&Column> = Vec::new();
            tracing::info!(
                "mysql prepare metadata stmt_id={} params={} columns=0 (compat noop)",
                statement_id,
                params.len()
            );
            info.reply(statement_id, params, columns).await?;
            return Ok(());
        }

        let translated = match translate_preparable_sql(query, &self.config.translator) {
            Ok(result) => result.translated_sql,
            Err(err) => {
                tracing::warn!("mysql prepare translation failed for `{}`: {}", query, err);
                info.error(ErrorKind::ER_PARSE_ERROR, err.to_string().as_bytes()).await?;
                return Ok(());
            }
        };
        let postgres_sql = rewrite_prepare_placeholders_for_postgres(&translated)?;
        tracing::info!("mysql prepare translated: {}", postgres_sql);
        let canned_rows = canned_response_for_query(query);
        let statement_id = self.next_statement_id;
        self.next_statement_id += 1;
        let params = (0..param_count)
            .map(|idx| make_param_column(idx + 1))
            .collect::<Vec<_>>();
        let inferred_columns = infer_prepare_result_columns(query, &postgres_sql);
        let column_names = if inferred_columns.is_empty() || inferred_columns.iter().any(|name| name == "*") {
            match self.executor.describe_prepared_sql(&postgres_sql).await {
                Ok(described) => described,
                Err(err) => {
                    tracing::warn!(
                        "mysql prepare describe failed for `{}`: {}",
                        postgres_sql,
                        err
                    );
                    inferred_columns
                }
            }
        } else {
            inferred_columns
        };
        let columns = column_names
            .into_iter()
            .map(|name| make_string_column(&name))
            .collect::<Vec<_>>();
        tracing::info!(
            "mysql prepare metadata stmt_id={} params={} columns={}",
            statement_id,
            params.len(),
            columns.len()
        );
        self.prepared.insert(
            statement_id,
            PreparedStatement {
                original_sql: query.to_string(),
                postgres_sql,
                params,
                columns,
                canned_rows,
            },
        );

        let params: Vec<&Column> = self.prepared[&statement_id].params.iter().collect();
        let columns: Vec<&Column> = self.prepared[&statement_id].columns.iter().collect();
        info.reply(statement_id, params, columns).await?;
        Ok(())
    }

    async fn on_execute<'a>(
        &'a mut self,
        id: u32,
        params: ParamParser<'a>,
        results: QueryResultWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        let Some(statement) = self.prepared.get(&id) else {
            results
                .error(ErrorKind::ER_UNKNOWN_STMT_HANDLER, b"unknown prepared statement id")
                .await?;
            return Ok(());
        };
        tracing::info!("mysql execute stmt_id={id}");

        if let Some((columns, rows)) = &statement.canned_rows {
            return write_canned_result(results, columns, rows).await;
        }

        if statement.postgres_sql.is_empty() && is_compat_noop_query(&statement.original_sql) {
            results.completed(OkResponse::default()).await?;
            return Ok(());
        }

        let bound_params = match decode_mysql_params(params) {
            Ok(params) => params,
            Err(err) => {
                tracing::warn!("mysql execute bind decode failed for stmt_id={id}: {}", err);
                results.error(ErrorKind::ER_PARSE_ERROR, err.to_string().as_bytes()).await?;
                return Ok(());
            }
        };
        if statement.params.len() != bound_params.len() {
            let msg = format!(
                "prepared statement parameter count mismatch: expected {}, got {}",
                statement.params.len(),
                bound_params.len()
            );
            results.error(ErrorKind::ER_PARSE_ERROR, msg.as_bytes()).await?;
            return Ok(());
        }
        tracing::info!("mysql execute postgres_sql: {}", statement.postgres_sql);

        let execution = if bound_params.is_empty() && statement.postgres_sql.contains(';') {
            execute_multi_statement_sql(self.executor.as_ref(), &statement.postgres_sql).await
        } else {
            self.executor
                .execute_prepared_sql(&statement.postgres_sql, &bound_params)
                .await
        };

        match execution {
            Ok(mut query_result) => {
                if query_result.last_insert_id > 0 {
                    self.last_insert_id = query_result.last_insert_id;
                }
                if !statement.columns.is_empty() && !query_result.columns.is_empty() {
                    query_result.columns = statement
                        .columns
                        .iter()
                        .map(|column| column.column.clone())
                        .collect();
                }
                write_query_result(results, query_result).await
            }
            Err(err) => {
                tracing::warn!(
                    "mysql execute failed stmt_id={id} sql=`{}`: {}",
                    statement.postgres_sql,
                    err
                );
                if let Some(query_result) =
                    compat_empty_result_for_missing_matomo_option(
                        &statement.original_sql,
                        &statement.postgres_sql,
                        &err,
                    )
                {
                    return write_query_result(results, query_result).await;
                }
                let msg = err.to_string();
                results.error(ErrorKind::ER_UNKNOWN_ERROR, msg.as_bytes()).await?;
                Ok(())
            }
        }
    }

    async fn on_close<'a>(&'a mut self, stmt: u32)
    where
        W: 'async_trait,
    {
        self.prepared.remove(&stmt);
    }

    async fn on_reset<'a>(&'a mut self, _stmt: u32) -> Result<OkResponse, Self::Error>
    where
        W: 'async_trait,
    {
        Ok(OkResponse {
            last_insert_id: self.last_insert_id,
            ..Default::default()
        })
    }

    async fn on_query<'a>(
        &'a mut self,
        query: &'a str,
        results: QueryResultWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        let trimmed = query.trim();
        tracing::info!("mysql query: {}", trimmed);

        if is_compat_noop_query(trimmed) {
            results.completed(OkResponse::default()).await?;
            return Ok(());
        }

        if let Some((columns, rows)) = canned_response_for_query(trimmed) {
            write_canned_result(results, &columns, &rows).await?;
            return Ok(());
        }

        let translated = match translate_sql(trimmed, &self.config.translator) {
            Ok(result) => result.translated_sql,
            Err(err) => {
                tracing::warn!("mysql query translation failed for `{}`: {}", trimmed, err);
                let msg = err.to_string();
                results.error(ErrorKind::ER_PARSE_ERROR, msg.as_bytes()).await?;
                return Ok(());
            }
        };
        tracing::info!("mysql query translated: {}", translated);

        match self.executor.execute_sql(&translated).await {
            Ok(query_result) => {
                if query_result.last_insert_id > 0 {
                    self.last_insert_id = query_result.last_insert_id;
                }
                write_query_result(results, query_result).await
            }
            Err(err) => {
                tracing::warn!("mysql query execution failed for `{}`: {}", translated, err);
                let msg = err.to_string();
                results.error(ErrorKind::ER_UNKNOWN_ERROR, msg.as_bytes()).await?;
                Ok(())
            }
        }
    }
}

async fn execute_multi_statement_sql(
    executor: &dyn PostgresExecutor,
    sql: &str,
) -> Result<QueryResult, MiddlewareError> {
    let mut total_row_count = 0u64;

    for statement in sql.split(';').map(str::trim).filter(|part| !part.is_empty()) {
        let result = executor.execute_sql(statement).await?;
        total_row_count += result.row_count;
    }

    Ok(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        row_count: total_row_count,
        last_insert_id: 0,
    })
}

async fn write_query_result<W>(
    results: QueryResultWriter<'_, W>,
    query_result: QueryResult,
) -> Result<(), MySqlServerError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if query_result.columns.is_empty() {
        results
            .completed(OkResponse {
                affected_rows: query_result.row_count,
                last_insert_id: query_result.last_insert_id,
                ..Default::default()
            })
            .await?;
        return Ok(());
    }

    let columns = query_result
        .columns
        .iter()
        .map(|name| make_string_column(name))
        .collect::<Vec<_>>();

    let mut writer = results.start(&columns).await?;
    for row in &query_result.rows {
        writer.write_row(row.clone()).await?;
    }
    writer.finish().await?;
    Ok(())
}

async fn write_canned_result<W>(
    results: QueryResultWriter<'_, W>,
    columns: &[String],
    rows: &[Vec<String>],
) -> Result<(), MySqlServerError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let columns = columns.iter().map(|name| make_string_column(name)).collect::<Vec<_>>();
    let mut writer = results.start(&columns).await?;
    for row in rows {
        writer.write_row(row.clone()).await?;
    }
    writer.finish().await?;
    Ok(())
}

fn compat_empty_result_for_missing_matomo_option(
    original_sql: &str,
    rendered_sql: &str,
    err: &MiddlewareError,
) -> Option<QueryResult> {
    let err_text = err.to_string();
    if !err_text.contains("relation \"matomo_option\" does not exist") {
        return None;
    }

    let normalized = rendered_sql.trim_start().to_ascii_uppercase();
    if !normalized.starts_with("SELECT") || !rendered_sql.contains("\"matomo_option\"") {
        return None;
    }

    let columns = infer_result_columns(original_sql);
    Some(QueryResult {
        columns,
        rows: Vec::new(),
        row_count: 0,
        last_insert_id: 0,
    })
}

fn make_string_column(name: &str) -> Column {
    Column {
        table: "result".to_string(),
        column: name.to_string(),
        coltype: ColumnType::MYSQL_TYPE_VAR_STRING,
        colflags: ColumnFlags::empty(),
    }
}

fn make_param_column(index: usize) -> Column {
    Column {
        table: "".to_string(),
        column: format!("p{index}"),
        coltype: ColumnType::MYSQL_TYPE_VAR_STRING,
        colflags: ColumnFlags::empty(),
    }
}

fn infer_result_columns(query: &str) -> Vec<String> {
    let Ok(statements) = parse_mysql_sql(query) else {
        return Vec::new();
    };
    let Some(statement) = statements.first() else {
        return Vec::new();
    };

    match statement {
        Statement::Query(query) => infer_query_result_columns(query),
        _ => Vec::new(),
    }
}

fn infer_prepare_result_columns(original_sql: &str, translated_sql: &str) -> Vec<String> {
    if let Some(columns) = compat_prepare_columns_for_query(original_sql) {
        return columns;
    }

    let columns = infer_result_columns(original_sql);
    if !columns.is_empty() {
        return columns;
    }

    let translated_columns = infer_result_columns(translated_sql);
    if translated_columns.iter().any(|name| name == "*") {
        Vec::new()
    } else {
        translated_columns
    }
}

fn compat_prepare_columns_for_query(query: &str) -> Option<Vec<String>> {
    let normalized = query.trim().to_ascii_uppercase();

    if normalized.starts_with("SHOW VARIABLES") || normalized.starts_with("SHOW STATUS") {
        return Some(vec!["Variable_name".to_string(), "Value".to_string()]);
    }

    if normalized.starts_with("SHOW CHARACTER SET") {
        return Some(vec![
            "Charset".to_string(),
            "Description".to_string(),
            "Default collation".to_string(),
            "Maxlen".to_string(),
        ]);
    }

    if normalized.starts_with("SHOW TABLES") {
        return Some(vec!["Tables_in_current_schema".to_string()]);
    }

    if normalized.starts_with("SHOW INDEX")
        || normalized.starts_with("SHOW INDEXES")
        || normalized.starts_with("SHOW KEYS")
    {
        return Some(vec![
            "Table".to_string(),
            "Non_unique".to_string(),
            "Key_name".to_string(),
            "Seq_in_index".to_string(),
            "Column_name".to_string(),
            "Collation".to_string(),
            "Cardinality".to_string(),
            "Sub_part".to_string(),
            "Packed".to_string(),
            "Null".to_string(),
            "Index_type".to_string(),
            "Comment".to_string(),
            "Index_comment".to_string(),
            "Visible".to_string(),
            "Expression".to_string(),
        ]);
    }

    None
}

fn infer_query_result_columns(query: &Query) -> Vec<String> {
    match query.body.as_ref() {
        SetExpr::Select(select) => select
            .projection
            .iter()
            .map(infer_select_item_name)
            .collect(),
        _ => Vec::new(),
    }
}

fn infer_select_item_name(item: &SelectItem) -> String {
    match item {
        SelectItem::UnnamedExpr(expr) => infer_expr_name(expr),
        SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
        SelectItem::QualifiedWildcard(_, _) | SelectItem::Wildcard(_) => "*".to_string(),
    }
}

fn infer_expr_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(ident) => ident.value.clone(),
        Expr::CompoundIdentifier(parts) => parts.last().map(|p| p.value.clone()).unwrap_or_else(|| expr.to_string()),
        _ => expr.to_string(),
    }
}

fn canned_response_for_query(query: &str) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let normalized = query.trim();
    if normalized.eq_ignore_ascii_case("SELECT DATABASE()") {
        return Some((vec!["DATABASE()".to_string()], vec![vec!["app".to_string()]]));
    }
    if normalized.eq_ignore_ascii_case("SELECT VERSION()") {
        return Some((
            vec!["VERSION()".to_string()],
            vec![vec!["8.0.0-mysql2pg".to_string()]],
        ));
    }
    if normalized.eq_ignore_ascii_case("SELECT @@VERSION") {
        return Some((
            vec!["@@VERSION".to_string()],
            vec![vec!["8.0.0-mysql2pg".to_string()]],
        ));
    }
    if normalized.eq_ignore_ascii_case("SELECT @@VERSION_COMMENT") {
        return Some((
            vec!["@@VERSION_COMMENT".to_string()],
            vec![vec!["mysql2pg-middleware".to_string()]],
        ));
    }
    if normalized.eq_ignore_ascii_case("SELECT @@SESSION.SQL_MODE")
        || normalized.eq_ignore_ascii_case("SELECT @@SQL_MODE")
    {
        return Some((
            vec![normalized.to_string()],
            vec![vec!["ANSI".to_string()]],
        ));
    }
    None
}

fn is_compat_noop_query(query: &str) -> bool {
    let normalized = query.trim().to_ascii_uppercase();
    normalized.starts_with("SET NAMES ")
        || normalized.starts_with("SET SQL_MODE")
        || normalized.starts_with("CREATE DATABASE ")
        || normalized.starts_with("DROP DATABASE ")
        || is_mysql_session_compat_noop(&normalized)
}

fn is_mysql_session_compat_noop(normalized_query: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "SET WAIT_TIMEOUT",
        "SET SESSION WAIT_TIMEOUT",
        "SET @@WAIT_TIMEOUT",
        "SET @@SESSION.WAIT_TIMEOUT",
        "SET INTERACTIVE_TIMEOUT",
        "SET SESSION INTERACTIVE_TIMEOUT",
        "SET @@INTERACTIVE_TIMEOUT",
        "SET @@SESSION.INTERACTIVE_TIMEOUT",
        "SET SESSION GROUP_CONCAT_MAX_LEN",
        "SET @@SESSION.GROUP_CONCAT_MAX_LEN",
        "SET @@GROUP_CONCAT_MAX_LEN",
        "SET @@INNODB_LOCK_WAIT_TIMEOUT",
        "SET @@SESSION.INNODB_LOCK_WAIT_TIMEOUT",
        "SET SESSION SQL_REQUIRE_PRIMARY_KEY",
        "SET @@SESSION.SQL_REQUIRE_PRIMARY_KEY",
        "SET @@SQL_REQUIRE_PRIMARY_KEY",
        "SET GLOBAL INNODB_FORCE_PRIMARY_KEY",
        "SET SESSION CHARACTER_SET_CLIENT",
        "SET SESSION CHARACTER_SET_RESULTS",
        "SET SESSION COLLATION_CONNECTION",
        "SET CHARACTER_SET_CLIENT",
        "SET CHARACTER_SET_RESULTS",
        "SET COLLATION_CONNECTION",
        "SET TIME_ZONE",
        "SET SESSION TIME_ZONE",
        "SET FOREIGN_KEY_CHECKS",
        "SET UNIQUE_CHECKS",
        "SET SQL_NOTES",
    ];

    PREFIXES
        .iter()
        .any(|prefix| normalized_query.starts_with(prefix))
}

fn decode_mysql_params(params: ParamParser<'_>) -> Result<Vec<PgParam>, MiddlewareError> {
    params.into_iter().map(decode_mysql_param).collect()
}

fn translate_preparable_sql(
    query: &str,
    cfg: &crate::config::TranslatorConfig,
) -> Result<TranslationResult, MiddlewareError> {
    match translate_sql(query, cfg) {
        Ok(result) => Ok(result),
        Err(err) if query.contains('?') => {
            let rewritten = replace_prepare_params_with_string_literals(query);
            match translate_sql(&rewritten, cfg) {
                Ok(mut result) => {
                    result.original_sql = query.to_string();
                    result.canonical_mysql_sql = restore_prepare_param_markers(&result.canonical_mysql_sql);
                    result.translated_sql = restore_prepare_param_markers(&result.translated_sql);
                    Ok(result)
                }
                Err(_) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

fn replace_prepare_params_with_string_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 16);
    let mut idx = 1usize;
    for ch in sql.chars() {
        if ch == '?' {
            out.push('\'');
            out.push_str(&prepare_param_marker(idx));
            out.push('\'');
            idx += 1;
        } else {
            out.push(ch);
        }
    }
    out
}

fn restore_prepare_param_markers(sql: &str) -> String {
    let mut restored = sql.to_string();
    for idx in 1..=sql.matches("__mw_prepare_param_").count().max(1) {
        let quoted = format!("'{}'", prepare_param_marker(idx));
        let bare = prepare_param_marker(idx);
        restored = restored.replace(&quoted, "?");
        restored = restored.replace(&bare, "?");
    }
    restored
}

fn prepare_param_marker(index: usize) -> String {
    format!("__mw_prepare_param_{index}__")
}

fn count_prepare_placeholders(sql: &str) -> usize {
    let mut count = 0usize;
    scan_prepare_placeholders(sql, |_| {
        count += 1;
        "?".to_string()
    });
    count
}

fn rewrite_prepare_placeholders_for_postgres(sql: &str) -> Result<String, MiddlewareError> {
    let rewritten = scan_prepare_placeholders(sql, |index| format!("${index}"));
    let expected = count_prepare_placeholders(sql);
    if expected == 0 {
        return Ok(rewritten);
    }
    if !rewritten.contains("$1") {
        return Err(MiddlewareError::Translation(
            "failed to rewrite prepared statement placeholders".to_string(),
        ));
    }
    Ok(rewritten)
}

fn scan_prepare_placeholders(
    sql: &str,
    mut replacement: impl FnMut(usize) -> String,
) -> String {
    let mut count = 0usize;
    let mut out = String::with_capacity(sql.len() + 8);
    let mut chars = sql.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while let Some(ch) = chars.next() {
        if in_line_comment {
            out.push(ch);
            if ch == '\n' {
                in_line_comment = false;
            }
            continue;
        }

        if in_block_comment {
            out.push(ch);
            if ch == '*' && chars.peek() == Some(&'/') {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                in_block_comment = false;
            }
            continue;
        }

        if in_single {
            out.push(ch);
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                continue;
            }
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }

        if in_double {
            out.push(ch);
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                continue;
            }
            if ch == '"' {
                in_double = false;
            }
            continue;
        }

        if in_backtick {
            out.push(ch);
            if ch == '`' {
                in_backtick = false;
            }
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                out.push(ch);
                continue;
            }
            '"' => {
                in_double = true;
                out.push(ch);
                continue;
            }
            '`' => {
                in_backtick = true;
                out.push(ch);
                continue;
            }
            '-' if chars.peek() == Some(&'-') => {
                out.push(ch);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                in_line_comment = true;
                continue;
            }
            '#' => {
                out.push(ch);
                in_line_comment = true;
                continue;
            }
            '/' if chars.peek() == Some(&'*') => {
                out.push(ch);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                in_block_comment = true;
                continue;
            }
            '?' => {
                count += 1;
                out.push_str(&replacement(count));
                continue;
            }
            _ => {}
        }

        out.push(ch);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TranslatorConfig;

    #[test]
    fn translate_preparable_show_variables_like_param() {
        let result =
            translate_preparable_sql("SHOW VARIABLES LIKE ?", &TranslatorConfig::default()).unwrap();
        assert!(result.translated_sql.contains("\"Variable_name\" LIKE ?"));
    }

    #[test]
    fn translate_preparable_show_tables_like_param() {
        let result =
            translate_preparable_sql("SHOW TABLES LIKE ?", &TranslatorConfig::default()).unwrap();
        assert!(result.translated_sql.contains("table_name LIKE ?"));
    }

    #[test]
    fn infer_columns_for_simple_select_prepare() {
        let columns = infer_result_columns("SELECT option_value FROM `matomo_option` WHERE option_name = ?");
        assert_eq!(columns, vec!["option_value".to_string()]);
    }

    #[test]
    fn infer_columns_for_show_variables_prepare() {
        let columns = infer_prepare_result_columns(
            "SHOW VARIABLES LIKE 'character_set_database'",
            "SELECT * FROM something",
        );
        assert_eq!(columns, vec!["Variable_name".to_string(), "Value".to_string()]);
    }

    #[test]
    fn infer_columns_for_show_charset_prepare() {
        let columns = infer_prepare_result_columns(
            "SHOW CHARACTER SET LIKE 'utf8mb4'",
            "SELECT * FROM something",
        );
        assert_eq!(
            columns,
            vec![
                "Charset".to_string(),
                "Description".to_string(),
                "Default collation".to_string(),
                "Maxlen".to_string()
            ]
        );
    }

    #[test]
    fn infer_columns_for_show_index_prepare() {
        let columns = infer_prepare_result_columns(
            "SHOW INDEX FROM `log_visit` WHERE Key_name = ?",
            "SELECT * FROM something",
        );
        assert_eq!(
            columns,
            vec![
                "Table".to_string(),
                "Non_unique".to_string(),
                "Key_name".to_string(),
                "Seq_in_index".to_string(),
                "Column_name".to_string(),
                "Collation".to_string(),
                "Cardinality".to_string(),
                "Sub_part".to_string(),
                "Packed".to_string(),
                "Null".to_string(),
                "Index_type".to_string(),
                "Comment".to_string(),
                "Index_comment".to_string(),
                "Visible".to_string(),
                "Expression".to_string(),
            ]
        );
    }

    #[test]
    fn count_placeholders_ignores_literals_and_comments() {
        let sql = "SELECT '?', col FROM t WHERE a = ? AND b = \"?\" /* ? */ -- ?\n AND c = ?";
        assert_eq!(count_prepare_placeholders(sql), 2);
    }

    #[test]
    fn rewrite_prepare_placeholders_for_postgres_ignores_literals_and_comments() {
        let sql = "SELECT '?', col FROM t WHERE a = ? AND note = \"?\" /* ? */ -- ?\n AND c = ?";
        let rewritten = rewrite_prepare_placeholders_for_postgres(sql).unwrap();
        assert_eq!(
            rewritten,
            "SELECT '?', col FROM t WHERE a = $1 AND note = \"?\" /* ? */ -- ?\n AND c = $2"
        );
    }

    #[test]
    fn decode_mysql_time_parameter_to_text() {
        let rendered = decode_mysql_time_literal(&[0, 1, 0, 0, 0, 2, 3, 4]).unwrap();
        assert_eq!(rendered, "26:03:04");
    }

    #[test]
    fn mysql_session_compat_noops_cover_wait_timeout() {
        assert!(is_compat_noop_query("SET wait_timeout=28800"));
        assert!(is_compat_noop_query("SET SESSION group_concat_max_len=131072"));
        assert!(is_compat_noop_query("SET @@innodb_lock_wait_timeout = 3"));
    }

    #[test]
    fn canned_response_supports_select_session_sql_mode() {
        let (columns, rows) = canned_response_for_query("SELECT @@SESSION.sql_mode").unwrap();
        assert_eq!(columns, vec!["SELECT @@SESSION.sql_mode".to_string()]);
        assert_eq!(rows, vec![vec!["ANSI".to_string()]]);
    }
}

fn decode_mysql_param(param: opensrv_mysql::ParamValue<'_>) -> Result<PgParam, MiddlewareError> {
    match param.value.into_inner() {
        ValueInner::NULL => Ok(PgParam::Null),
        ValueInner::Bytes(bytes) => Ok(PgParam::Text(std::str::from_utf8(bytes).map_err(|_| {
            MiddlewareError::Execution("binary prepared statement parameters are not supported yet".to_string())
        })?.to_string())),
        ValueInner::Int(v) => Ok(PgParam::Text(v.to_string())),
        ValueInner::UInt(v) => Ok(PgParam::Text(v.to_string())),
        ValueInner::Double(v) => Ok(PgParam::Text(v.to_string())),
        ValueInner::Date(bytes) => Ok(PgParam::Text(decode_mysql_date_literal(bytes)?)),
        ValueInner::Datetime(bytes) => Ok(PgParam::Text(decode_mysql_datetime_literal(bytes)?)),
        ValueInner::Time(bytes) => Ok(PgParam::Text(decode_mysql_time_literal(bytes)?)),
    }
}

fn decode_mysql_date_literal(bytes: &[u8]) -> Result<String, MiddlewareError> {
    if bytes.len() != 4 {
        return Err(MiddlewareError::Execution(format!(
            "unsupported MySQL date parameter payload length {}",
            bytes.len()
        )));
    }
    let year = u16::from_le_bytes([bytes[0], bytes[1]]);
    let month = bytes[2];
    let day = bytes[3];
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

fn decode_mysql_datetime_literal(bytes: &[u8]) -> Result<String, MiddlewareError> {
    match bytes.len() {
        0 => Ok("0000-00-00 00:00:00".to_string()),
        4 => {
            let year = u16::from_le_bytes([bytes[0], bytes[1]]);
            let month = bytes[2];
            let day = bytes[3];
            Ok(format!("{year:04}-{month:02}-{day:02} 00:00:00"))
        }
        7 => {
            let year = u16::from_le_bytes([bytes[0], bytes[1]]);
            let month = bytes[2];
            let day = bytes[3];
            let hour = bytes[4];
            let minute = bytes[5];
            let second = bytes[6];
            Ok(format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"))
        }
        11 => {
            let year = u16::from_le_bytes([bytes[0], bytes[1]]);
            let month = bytes[2];
            let day = bytes[3];
            let hour = bytes[4];
            let minute = bytes[5];
            let second = bytes[6];
            let micros = u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);
            Ok(format!(
                "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"
            ))
        }
        len => Err(MiddlewareError::Execution(format!(
            "unsupported MySQL datetime parameter payload length {len}"
        ))),
    }
}

fn decode_mysql_time_literal(bytes: &[u8]) -> Result<String, MiddlewareError> {
    if bytes.len() != 8 && bytes.len() != 12 {
        return Err(MiddlewareError::Execution(format!(
            "unsupported MySQL time parameter payload length {}",
            bytes.len()
        )));
    }
    let is_negative = bytes[0];
    let days = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    let hours = bytes[5];
    let minutes = bytes[6];
    let seconds = bytes[7];
    let micros = if bytes.len() == 12 {
        u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]])
    } else {
        0
    };

    let total_hours = days * 24 + u32::from(hours);
    let sign = if is_negative != 0 { "-" } else { "" };
    let fraction = if micros > 0 {
        format!(".{micros:06}")
    } else {
        String::new()
    };
    Ok(format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}{fraction}"))
}
