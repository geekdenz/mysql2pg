use thiserror::Error;

#[derive(Debug, Error)]
pub enum MiddlewareError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("translation error: {0}")]
    Translation(String),

    #[error("execution error: {0}")]
    Execution(String),
}
