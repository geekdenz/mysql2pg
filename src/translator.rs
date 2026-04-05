use regex::{Captures, Regex};
use serde::Serialize;
use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateTable, DataType, ExactNumberInfo, ShowStatementFilter,
    ShowStatementFilterPosition, ShowStatementInParentType, ShowStatementOptions, Statement,
    TableConstraint, TimezoneInfo,
};

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
    let mut translated = translate_statements(&statements, &mut warnings)?;

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

fn translate_statements(
    statements: &[Statement],
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    statements
        .iter()
        .map(|stmt| translate_statement(stmt, warnings))
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("; "))
}

fn translate_statement(stmt: &Statement, warnings: &mut Vec<String>) -> Result<String, MiddlewareError> {
    match stmt {
        Statement::CreateTable(create) => translate_create_table(create, warnings),
        Statement::ShowTables {
            terse,
            history,
            extended,
            full,
            external,
            show_options,
        } => translate_show_tables(
            *terse,
            *history,
            *extended,
            *full,
            *external,
            show_options,
            warnings,
        ),
        _ => Ok(stmt.to_string()),
    }
}

fn translate_show_tables(
    terse: bool,
    history: bool,
    extended: bool,
    full: bool,
    external: bool,
    show_options: &ShowStatementOptions,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if terse || history || extended || external {
        return Err(MiddlewareError::Translation(
            "SHOW TABLES options TERSE/HISTORY/EXTENDED/EXTERNAL are not supported yet".to_string(),
        ));
    }

    if show_options.starts_with.is_some() || show_options.limit.is_some() || show_options.limit_from.is_some() {
        return Err(MiddlewareError::Translation(
            "SHOW TABLES STARTS WITH/LIMIT options are not supported yet".to_string(),
        ));
    }

    let (schema_expr, column_alias) = resolve_show_tables_schema(show_options)?;
    let object_type_expr = if full {
        "CASE WHEN table_type = 'VIEW' THEN 'VIEW' ELSE 'BASE TABLE' END AS \"Table_type\""
    } else {
        ""
    };

    let mut sql = format!(
        "SELECT table_name AS \"{column_alias}\"{} FROM information_schema.tables WHERE table_schema = {schema_expr} AND table_type IN ('BASE TABLE', 'VIEW')",
        if full { format!(", {object_type_expr}") } else { String::new() }
    );

    if let Some(filter_sql) = translate_show_tables_filter(show_options)? {
        sql.push_str(" AND ");
        sql.push_str(&filter_sql);
    }

    sql.push_str(" ORDER BY table_name");
    warnings.push("rewrote MySQL SHOW TABLES to information_schema query".to_string());
    Ok(sql)
}

fn resolve_show_tables_schema(show_options: &ShowStatementOptions) -> Result<(String, String), MiddlewareError> {
    let Some(show_in) = &show_options.show_in else {
        return Ok((
            "current_schema()".to_string(),
            "Tables_in_current_schema".to_string(),
        ));
    };

    match &show_in.parent_type {
        None | Some(ShowStatementInParentType::Schema) | Some(ShowStatementInParentType::Database) => {
            if show_in.parent_name.is_some() {
                let alias_name = show_in.parent_name.as_ref().unwrap().to_string();
                Ok((
                    format!("'{}'", alias_name.replace('\'', "''")),
                    format!("Tables_in_{alias_name}"),
                ))
            } else {
                Ok((
                    "current_schema()".to_string(),
                    "Tables_in_current_schema".to_string(),
                ))
            }
        }
        Some(other) => Err(MiddlewareError::Translation(format!(
            "SHOW TABLES {} is not supported yet",
            other
        ))),
    }
}

fn translate_show_tables_filter(
    show_options: &ShowStatementOptions,
) -> Result<Option<String>, MiddlewareError> {
    let Some(filter_position) = &show_options.filter_position else {
        return Ok(None);
    };

    let filter = match filter_position {
        ShowStatementFilterPosition::Infix(filter) | ShowStatementFilterPosition::Suffix(filter) => filter,
    };

    match filter {
        ShowStatementFilter::Like(pattern)
        | ShowStatementFilter::ILike(pattern)
        | ShowStatementFilter::NoKeyword(pattern) => Ok(Some(format!(
            "table_name LIKE '{}'",
            pattern.replace('\'', "''")
        ))),
        ShowStatementFilter::Where(_) => Err(MiddlewareError::Translation(
            "SHOW TABLES ... WHERE is not supported yet".to_string(),
        )),
    }
}

