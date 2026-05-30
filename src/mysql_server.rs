use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, LazyLock, Mutex,
    },
};

use async_trait::async_trait;
use opensrv_mysql::{
    AsyncMysqlIntermediary, AsyncMysqlShim, Column, ColumnFlags, ColumnType, ErrorKind, InitWriter,
    IntermediaryOptions, OkResponse, ParamParser, QueryResultWriter, StatementMetaWriter,
    ValueInner,
};
use regex::Regex;
use sqlparser::ast::{Expr, Query, SelectItem, SetExpr, Statement};
use tokio::{io::split, net::TcpListener};

use crate::{
    config::AppConfig,
    error::MiddlewareError,
    executor::{PgParam, PostgresExecutor, QueryResult, SessionPostgresExecutor},
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
    connection_string: String,
}

impl MySqlFrontendFactory {
    pub fn new(config: Arc<AppConfig>, connection_string: String) -> Self {
        Self { config, connection_string }
    }

    fn connection_backend(&self) -> MySqlBackend {
        MySqlBackend {
            config: self.config.clone(),
            executor: Arc::new(SessionPostgresExecutor::new(self.connection_string.clone())),
            next_statement_id: 1,
            prepared: HashMap::new(),
            connection_id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
            current_db: default_database_name(),
            last_insert_id: 0,
            session_charset: "utf8mb4".to_string(),
            session_collation: "utf8mb4_general_ci".to_string(),
            session_sql_mode: "NO_AUTO_VALUE_ON_ZERO".to_string(),
            transaction_isolation: "REPEATABLE-READ".to_string(),
        }
    }
}

static NEXT_CONNECTION_ID: AtomicU32 = AtomicU32::new(9);
static KILLED_CONNECTION_IDS: LazyLock<Mutex<HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static DEFAULT_DATABASE_NAME: LazyLock<Mutex<Option<String>>> =
    LazyLock::new(|| Mutex::new(None));
const MYSQL_COMPAT_VERSION: &str = "11.8.7-MariaDB-ubu2404";
const MYSQL_COMPAT_VERSION_COMMENT: &str = "MariaDB Server";
static SELECT_SYSTEM_VARIABLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?ix)
        ^\s*SELECT\s+
        @@(?:(SESSION|GLOBAL)\.)?([A-Z_][A-Z0-9_]*)
        (?:\s+(?:AS\s+)?`?([A-Z_][A-Z0-9_]*)`?)?
        \s*;?\s*$
        "#,
    )
    .expect("valid system variable SELECT regex")
});

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
    connection_id: u32,
    current_db: Option<String>,
    last_insert_id: u64,
    session_charset: String,
    session_collation: String,
    session_sql_mode: String,
    transaction_isolation: String,
}

