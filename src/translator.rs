use regex::{Captures, Regex};
use serde::Serialize;

use crate::{config::TranslatorConfig, error::MiddlewareError, parser::parse_mysql_sql};

#[derive(Debug, Clone, Serialize)]
pub struct TranslationResult {
    pub original_sql: String,
    pub canonical_mysql_sql: String,
    pub translated_sql: String,
    pub warnings: Vec<String>,
}

pub fn translate_sql(sql: &str, cfg: &TranslatorConfig) -> Result<TranslationResult, MiddlewareError> {
    let statements = parse_mysql_sql(sql)?;
    if statements.is_empty() {
        return Err(MiddlewareError::Translation("no statements were parsed".to_string()));
    }

    let canonical_mysql_sql = statements
        .iter()
        .map(|stmt| stmt.to_string())
        .collect::<Vec<_>>()
        .join("; ");

    let mut warnings = Vec::new();
    let mut translated = canonical_mysql_sql.clone();

    if cfg.normalize_mysql_backticks {
        translated = replace_backticks(&translated);
    }
    if cfg.rewrite_limit_comma {
        translated = rewrite_limit_offset_count(&translated, &mut warnings);
    }
    if cfg.normalize_boolean_literals {
        translated = rewrite_boolean_literals(&translated);
    }
    if cfg.rewrite_mysql_functions {
        translated = rewrite_mysql_functions(&translated, &mut warnings);
    }
    if cfg.rewrite_json_operators {
        translated = rewrite_json_extract(&translated, &mut warnings);
    }
    if cfg.strip_mysql_table_options {
        translated = strip_mysql_table_options(&translated, &mut warnings);
    }

    reject_unsupported(&translated)?;

    Ok(TranslationResult {
        original_sql: sql.to_string(),
        canonical_mysql_sql,
        translated_sql: translated,
        warnings,
    })
}

fn replace_backticks(sql: &str) -> String {
    sql.replace('`', "\"")
}

fn rewrite_limit_offset_count(sql: &str, warnings: &mut Vec<String>) -> String {
    let re = Regex::new(r"(?i)\bLIMIT\s+(\d+)\s*,\s*(\d+)\b").expect("valid regex");
    let changed = re.is_match(sql);
    let out = re.replace_all(sql, "LIMIT $2 OFFSET $1").to_string();
    if changed {
        warnings.push("rewrote MySQL LIMIT offset,count to PostgreSQL LIMIT count OFFSET offset".to_string());
    }
    out
}

fn rewrite_boolean_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let mut idx = 0;
    let mut quote: Option<char> = None;

    while idx < chars.len() {
        let ch = chars[idx];

        if let Some(active_quote) = quote {
            out.push(ch);
            idx += 1;

            if ch == active_quote {
                if idx < chars.len() && chars[idx] == active_quote {
                    out.push(chars[idx]);
                    idx += 1;
                } else {
                    quote = None;
                }
            }
            continue;
        }

        if matches!(ch, '\'' | '"' | '`') {
            quote = Some(ch);
            out.push(ch);
            idx += 1;
            continue;
        }

        if is_identifier_char(ch) {
            let start = idx;
            idx += 1;
            while idx < chars.len() && is_identifier_char(chars[idx]) {
                idx += 1;
            }

            let token = chars[start..idx].iter().collect::<String>();
            if token.eq_ignore_ascii_case("true") {
                out.push_str("TRUE");
            } else if token.eq_ignore_ascii_case("false") {
                out.push_str("FALSE");
            } else {
                out.push_str(&token);
            }
            continue;
        }

        out.push(ch);
        idx += 1;
    }

    out
}

fn is_identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn rewrite_mysql_functions(sql: &str, warnings: &mut Vec<String>) -> String {
    let mut out = sql.to_string();

    let replacements = [
        (r"(?i)\bIFNULL\s*\(", "COALESCE("),
        (r"(?i)\bNOW\s*\(", "CURRENT_TIMESTAMP("),
        (r"(?i)\bRAND\s*\(", "RANDOM("),
    ];

    for (pattern, replacement) in replacements {
        let re = Regex::new(pattern).expect("valid regex");
        if re.is_match(&out) {
            warnings.push(format!("rewrote MySQL function pattern `{pattern}`"));
            out = re.replace_all(&out, replacement).to_string();
        }
    }

    let unix_ts_re = Regex::new(r"(?i)\bUNIX_TIMESTAMP\s*\(([^\)]*)\)").expect("valid regex");
    if unix_ts_re.is_match(&out) {
        warnings.push("rewrote UNIX_TIMESTAMP(expr) to EXTRACT(EPOCH FROM expr)".to_string());
        out = unix_ts_re.replace_all(&out, |caps: &Captures| {
            let expr = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            if expr.is_empty() {
                "EXTRACT(EPOCH FROM CURRENT_TIMESTAMP)".to_string()
            } else {
                format!("EXTRACT(EPOCH FROM {expr})")
            }
        }).to_string();
    }

    let from_unixtime_re = Regex::new(r"(?i)\bFROM_UNIXTIME\s*\(([^\)]*)\)").expect("valid regex");
    if from_unixtime_re.is_match(&out) {
        warnings.push("rewrote FROM_UNIXTIME(expr) to TO_TIMESTAMP(expr)".to_string());
        out = from_unixtime_re.replace_all(&out, |caps: &Captures| {
            let expr = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            format!("TO_TIMESTAMP({expr})")
        }).to_string();
    }

    out
}

