use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::AppConfig,
    executor::{build_executor, PostgresExecutor, QueryResult},
    mysql_server::{serve_mysql, MySqlFrontendFactory},
    translator::{translate_sql, TranslationResult},
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub executor: Arc<dyn PostgresExecutor>,
}

#[derive(Debug, Deserialize)]
pub struct SqlRequest {
    pub sql: String,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub http_bind_addr: String,
    pub mysql_bind_addr: String,
}

#[derive(Debug, Serialize)]
pub struct ExecuteResponse {
    pub translation: TranslationResult,
    pub execution: QueryResult,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub async fn serve(config: AppConfig) -> anyhow::Result<()> {
    let http_bind_addr = config.server.bind_addr.clone();
    let mysql_bind_addr = config.server.mysql_bind_addr.clone();
    let shared_config = Arc::new(config);
    let executor = build_executor(shared_config.as_ref())?;

    let http_state = AppState {
        executor: executor.clone(),
        config: shared_config.clone(),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/translate", post(translate))
        .route("/execute", post(execute))
        .with_state(http_state);

    let http_listener = tokio::net::TcpListener::bind(&http_bind_addr).await?;
    let mysql_factory = MySqlFrontendFactory::new(shared_config.clone(), executor);

    tracing::info!("http frontend listening on {}", http_bind_addr);
    tracing::info!("mysql-compatible frontend listening on {}", mysql_bind_addr);

    let http_task = async move {
        axum::serve(http_listener, app).await?;
        Ok::<(), anyhow::Error>(())
    };

    let mysql_task = async move {
        serve_mysql(mysql_factory, mysql_bind_addr).await?;
        Ok::<(), anyhow::Error>(())
    };

    let (_http_result, _mysql_result) = tokio::try_join!(http_task, mysql_task)?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        http_bind_addr: state.config.server.bind_addr.clone(),
        mysql_bind_addr: state.config.server.mysql_bind_addr.clone(),
    })
}

async fn translate(
    State(state): State<AppState>,
    Json(payload): Json<SqlRequest>,
) -> impl IntoResponse {
    match translate_sql(&payload.sql, &state.config.translator) {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: err.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn execute(
    State(state): State<AppState>,
    Json(payload): Json<SqlRequest>,
) -> impl IntoResponse {
    let result = match translate_sql(&payload.sql, &state.config.translator) {
        Ok(result) => result,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: err.to_string(),
                }),
            )
                .into_response()
        }
    };

    match state.executor.execute_sql(&result.translated_sql).await {
        Ok(query_result) => (
            StatusCode::OK,
            Json(ExecuteResponse {
                translation: result,
                execution: query_result,
            }),
        )
            .into_response(),
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: err.to_string(),
            }),
        )
            .into_response(),
    }
}