fn translate_create_table(
    create: &CreateTable,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if create.or_replace || create.temporary || create.external || create.dynamic || create.global.is_some()
        || create.transient || create.volatile || create.iceberg || create.query.is_some()
        || create.without_rowid || create.like.is_some() || create.clone.is_some()
        || create.version.is_some() || create.comment.is_some() || create.on_commit.is_some()
        || create.on_cluster.is_some() || create.primary_key.is_some() || create.order_by.is_some()
        || create.partition_by.is_some() || create.cluster_by.is_some() || create.clustered_by.is_some()
        || create.inherits.is_some() || create.partition_of.is_some() || create.for_values.is_some()
        || create.strict || create.copy_grants || create.enable_schema_evolution.is_some()
        || create.change_tracking.is_some() || create.data_retention_time_in_days.is_some()
        || create.max_data_extension_time_in_days.is_some() || create.default_ddl_collation.is_some()
        || create.with_aggregation_policy.is_some() || create.with_row_access_policy.is_some()
        || create.with_tags.is_some() || create.external_volume.is_some() || create.base_location.is_some()
        || create.catalog.is_some() || create.catalog_sync.is_some() || create.storage_serialization_policy.is_some()
        || create.target_lag.is_some() || create.warehouse.is_some() || create.refresh_mode.is_some()
        || create.initialize.is_some() || create.require_user
    {
        return Err(MiddlewareError::Translation(
            "complex CREATE TABLE variants are not yet supported for PostgreSQL translation".to_string(),
        ));
    }

    let mut rendered_items = Vec::new();
    let mut extra_constraints = Vec::new();

    for column in &create.columns {
        let (rendered, constraints) = translate_column(column, warnings)?;
        rendered_items.push(rendered);
        extra_constraints.extend(constraints);
    }

    for constraint in &create.constraints {
        rendered_items.push(translate_table_constraint(constraint)?);
    }
    rendered_items.extend(extra_constraints);

    if !matches!(create.table_options, sqlparser::ast::CreateTableOptions::None) {
        warnings.push("stripped MySQL-specific CREATE TABLE options".to_string());
    }

    let if_not_exists = if create.if_not_exists { "IF NOT EXISTS " } else { "" };
    Ok(format!(
        "CREATE TABLE {if_not_exists}{} ({})",
        create.name,
        rendered_items.join(", ")
    ))
}

fn translate_table_constraint(constraint: &TableConstraint) -> Result<String, MiddlewareError> {
    match constraint {
        TableConstraint::Unique(unique) => {
            let name = unique
                .name
                .as_ref()
                .map(|name| format!("CONSTRAINT {name} "))
                .unwrap_or_default();
            let columns = unique.columns.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
            let nulls_distinct = unique.nulls_distinct.to_string();
            let characteristics = unique
                .characteristics
                .as_ref()
                .map(|value| format!(" {value}"))
                .unwrap_or_default();
            Ok(format!("{name}UNIQUE{nulls_distinct} ({columns}){characteristics}"))
        }
        TableConstraint::PrimaryKey(primary_key) => {
            let name = primary_key
                .name
                .as_ref()
                .map(|name| format!("CONSTRAINT {name} "))
                .unwrap_or_default();
            let columns = primary_key
                .columns
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let characteristics = primary_key
                .characteristics
                .as_ref()
                .map(|value| format!(" {value}"))
                .unwrap_or_default();
            Ok(format!("{name}PRIMARY KEY ({columns}){characteristics}"))
        }
        TableConstraint::ForeignKey(foreign_key) => {
            let name = foreign_key
                .name
                .as_ref()
                .map(|name| format!("CONSTRAINT {name} "))
                .unwrap_or_default();
            let columns = foreign_key
                .columns
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let referred_columns = foreign_key
                .referred_columns
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let match_kind = foreign_key
                .match_kind
                .as_ref()
                .map(|value| format!(" {value}"))
                .unwrap_or_default();
            let on_delete = foreign_key
                .on_delete
                .as_ref()
                .map(|value| format!(" ON DELETE {value}"))
                .unwrap_or_default();
            let on_update = foreign_key
                .on_update
                .as_ref()
                .map(|value| format!(" ON UPDATE {value}"))
                .unwrap_or_default();
            let characteristics = foreign_key
                .characteristics
                .as_ref()
                .map(|value| format!(" {value}"))
                .unwrap_or_default();
            Ok(format!(
                "{name}FOREIGN KEY ({columns}) REFERENCES {} ({referred_columns}){match_kind}{on_delete}{on_update}{characteristics}",
                foreign_key.foreign_table
            ))
        }
        TableConstraint::Check(check) => {
            let name = check
                .name
                .as_ref()
                .map(|name| format!("CONSTRAINT {name} "))
                .unwrap_or_default();
            Ok(format!("{name}CHECK ({})", check.expr))
        }
        TableConstraint::Index(index) => Err(MiddlewareError::Translation(format!(
            "MySQL KEY/INDEX constraint `{}` inside CREATE TABLE is not translated yet; create the index separately in PostgreSQL",
            index.name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "<unnamed>".to_string())
        ))),
        TableConstraint::FulltextOrSpatial(index) => Err(MiddlewareError::Translation(format!(
            "MySQL FULLTEXT/SPATIAL constraint `{}` is not supported in PostgreSQL translation",
            index.opt_index_name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "<unnamed>".to_string())
        ))),
    }
}

