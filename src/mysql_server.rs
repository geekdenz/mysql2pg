use std::{collections::HashMap, io, sync::Arc};

use async_trait::async_trait;
use opensrv_mysql::{
    AsyncMysqlIntermediary, AsyncMysqlShim, Column, ColumnFlags, ColumnType, ErrorKind, IntermediaryOptions,
    OkResponse, ParamParser, QueryResultWriter, StatementMetaWriter,
};
use tokio::{io::split, net::TcpListener};

use crate::{
    config::AppConfig,
    error::MiddlewareError,
    executor::{PostgresExecutor, QueryResult},
    translator::translate_sql,
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
    translated_sql: String,
}

struct MySqlBackend {
    config: Arc<AppConfig>,
    executor: Arc<dyn PostgresExecutor>,
    next_statement_id: u32,
    prepared: HashMap<u32, PreparedStatement>,
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

    async fn on_prepare<'a>(
        &'a mut self,
        query: &'a str,
        info: StatementMetaWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        let param_count = query.matches('?').count();
        if param_count > 0 {
            info.error(
                ErrorKind::ER_NOT_SUPPORTED_YET,
                b"prepared statements with bind parameters are not implemented yet; use text queries for this iteration",
            )
            .await?;
            return Ok(());
        }

        let translated = translate_sql(query, &self.config.translator)?.translated_sql;
        let statement_id = self.next_statement_id;
        self.next_statement_id += 1;
        self.prepared.insert(statement_id, PreparedStatement { translated_sql: translated });

        let params: Vec<&Column> = Vec::new();
        let columns: Vec<&Column> = Vec::new();
        info.reply(statement_id, params, columns).await?;
        Ok(())
    }

    async fn on_execute<'a>(
        &'a mut self,
        id: u32,
        _params: ParamParser<'a>,
        results: QueryResultWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        let Some(statement) = self.prepared.get(&id) else {
            results
                .error(ErrorKind::ER_UNKNOWN_STMT_HANDLER, b"unknown prepared statement id")
                .await?;
            return Ok(());
        };

        let query_result = self.executor.execute_sql(&statement.translated_sql).await?;
        write_query_result(results, query_result).await
    }

    async fn on_close<'a>(&'a mut self, stmt: u32)
    where
        W: 'async_trait,
    {
        self.prepared.remove(&stmt);
    }

    async fn on_query<'a>(
        &'a mut self,
        query: &'a str,
        results: QueryResultWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        let trimmed = query.trim();

        if trimmed.eq_ignore_ascii_case("SELECT VERSION()") {
            let columns = vec![make_string_column("VERSION()")];
            let mut writer = results.start(&columns).await?;
            writer.write_row(vec!["8.0.0-mysql2pg"]).await?;
            writer.finish().await?;
            return Ok(());
        }

        let translated = translate_sql(trimmed, &self.config.translator)?.translated_sql;
        let query_result = self.executor.execute_sql(&translated).await?;
        write_query_result(results, query_result).await
    }
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

fn make_string_column(name: &str) -> Column {
    Column {
        table: "".to_string(),
        column: name.to_string(),
        coltype: ColumnType::MYSQL_TYPE_VAR_STRING,
        colflags: ColumnFlags::empty(),
    }
}
