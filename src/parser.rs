use sqlparser::{
    dialect::{MySqlDialect, PostgreSqlDialect},
    parser::Parser,
};

use crate::error::MiddlewareError;

pub fn parse_mysql_sql(sql: &str) -> Result<Vec<sqlparser::ast::Statement>, MiddlewareError> {
    Parser::parse_sql(&MySqlDialect {}, sql)
        .map_err(|e| MiddlewareError::Parse(format!("{e}")))
}

/// Parse already-translated SQL, where double quotes delimit identifiers.
///
/// Parsing PostgreSQL output with the MySQL dialect would read `"users"."id"`
/// as a string literal rather than a qualified column.
pub fn parse_postgres_sql(sql: &str) -> Result<Vec<sqlparser::ast::Statement>, MiddlewareError> {
    Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| MiddlewareError::Parse(format!("{e}")))
}