#[async_trait]
impl<W> AsyncMysqlShim<W> for MySqlBackend
where
    W: tokio::io::AsyncWrite + Send + Unpin,
{
    type Error = MySqlServerError;

    fn version(&self) -> String {
        MYSQL_COMPAT_VERSION.to_string()
    }

    fn connect_id(&self) -> u32 {
        self.connection_id
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
        set_default_database_name(self.current_db.as_deref());
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

        if let Some((columns, rows)) = dynamic_canned_response_for_query(
            self.connection_id,
            self.current_db.as_deref(),
            &self.session_collation,
            &self.session_sql_mode,
            &self.transaction_isolation,
            query,
        ) {
            let statement_id = self.next_statement_id;
            self.next_statement_id += 1;
            self.prepared.insert(
                statement_id,
                PreparedStatement {
                    original_sql: query.to_string(),
                    postgres_sql: String::new(),
                    params: Vec::new(),
                    columns: columns.iter().map(|name| make_string_column(name)).collect(),
                    canned_rows: Some((columns, rows)),
                },
            );

            let params: Vec<&Column> = Vec::new();
            let columns: Vec<&Column> = self.prepared[&statement_id].columns.iter().collect();
            info.reply(statement_id, params, columns).await?;
            return Ok(());
        }

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
            match self
                .executor
                .describe_prepared_sql_in_schema(active_schema_name(self.current_db.as_deref()), &postgres_sql)
                .await
            {
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
        abort_if_connection_killed(self.connection_id)?;
        let Some(statement) = self.prepared.get(&id) else {
            results
                .error(ErrorKind::ER_UNKNOWN_STMT_HANDLER, b"unknown prepared statement id")
                .await?;
            return Ok(());
        };
        let original_sql = statement.original_sql.clone();
        let postgres_sql = statement.postgres_sql.clone();
        let params_len = statement.params.len();
        let statement_columns = statement.columns.clone();
        let canned_rows = statement.canned_rows.clone();
        tracing::info!("mysql execute stmt_id={id}");

        if let Some((columns, rows)) = &canned_rows {
            return write_canned_result(results, columns, rows).await;
        }

        if postgres_sql.is_empty() {
            match handle_mysql_session_query(self, &original_sql) {
                Ok(Some(response)) => {
                    results.completed(response).await?;
                    return Ok(());
                }
                Ok(None) => {}
                Err(err) => {
                    let msg = err.to_string();
                    results.error(ErrorKind::ER_UNKNOWN_ERROR, msg.as_bytes()).await?;
                    return Ok(());
                }
            }
            if is_compat_noop_query(&original_sql) {
                results.completed(OkResponse::default()).await?;
                return Ok(());
            }
        }

        let bound_params = match decode_mysql_params(params) {
            Ok(params) => params,
            Err(err) => {
                tracing::warn!("mysql execute bind decode failed for stmt_id={id}: {}", err);
                results.error(ErrorKind::ER_PARSE_ERROR, err.to_string().as_bytes()).await?;
                return Ok(());
            }
        };
        if params_len != bound_params.len() {
            let msg = format!(
                "prepared statement parameter count mismatch: expected {}, got {}",
                params_len,
                bound_params.len()
            );
            results.error(ErrorKind::ER_PARSE_ERROR, msg.as_bytes()).await?;
            return Ok(());
        }
        tracing::info!("mysql execute postgres_sql: {}", postgres_sql);

        let execution = if bound_params.is_empty() && postgres_sql.contains(';') {
            execute_multi_statement_sql(
                self.executor.as_ref(),
                active_schema_name(self.current_db.as_deref()),
                &postgres_sql,
            )
            .await
        } else {
            self.executor
                .execute_prepared_sql_in_schema(
                    active_schema_name(self.current_db.as_deref()),
                    &postgres_sql,
                    &bound_params,
                )
                .await
        };

        match execution {
            Ok(mut query_result) => {
                if query_result.last_insert_id > 0 {
                    self.last_insert_id = query_result.last_insert_id;
                }
                if !statement_columns.is_empty() && !query_result.columns.is_empty() {
                    query_result.columns = statement_columns
                        .iter()
                        .map(|column| column.column.clone())
                        .collect();
                }
                write_query_result(results, query_result).await
            }
            Err(err) => {
                tracing::warn!(
                    "mysql execute failed stmt_id={id} sql=`{}`: {}",
                    postgres_sql,
                    err
                );
                if let Some(query_result) =
                    compat_empty_result_for_missing_matomo_option(
                        &original_sql,
                        &postgres_sql,
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
        abort_if_connection_killed(self.connection_id)?;
        let trimmed = query.trim();
        tracing::info!("mysql query: {}", trimmed);

        if let Some(killed_id) = parse_kill_connection_id(trimmed) {
            kill_connection(killed_id);
            results.completed(OkResponse::default()).await?;
            return Ok(());
        }

        if let Some((columns, rows)) = dynamic_canned_response_for_query(
            self.connection_id,
            self.current_db.as_deref(),
            &self.session_collation,
            &self.session_sql_mode,
            &self.transaction_isolation,
            trimmed,
        ) {
            write_canned_result(results, &columns, &rows).await?;
            return Ok(());
        }

        match handle_mysql_session_query(self, trimmed) {
            Ok(Some(response)) => {
                results.completed(response).await?;
                return Ok(());
            }
            Ok(None) => {}
            Err(err) => {
                let msg = err.to_string();
                results.error(ErrorKind::ER_UNKNOWN_ERROR, msg.as_bytes()).await?;
                return Ok(());
            }
        }

        if is_compat_noop_query(trimmed) {
            if let Some(database_name) = parse_create_database_name(trimmed) {
                self.executor.create_schema(&database_name).await?;
                self.current_db = Some(database_name.clone());
                set_default_database_name(Some(&database_name));
                results.completed(OkResponse::default()).await?;
                return Ok(());
            }
            if let Some(database_name) = parse_drop_database_name(trimmed) {
                self.executor.drop_schema(&database_name).await?;
                if self.current_db.as_deref() == Some(database_name.as_str()) {
                    self.current_db = None;
                }
                clear_default_database_name_if_matches(&database_name);
                results.completed(OkResponse::default()).await?;
                return Ok(());
            }
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

        match self
            .executor
            .execute_sql_in_schema(active_schema_name(self.current_db.as_deref()), &translated)
            .await
        {
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
    schema: Option<&str>,
    sql: &str,
) -> Result<QueryResult, MiddlewareError> {
    let mut total_row_count = 0u64;

    for statement in sql.split(';').map(str::trim).filter(|part| !part.is_empty()) {
        let result = executor.execute_sql_in_schema(schema, statement).await?;
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
    let missing_option_table =
        err_text.contains("relation \"matomo_option\" does not exist")
            || err_text.contains("relation \"option\" does not exist");
    if !missing_option_table {
        return None;
    }

    let normalized = rendered_sql.trim_start().to_ascii_uppercase();
    let targets_option_table =
        rendered_sql.contains("\"matomo_option\"") || rendered_sql.contains("\"option\"");
    if !targets_option_table {
        return None;
    }

    if normalized.starts_with("SELECT") {
        let columns = infer_result_columns(original_sql);
        return Some(QueryResult {
            columns,
            rows: Vec::new(),
            row_count: 0,
            last_insert_id: 0,
        });
    }

    if normalized.starts_with("DELETE") || normalized.starts_with("UPDATE") {
        return Some(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            row_count: 0,
            last_insert_id: 0,
        });
    }

    None
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
    if normalized.eq_ignore_ascii_case("SELECT VERSION()") {
        return Some((
            vec!["VERSION()".to_string()],
            vec![vec![MYSQL_COMPAT_VERSION.to_string()]],
        ));
    }
    if normalized.eq_ignore_ascii_case("SELECT @@VERSION") {
        return Some((
            vec!["@@VERSION".to_string()],
            vec![vec![MYSQL_COMPAT_VERSION.to_string()]],
        ));
    }
    if normalized.eq_ignore_ascii_case("SELECT @@VERSION_COMMENT") {
        return Some((
            vec!["@@VERSION_COMMENT".to_string()],
            vec![vec![MYSQL_COMPAT_VERSION_COMMENT.to_string()]],
        ));
    }
    if normalized.eq_ignore_ascii_case("SELECT @@SESSION.SQL_MODE")
        || normalized.eq_ignore_ascii_case("SELECT @@SQL_MODE")
    {
        return Some((
            vec![normalized.to_string()],
            vec![vec!["NO_AUTO_VALUE_ON_ZERO".to_string()]],
        ));
    }
    None
}

fn dynamic_canned_response_for_query(
    connection_id: u32,
    current_db: Option<&str>,
    session_collation: &str,
    session_sql_mode: &str,
    transaction_isolation: &str,
    query: &str,
) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let normalized = query.trim();
    if let Some(response) = dynamic_canned_response_for_system_variable_select(
        session_collation,
        session_sql_mode,
        transaction_isolation,
        normalized,
    ) {
        return Some(response);
    }

    if normalized.eq_ignore_ascii_case("SELECT CONNECTION_ID()") {
        return Some((
            vec!["CONNECTION_ID()".to_string()],
            vec![vec![connection_id.to_string()]],
        ));
    }
    if normalized.eq_ignore_ascii_case("SELECT DATABASE()") {
        return Some((
            vec!["DATABASE()".to_string()],
            vec![vec![current_db.unwrap_or_default().to_string()]],
        ));
    }
    if normalized.eq_ignore_ascii_case("SELECT @@COLLATION_CONNECTION") {
        return Some((
            vec!["@@collation_connection".to_string()],
            vec![vec![session_collation.to_string()]],
        ));
    }
    if normalized.eq_ignore_ascii_case("SHOW GLOBAL VARIABLES LIKE 'T%_ISOLATION'")
        || normalized.eq_ignore_ascii_case("SHOW GLOBAL VARIABLES LIKE 't%_isolation'")
    {
        return Some((
            vec!["Variable_name".to_string(), "Value".to_string()],
            vec![vec![
                "transaction_isolation".to_string(),
                transaction_isolation.to_string(),
            ]],
        ));
    }
    None
}

fn dynamic_canned_response_for_system_variable_select(
    session_collation: &str,
    session_sql_mode: &str,
    transaction_isolation: &str,
    query: &str,
) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let captures = SELECT_SYSTEM_VARIABLE_RE.captures(query)?;
    let scope = captures.get(1).map(|value| value.as_str());
    let variable_match = captures.get(2)?;
    let variable = variable_match.as_str().to_ascii_lowercase();
    let alias = captures.get(3).map(|value| value.as_str().to_string());

    let value = match variable.as_str() {
        "sql_mode" => session_sql_mode.to_string(),
        "collation_connection" => session_collation.to_string(),
        "transaction_isolation" | "tx_isolation" => transaction_isolation.to_string(),
        "version" => MYSQL_COMPAT_VERSION.to_string(),
        "version_comment" => MYSQL_COMPAT_VERSION_COMMENT.to_string(),
        _ => return None,
    };

    let column_name = alias.unwrap_or_else(|| {
        if let Some(scope) = scope {
            format!("@@{scope}.{}", variable_match.as_str())
        } else {
            format!("@@{}", variable_match.as_str())
        }
    });

    Some((vec![column_name], vec![vec![value]]))
}

fn parse_kill_connection_id(query: &str) -> Option<u32> {
    let trimmed = query.trim().trim_end_matches(';').trim();
    let upper = trimmed.to_ascii_uppercase();
    let id = upper.strip_prefix("KILL ")?.trim();
    id.parse().ok()
}

fn kill_connection(connection_id: u32) {
    if let Ok(mut killed) = KILLED_CONNECTION_IDS.lock() {
        killed.insert(connection_id);
    }
}

fn abort_if_connection_killed(connection_id: u32) -> Result<(), MySqlServerError> {
    let Ok(mut killed) = KILLED_CONNECTION_IDS.lock() else {
        return Ok(());
    };
    if killed.remove(&connection_id) {
        return Err(MySqlServerError::Io(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "MySQL server has gone away",
        )));
    }
    Ok(())
}

fn handle_mysql_session_query(
    backend: &mut MySqlBackend,
    query: &str,
) -> Result<Option<OkResponse>, MySqlServerError> {
    let trimmed = query.trim().trim_end_matches(';').trim();
    let upper = trimmed.to_ascii_uppercase();

    if upper.starts_with("SET NAMES ") {
        apply_set_names(backend, trimmed)?;
        return Ok(Some(OkResponse::default()));
    }

    if is_set_sql_mode_query(&upper) {
        backend.session_sql_mode = parse_set_sql_mode_value(trimmed);
        return Ok(Some(OkResponse::default()));
    }

    if upper.starts_with("SET SESSION TRANSACTION ISOLATION LEVEL ") {
        let level = trimmed["SET SESSION TRANSACTION ISOLATION LEVEL ".len()..].trim();
        backend.transaction_isolation = normalize_transaction_isolation(level);
        return Ok(Some(OkResponse::default()));
    }

    if upper.starts_with("SET TRANSACTION ISOLATION LEVEL ") {
        let level = trimmed["SET TRANSACTION ISOLATION LEVEL ".len()..].trim();
        backend.transaction_isolation = normalize_transaction_isolation(level);
        return Ok(Some(OkResponse::default()));
    }

    Ok(None)
}

fn apply_set_names(backend: &mut MySqlBackend, sql: &str) -> Result<(), MySqlServerError> {
    let payload = sql["SET NAMES ".len()..].trim();
    let mut parts = payload.splitn(2, char::is_whitespace);
    let charset = normalize_mysql_charset(
        parts
        .next()
        .unwrap_or_default()
        .trim_matches('\'')
        .trim_matches('"')
        .trim(),
    );

    if default_mysql_collation_for_charset(&charset).is_none() {
        return Err(MySqlServerError::Middleware(MiddlewareError::Execution(format!(
            "unknown character set: {charset}"
        ))));
    }

    let mut collation = None;
    if let Some(rest) = parts.next() {
        let rest = rest.trim();
        if !rest.is_empty() {
            let upper = rest.to_ascii_uppercase();
            if let Some(value) = upper.strip_prefix("COLLATE ") {
                let original = &rest[rest.len() - value.len()..];
                collation = Some(
                    original
                        .trim()
                        .trim_matches('\'')
                        .trim_matches('"')
                        .to_ascii_lowercase(),
                );
            }
        }
    }

    if let Some(collation) = collation {
        let collation = normalize_mysql_collation(&charset, &collation)?;
        if !collation_matches_charset(&charset, &collation) {
            return Err(MySqlServerError::Middleware(MiddlewareError::Execution(format!(
                "unknown collation: {collation}"
            ))));
        }
        backend.session_collation = collation;
    } else if let Some(default_collation) = default_mysql_collation_for_charset(&charset) {
        backend.session_collation = default_collation.to_string();
    }

    backend.session_charset = charset;
    Ok(())
}

fn normalize_mysql_charset(charset: &str) -> String {
    match charset.to_ascii_lowercase().as_str() {
        "utf8mb3" => "utf8".to_string(),
        other => other.to_string(),
    }
}

fn normalize_mysql_collation(charset: &str, collation: &str) -> Result<String, MySqlServerError> {
    let collation = collation.to_ascii_lowercase();
    if collation == "default" || matches!(collation.as_str(), "utf8" | "utf8mb3" | "utf8mb4") {
        return default_mysql_collation_for_charset(charset)
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                MySqlServerError::Middleware(MiddlewareError::Execution(format!(
                    "unknown character set: {charset}"
                )))
            });
    }

    Ok(collation)
}

fn default_mysql_collation_for_charset(charset: &str) -> Option<&'static str> {
    match charset {
        "utf8" => Some("utf8_general_ci"),
        "utf8mb4" => Some("utf8mb4_general_ci"),
        _ => None,
    }
}

fn collation_matches_charset(charset: &str, collation: &str) -> bool {
    collation.starts_with(&(charset.to_string() + "_"))
}

fn normalize_transaction_isolation(level: &str) -> String {
    level
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_uppercase()
}

fn is_set_sql_mode_query(upper: &str) -> bool {
    upper.starts_with("SET SQL_MODE")
        || upper.starts_with("SET SESSION SQL_MODE")
        || upper.starts_with("SET @@SQL_MODE")
        || upper.starts_with("SET @@SESSION.SQL_MODE")
}

fn parse_set_sql_mode_value(sql: &str) -> String {
    let Some((_, value)) = sql.split_once('=') else {
        return String::new();
    };

    value
        .trim()
        .trim_end_matches(';')
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .to_string()
}

fn is_compat_noop_query(query: &str) -> bool {
    let normalized = query.trim().to_ascii_uppercase();
    normalized.starts_with("SET NAMES ")
        || normalized.starts_with("SET SQL_MODE")
        || is_set_sql_mode_query(&normalized)
        || normalized.starts_with("CREATE DATABASE ")
        || normalized.starts_with("DROP DATABASE ")
        || is_mysql_session_compat_noop(&normalized)
}

fn active_schema_name(current_db: Option<&str>) -> Option<&str> {
    current_db.filter(|db| !db.trim().is_empty())
}

fn default_database_name() -> Option<String> {
    DEFAULT_DATABASE_NAME.lock().ok().and_then(|db| db.clone())
}

fn set_default_database_name(db_name: Option<&str>) {
    if let Ok(mut current) = DEFAULT_DATABASE_NAME.lock() {
        *current = db_name
            .map(str::trim)
            .filter(|db| !db.is_empty())
            .map(ToOwned::to_owned);
    }
}

fn clear_default_database_name_if_matches(db_name: &str) {
    if let Ok(mut current) = DEFAULT_DATABASE_NAME.lock() {
        if current.as_deref() == Some(db_name) {
            *current = None;
        }
    }
}

fn parse_database_name(sql: &str, prefix: &str) -> Option<String> {
    let rest = sql.trim().strip_prefix(prefix)?.trim();
    let rest = rest
        .strip_prefix("IF NOT EXISTS ")
        .or_else(|| rest.strip_prefix("IF EXISTS "))
        .unwrap_or(rest)
        .trim();
    let ident = rest
        .split_whitespace()
        .next()?
        .trim_matches(';')
        .trim_matches('`')
        .trim_matches('"');
    if ident.is_empty() {
        None
    } else {
        Some(ident.to_string())
    }
}

fn parse_create_database_name(sql: &str) -> Option<String> {
    parse_database_name(sql, "CREATE DATABASE")
        .or_else(|| parse_database_name(sql, "create database"))
}

fn parse_drop_database_name(sql: &str) -> Option<String> {
    parse_database_name(sql, "DROP DATABASE").or_else(|| parse_database_name(sql, "drop database"))
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
        "SET SESSION TRANSACTION ISOLATION LEVEL",
        "SET TRANSACTION ISOLATION LEVEL",
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
        assert_eq!(rows, vec![vec!["NO_AUTO_VALUE_ON_ZERO".to_string()]]);
    }

    #[test]
    fn dynamic_canned_response_supports_aliased_sql_mode_selects() {
        let (columns, rows) = dynamic_canned_response_for_query(
            10,
            Some("latest_stable"),
            "utf8mb4_general_ci",
            "NO_AUTO_VALUE_ON_ZERO",
            "REPEATABLE-READ",
            "SELECT @@sql_mode AS sql_mode",
        )
        .unwrap();
        assert_eq!(columns, vec!["sql_mode".to_string()]);
        assert_eq!(rows, vec![vec!["NO_AUTO_VALUE_ON_ZERO".to_string()]]);

        let (columns, rows) = dynamic_canned_response_for_query(
            10,
            Some("latest_stable"),
            "utf8mb4_general_ci",
            "ANSI_QUOTES",
            "REPEATABLE-READ",
            "SELECT @@SESSION.sql_mode",
        )
        .unwrap();
        assert_eq!(columns, vec!["@@SESSION.sql_mode".to_string()]);
        assert_eq!(rows, vec![vec!["ANSI_QUOTES".to_string()]]);
    }

    #[test]
    fn dynamic_canned_response_preserves_system_variable_column_case() {
        let (columns, rows) = dynamic_canned_response_for_query(
            10,
            Some("latest_stable"),
            "utf8mb4_general_ci",
            "NO_AUTO_VALUE_ON_ZERO",
            "REPEATABLE-READ",
            "SELECT @@VERSION",
        )
        .unwrap();

        assert_eq!(columns, vec!["@@VERSION".to_string()]);
        assert_eq!(rows, vec![vec!["11.8.7-MariaDB-ubu2404".to_string()]]);
    }

    #[test]
    fn set_sql_mode_variants_are_session_compat_queries() {
        assert!(is_compat_noop_query("SET SESSION sql_mode = 'ANSI_QUOTES'"));
        assert!(is_compat_noop_query("SET @@sql_mode = ''"));
        assert!(is_compat_noop_query(
            "SET @@SESSION.sql_mode = 'NO_AUTO_VALUE_ON_ZERO'"
        ));
    }

    #[test]
    fn charset_style_collation_values_map_to_mysql_defaults() {
        assert_eq!(normalize_mysql_charset("utf8mb3"), "utf8");
        assert_eq!(
            normalize_mysql_collation("utf8", "utf8").unwrap(),
            "utf8_general_ci"
        );
        assert_eq!(
            normalize_mysql_collation("utf8mb4", "utf8").unwrap(),
            "utf8mb4_general_ci"
        );
        assert_eq!(
            normalize_mysql_collation("utf8mb4", "DEFAULT").unwrap(),
            "utf8mb4_general_ci"
        );
        assert!(collation_matches_charset("utf8", "utf8_general_ci"));
        assert!(collation_matches_charset("utf8mb4", "utf8mb4_general_ci"));
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