fn translate_column(column: &ColumnDef, warnings: &mut Vec<String>) -> Result<(String, Vec<String>), MiddlewareError> {
    let mut extra_constraints = Vec::new();
    let mut auto_increment = false;
    let mut rendered_options = Vec::new();

    let translated_type = translate_data_type(&column.name.to_string(), &column.data_type, &mut extra_constraints, warnings);

    for option in &column.options {
        match &option.option {
            ColumnOption::DialectSpecific(tokens) if is_auto_increment(tokens) => {
                auto_increment = true;
                warnings.push(format!(
                    "rewrote AUTO_INCREMENT on column `{}` to PostgreSQL identity",
                    column.name
                ));
            }
            ColumnOption::OnUpdate(_) => {
                warnings.push(format!(
                    "dropped MySQL ON UPDATE clause from column `{}`; PostgreSQL requires a trigger for equivalent behavior",
                    column.name
                ));
            }
            ColumnOption::CharacterSet(_) | ColumnOption::Collation(_) => {
                warnings.push(format!(
                    "dropped MySQL character set/collation column option from `{}`",
                    column.name
                ));
            }
            ColumnOption::Comment(_) => {
                warnings.push(format!(
                    "dropped MySQL column comment from `{}`",
                    column.name
                ));
            }
            ColumnOption::Invisible => {
                warnings.push(format!(
                    "dropped MySQL INVISIBLE column attribute from `{}`",
                    column.name
                ));
            }
            other => rendered_options.push(other.to_string()),
        }
    }

    if auto_increment {
        rendered_options.push("GENERATED BY DEFAULT AS IDENTITY".to_string());
    }

    let mut rendered = format!("{} {}", column.name, translated_type);
    if !rendered_options.is_empty() {
        rendered.push(' ');
        rendered.push_str(&rendered_options.join(" "));
    }

    Ok((rendered, extra_constraints))
}

fn is_auto_increment(tokens: &[sqlparser::tokenizer::Token]) -> bool {
    tokens.len() == 1 && tokens[0].to_string().eq_ignore_ascii_case("AUTO_INCREMENT")
}

