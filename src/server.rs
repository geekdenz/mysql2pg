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
    let bind_addr = config.server.bind_addr.clone();
    let state = AppState {
        executor: build_executor(&config)?,
        config: Arc::new(config),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/translate", post(translate))
        .route("/execute", post(execute))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("mysql2pg-middleware listening on {}", bind_addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
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
