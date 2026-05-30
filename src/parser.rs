use sqlparser::{dialect::MySqlDialect, parser::Parser};

use crate::error::MiddlewareError;

pub fn parse_mysql_sql(sql: &str) -> Result<Vec<sqlparser::ast::Statement>, MiddlewareError> {
    Parser::parse_sql(&MySqlDialect {}, sql)
        .map_err(|e| MiddlewareError::Parse(format!("{e}")))
}