fn rewrite_json_extract(sql: &str, warnings: &mut Vec<String>) -> String {
    let re = Regex::new(r#"(?i)JSON_EXTRACT\s*\(\s*([A-Za-z0-9_\.\"]+)\s*,\s*'\$\.([^']+)'\s*\)"#).expect("valid regex");
    if re.is_match(sql) {
        warnings.push("rewrote JSON_EXTRACT(col, '$.path') to PostgreSQL jsonb #>> '{path}' form where possible".to_string());
    }
    re.replace_all(sql, |caps: &Captures| {
        let col = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let path = caps.get(2)
            .map(|m| m.as_str().split('.').collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        format!("{col} #>> '{{{path}}}'")
    }).to_string()
}

fn strip_mysql_table_options(sql: &str, warnings: &mut Vec<String>) -> String {
    let re = Regex::new(r"(?i)\)\s*ENGINE\s*=\s*\w+(?:\s+DEFAULT)?(?:\s+CHARSET\s*=\s*\w+)?").expect("valid regex");
    if re.is_match(sql) {
        warnings.push("stripped MySQL table ENGINE/CHARSET options".to_string());
    }
    re.replace_all(sql, ")").to_string()
}

fn reject_unsupported(sql: &str) -> Result<(), MiddlewareError> {
    let unsupported = [
        (r"(?i)\bREPLACE\s+INTO\b", "REPLACE INTO is MySQL-specific; use INSERT ... ON CONFLICT in PostgreSQL"),
        (r"(?i)\bON\s+DUPLICATE\s+KEY\s+UPDATE\b", "ON DUPLICATE KEY UPDATE needs table/key-specific ON CONFLICT translation"),
        (r"(?i)\bSQL_CALC_FOUND_ROWS\b", "SQL_CALC_FOUND_ROWS is not supported in PostgreSQL"),
        (r"(?i)\bSTRAIGHT_JOIN\b", "STRAIGHT_JOIN is MySQL-specific"),
        (r"(?i)\bLOCK\s+IN\s+SHARE\s+MODE\b", "LOCK IN SHARE MODE is MySQL-specific; use FOR SHARE in PostgreSQL when applicable"),
        (r"(?i)\bAUTO_INCREMENT\b", "AUTO_INCREMENT in DDL requires SERIAL/IDENTITY-aware transformation not yet implemented"),
        (r"(?i)\bUNSIGNED\b", "UNSIGNED numeric types need schema-aware conversion in PostgreSQL"),
    ];

    for (pattern, message) in unsupported {
        let re = Regex::new(pattern).expect("valid regex");
        if re.is_match(sql) {
            return Err(MiddlewareError::Translation(message.to_string()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn translates_limit_offset() {
        let result = translate_sql("SELECT * FROM users LIMIT 5, 10", &TranslatorConfig::default()).unwrap();
        assert_eq!(result.translated_sql, "SELECT * FROM users LIMIT 10 OFFSET 5");
    }

    #[test]
    fn rewrites_common_functions() {
        let result = translate_sql(
            "SELECT IFNULL(name, 'x'), FROM_UNIXTIME(created_at), UNIX_TIMESTAMP(updated_at), RAND() FROM users",
            &TranslatorConfig::default(),
        ).unwrap();
        assert!(result.translated_sql.contains("COALESCE(name, 'x')"));
        assert!(result.translated_sql.contains("TO_TIMESTAMP(created_at)"));
        assert!(result.translated_sql.contains("EXTRACT(EPOCH FROM updated_at)"));
        assert!(result.translated_sql.contains("RANDOM()"));
    }

    #[test]
    fn leaves_string_literals_unchanged_when_normalizing_booleans() {
        let result = translate_sql("SELECT true, false, 'true', \"false_value\" FROM flags", &TranslatorConfig::default()).unwrap();
        assert!(result.translated_sql.contains("SELECT TRUE, FALSE, 'true', \"false_value\" FROM flags"));
    }

    #[test]
    fn simple_select_does_not_panic_during_boolean_normalization() {
        let result = translate_sql("select 1", &TranslatorConfig::default()).unwrap();
        assert_eq!(result.translated_sql, "SELECT 1");
    }
}