fn translate_data_type(
    column_name: &str,
    data_type: &DataType,
    extra_constraints: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> String {
    match data_type {
        DataType::TinyInt(_) => "SMALLINT".to_string(),
        DataType::Int2(_) | DataType::SmallInt(_) => "SMALLINT".to_string(),
        DataType::MediumInt(_) => "INTEGER".to_string(),
        DataType::Int(_) | DataType::Int4(_) | DataType::Integer(_) => "INTEGER".to_string(),
        DataType::BigInt(_) | DataType::Int8(_) => "BIGINT".to_string(),
        DataType::BigIntUnsigned(_) | DataType::Int8Unsigned(_) => {
            warnings.push(format!(
                "mapped `{column_name}` from BIGINT UNSIGNED to BIGINT; PostgreSQL cannot represent the full unsigned 64-bit range in a native integer identity column"
            ));
            "BIGINT".to_string()
        }
        DataType::IntegerUnsigned(_) | DataType::IntUnsigned(_) | DataType::Int4Unsigned(_) => {
            warnings.push(format!(
                "mapped `{column_name}` from INT UNSIGNED to BIGINT to preserve the MySQL value range"
            ));
            push_unsigned_check(extra_constraints, column_name, "4294967295");
            "BIGINT".to_string()
        }
        DataType::SmallIntUnsigned(_) | DataType::Int2Unsigned(_) => {
            warnings.push(format!(
                "mapped `{column_name}` from SMALLINT UNSIGNED to INTEGER to preserve the MySQL value range"
            ));
            push_unsigned_check(extra_constraints, column_name, "65535");
            "INTEGER".to_string()
        }
        DataType::TinyIntUnsigned(_) => {
            warnings.push(format!(
                "mapped `{column_name}` from TINYINT UNSIGNED to SMALLINT to preserve the MySQL value range"
            ));
            push_unsigned_check(extra_constraints, column_name, "255");
            "SMALLINT".to_string()
        }
        DataType::MediumIntUnsigned(_) => {
            warnings.push(format!(
                "mapped `{column_name}` from MEDIUMINT UNSIGNED to INTEGER to preserve the MySQL value range"
            ));
            push_unsigned_check(extra_constraints, column_name, "16777215");
            "INTEGER".to_string()
        }
        DataType::DecimalUnsigned(info) | DataType::DecUnsigned(info) => {
            warnings.push(format!(
                "mapped `{column_name}` from unsigned DECIMAL to NUMERIC and added a non-negative CHECK constraint"
            ));
            push_non_negative_check(extra_constraints, column_name);
            format!("NUMERIC{}", render_exact_number_info(info))
        }
        DataType::Decimal(info) | DataType::Dec(info) | DataType::Numeric(info) => {
            format!("NUMERIC{}", render_exact_number_info(info))
        }
        DataType::FloatUnsigned(info) => {
            warnings.push(format!(
                "mapped `{column_name}` from unsigned FLOAT to REAL and added a non-negative CHECK constraint"
            ));
            push_non_negative_check(extra_constraints, column_name);
            render_float_type(info, true)
        }
        DataType::DoubleUnsigned(info) => {
            warnings.push(format!(
                "mapped `{column_name}` from unsigned DOUBLE to DOUBLE PRECISION and added a non-negative CHECK constraint"
            ));
            push_non_negative_check(extra_constraints, column_name);
            render_double_type(info, true)
        }
        DataType::DoublePrecisionUnsigned | DataType::RealUnsigned => {
            warnings.push(format!(
                "mapped `{column_name}` from unsigned floating-point to DOUBLE PRECISION and added a non-negative CHECK constraint"
            ));
            push_non_negative_check(extra_constraints, column_name);
            "DOUBLE PRECISION".to_string()
        }
        DataType::Float(info) => render_float_type(info, false),
        DataType::Real | DataType::Float4 | DataType::Float32 => "REAL".to_string(),
        DataType::Double(info) => render_double_type(info, false),
        DataType::DoublePrecision | DataType::Float8 | DataType::Float64 => "DOUBLE PRECISION".to_string(),
        DataType::Bool => "BOOLEAN".to_string(),
        DataType::Enum(values, _) => {
            warnings.push(format!(
                "rewrote MySQL ENUM column `{column_name}` to TEXT with a CHECK constraint"
            ));
            push_enum_check(extra_constraints, column_name, values);
            "TEXT".to_string()
        }
        DataType::Set(_) => {
            warnings.push(format!(
                "mapped MySQL SET column `{column_name}` to TEXT; membership semantics are not preserved"
            ));
            "TEXT".to_string()
        }
        DataType::JSON => "JSONB".to_string(),
        DataType::TinyText | DataType::MediumText | DataType::LongText | DataType::Text | DataType::String(_) => "TEXT".to_string(),
        DataType::Binary(_) | DataType::Varbinary(_) | DataType::Blob(_) | DataType::TinyBlob | DataType::MediumBlob | DataType::LongBlob | DataType::Bytes(_) => "BYTEA".to_string(),
        DataType::Datetime(precision) => render_timestamp_type(*precision, false),
        DataType::Timestamp(precision, TimezoneInfo::None) => render_timestamp_type(*precision, false),
        DataType::Timestamp(precision, _) => render_timestamp_type(*precision, true),
        _ => data_type.to_string(),
    }
}

fn render_exact_number_info(info: &ExactNumberInfo) -> String {
    match info {
        ExactNumberInfo::None => String::new(),
        ExactNumberInfo::Precision(p) => format!("({p})"),
        ExactNumberInfo::PrecisionAndScale(p, s) => format!("({p},{s})"),
    }
}

fn render_float_type(info: &ExactNumberInfo, unsigned: bool) -> String {
    match info {
        ExactNumberInfo::None => "REAL".to_string(),
        _ => {
            let _ = unsigned;
            "REAL".to_string()
        }
    }
}

fn render_double_type(info: &ExactNumberInfo, unsigned: bool) -> String {
    match info {
        ExactNumberInfo::None => "DOUBLE PRECISION".to_string(),
        _ => {
            let _ = unsigned;
            "DOUBLE PRECISION".to_string()
        }
    }
}

fn render_timestamp_type(precision: Option<u64>, with_time_zone: bool) -> String {
    let base = if with_time_zone {
        "TIMESTAMP WITH TIME ZONE"
    } else {
        "TIMESTAMP"
    };
    match precision {
        Some(precision) => format!("{base}({precision})"),
        None => base.to_string(),
    }
}

fn push_non_negative_check(extra_constraints: &mut Vec<String>, column_name: &str) {
    extra_constraints.push(format!(
        "CHECK ({column_name} >= 0)"
    ));
}

fn push_unsigned_check(extra_constraints: &mut Vec<String>, column_name: &str, max: &str) {
    extra_constraints.push(format!(
        "CHECK ({column_name} >= 0 AND {column_name} <= {max})"
    ));
}

fn push_enum_check(
    extra_constraints: &mut Vec<String>,
    column_name: &str,
    values: &[sqlparser::ast::EnumMember],
) {
    let allowed = values
        .iter()
        .map(|value| match value {
            sqlparser::ast::EnumMember::Name(name) => format!("'{}'", name.replace('\'', "''")),
            sqlparser::ast::EnumMember::NamedValue(name, _) => format!("'{}'", name.replace('\'', "''")),
        })
        .collect::<Vec<_>>()
        .join(", ");

    extra_constraints.push(format!("CHECK ({column_name} IN ({allowed}))"));
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

    #[test]
    fn rewrites_mysql_create_table_for_postgres() {
        let sql = r#"
            CREATE TABLE IF NOT EXISTS order_details (
                order_id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
                product_id INT UNSIGNED NOT NULL,
                customer_id INT UNSIGNED NOT NULL,
                quantity SMALLINT NOT NULL DEFAULT 1,
                price DECIMAL(10, 2) NOT NULL,
                discount DECIMAL(3, 2) DEFAULT 0.00,
                status ENUM('pending', 'shipped', 'delivered', 'cancelled') DEFAULT 'pending',
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
                PRIMARY KEY (order_id),
                UNIQUE KEY unique_order_prod (order_id, product_id),
                CONSTRAINT fk_product FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE CASCADE,
                CONSTRAINT chk_quantity CHECK (quantity > 0)
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci
        "#;

        let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

        assert!(result.translated_sql.contains("CREATE TABLE IF NOT EXISTS order_details"));
        assert!(result.translated_sql.contains("order_id BIGINT NOT NULL GENERATED BY DEFAULT AS IDENTITY"));
        assert!(result.translated_sql.contains("product_id BIGINT NOT NULL"));
        assert!(result.translated_sql.contains("customer_id BIGINT NOT NULL"));
        assert!(result.translated_sql.contains("status TEXT DEFAULT 'pending'"));
        assert!(result.translated_sql.contains("CHECK (status IN ('pending', 'shipped', 'delivered', 'cancelled'))"));
        assert!(result.translated_sql.contains("CHECK (product_id >= 0 AND product_id <= 4294967295)"));
        assert!(result.translated_sql.contains("CHECK (customer_id >= 0 AND customer_id <= 4294967295)"));
        assert!(!result.translated_sql.contains("AUTO_INCREMENT"));
        assert!(!result.translated_sql.contains("UNSIGNED"));
        assert!(!result.translated_sql.contains("ON UPDATE CURRENT_TIMESTAMP"));
        assert!(!result.translated_sql.contains("ENGINE = InnoDB"));
    }
}
