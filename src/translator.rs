use regex::{Captures, Regex};
use serde::Serialize;
use sqlparser::ast::{
    AlterTable, AlterTableOperation, ColumnDef, ColumnOption, CreateTable, DataType,
    ExactNumberInfo, Expr, ObjectName, OnConflict, OnConflictAction, OnInsert, ShowCharset,
    ShowCreateObject, ShowStatementFilter, ShowStatementFilterPosition, ShowStatementInParentType,
    ShowStatementOptions, Statement, TableConstraint, TimezoneInfo,
};
use sqlparser::ast::table_constraints::{IndexConstraint, UniqueConstraint};

use crate::{config::TranslatorConfig, error::MiddlewareError, parser::parse_mysql_sql};

#[derive(Debug, Clone, Serialize)]
pub struct TranslationResult {
    pub original_sql: String,
    pub canonical_mysql_sql: String,
    pub translated_sql: String,
    pub warnings: Vec<String>,
}

pub fn translate_sql(sql: &str, cfg: &TranslatorConfig) -> Result<TranslationResult, MiddlewareError> {
    let direct_translation = translate_unparsed_sql(sql)?;
    let statements = match direct_translation.as_ref() {
        Some(_) => Vec::new(),
        None => parse_mysql_sql(sql)?,
    };
    if statements.is_empty() {
        if let Some((canonical_mysql_sql, mut translated, mut warnings)) = direct_translation {
            if cfg.normalize_mysql_backticks {
                translated = replace_backticks(&translated);
            }
            translated = rewrite_mysql_system_variables(&translated, &mut warnings);
            if cfg.rewrite_limit_comma {
                translated = rewrite_limit_offset_count(&translated, &mut warnings);
            }
            if cfg.normalize_boolean_literals {
                translated = rewrite_boolean_literals(&translated);
            }
            if cfg.rewrite_mysql_functions {
                translated = rewrite_mysql_functions(&translated, &mut warnings);
            }
            translated = strip_mysql_select_modifiers(&translated, &mut warnings);
            if cfg.rewrite_json_operators {
                translated = rewrite_json_extract(&translated, &mut warnings);
            }
            if cfg.strip_mysql_table_options {
                translated = strip_mysql_table_options(&translated, &mut warnings);
            }
            translated = quote_reserved_relation_references(&translated, &mut warnings);

            return Ok(TranslationResult {
                original_sql: sql.to_string(),
                canonical_mysql_sql,
                translated_sql: translated,
                warnings,
            });
        }
        return Err(MiddlewareError::Translation("no statements were parsed".to_string()));
    }

    let canonical_mysql_sql = statements
        .iter()
        .map(|stmt| stmt.to_string())
        .collect::<Vec<_>>()
        .join("; ");

    let mut warnings = Vec::new();
    let mut translated = translate_statements(&statements, &mut warnings)?;
    let requires_unsupported_rejection = statements.iter().all(statement_requires_unsupported_rejection);

    if cfg.normalize_mysql_backticks {
        translated = replace_backticks(&translated);
    }
    translated = rewrite_mysql_system_variables(&translated, &mut warnings);
    if cfg.rewrite_limit_comma {
        translated = rewrite_limit_offset_count(&translated, &mut warnings);
    }
    if cfg.normalize_boolean_literals {
        translated = rewrite_boolean_literals(&translated);
    }
    if cfg.rewrite_mysql_functions {
        translated = rewrite_mysql_functions(&translated, &mut warnings);
    }
    translated = strip_mysql_select_modifiers(&translated, &mut warnings);
    if cfg.rewrite_json_operators {
        translated = rewrite_json_extract(&translated, &mut warnings);
    }
    if cfg.strip_mysql_table_options {
        translated = strip_mysql_table_options(&translated, &mut warnings);
    }
    translated = quote_reserved_relation_references(&translated, &mut warnings);

    if requires_unsupported_rejection {
        reject_unsupported(&translated)?;
    }

    Ok(TranslationResult {
        original_sql: sql.to_string(),
        canonical_mysql_sql,
        translated_sql: translated,
        warnings,
    })
}

fn translate_unparsed_sql(
    sql: &str,
) -> Result<Option<(String, String, Vec<String>)>, MiddlewareError> {
    if let Some((translated_sql, warnings)) = translate_insert_on_duplicate_key_direct(sql)? {
        return Ok(Some((sql.trim().to_string(), translated_sql, warnings)));
    }

    if let Some((translated_sql, warnings)) = translate_show_index_direct(sql)? {
        return Ok(Some((sql.trim().to_string(), translated_sql, warnings)));
    }

    Ok(None)
}

fn statement_requires_unsupported_rejection(stmt: &Statement) -> bool {
    !matches!(
        stmt,
        Statement::ShowTables { .. }
            | Statement::ShowDatabases { .. }
            | Statement::ShowSchemas { .. }
            | Statement::ShowViews { .. }
            | Statement::ShowFunctions { .. }
            | Statement::ShowCollation { .. }
            | Statement::ShowCharset { .. }
            | Statement::ShowVariables { .. }
            | Statement::ShowStatus { .. }
            | Statement::ShowColumns { .. }
            | Statement::ShowCreate { .. }
            | Statement::ExplainTable { .. }
    )
}

fn replace_backticks(sql: &str) -> String {
    sql.replace('`', "\"")
}

fn quote_reserved_relation_references(sql: &str, warnings: &mut Vec<String>) -> String {
    let reserved_relations = ["user"];
    let patterns = [
        Regex::new(r#"(?i)\b(FROM|JOIN|UPDATE|INTO|TABLE|DELETE\s+FROM|USING|TRUNCATE\s+TABLE|TRUNCATE|LOCK\s+TABLE)\s+([A-Za-z_][A-Za-z0-9_]*)\b"#)
            .expect("valid relation-reference regex"),
        Regex::new(r#"(?i)\b(INSERT\s+INTO)\s+([A-Za-z_][A-Za-z0-9_]*)\b"#)
            .expect("valid insert-into relation regex"),
    ];

    let mut changed = false;
    let mut translated = sql.to_string();

    for pattern in patterns {
        translated = pattern
            .replace_all(&translated, |caps: &Captures<'_>| {
                let relation = &caps[2];
                if reserved_relations
                    .iter()
                    .any(|name| relation.eq_ignore_ascii_case(name))
                {
                    changed = true;
                    format!("{} {}", &caps[1], quote_ident(relation))
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();
    }

    if changed {
        warnings.push(
            "quoted reserved relation names in translated SQL to preserve PostgreSQL semantics"
                .to_string(),
        );
    }

    translated
}

fn rewrite_mysql_system_variables(sql: &str, warnings: &mut Vec<String>) -> String {
    let variable_re = Regex::new(
        r"(?i)@@(?:(?:SESSION|GLOBAL)\.)?(sql_mode|version_comment|version|collation_connection|transaction_isolation|tx_isolation)\b",
    )
    .expect("valid MySQL system variable regex");
    let version_fn_re = Regex::new(r"(?i)\bVERSION\s*\(\s*\)").expect("valid VERSION() regex");

    if !variable_re.is_match(sql) && !version_fn_re.is_match(sql) {
        return sql.to_string();
    }

    warnings.push("rewrote MySQL system variables to compatibility literals".to_string());
    let out = variable_re.replace_all(sql, |caps: &Captures<'_>| {
        match caps[1].to_ascii_lowercase().as_str() {
            "sql_mode" => "'NO_AUTO_VALUE_ON_ZERO'".to_string(),
            "version" => "'11.8.7-MariaDB-ubu2404'".to_string(),
            "version_comment" => "'MariaDB Server'".to_string(),
            "collation_connection" => "'utf8mb4_general_ci'".to_string(),
            "transaction_isolation" | "tx_isolation" => "'REPEATABLE-READ'".to_string(),
            _ => caps[0].to_string(),
        }
    })
    .into_owned();

    version_fn_re
        .replace_all(&out, "'11.8.7-MariaDB-ubu2404'")
        .into_owned()
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
        Statement::Insert(insert) => translate_insert(insert, warnings),
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
        Statement::ShowDatabases { terse, history, show_options } => {
            translate_show_databases(*terse, *history, show_options, warnings)
        }
        Statement::ShowSchemas { terse, history, show_options } => {
            translate_show_schemas(*terse, *history, show_options, warnings)
        }
        Statement::ShowViews {
            terse,
            materialized,
            show_options,
        } => translate_show_views(*terse, *materialized, show_options, warnings),
        Statement::ShowFunctions { filter } => translate_show_functions(filter.as_ref(), warnings),
        Statement::ShowCollation { filter } => translate_show_collation(filter.as_ref(), warnings),
        Statement::ShowCharset(show_charset) => translate_show_charset(show_charset, warnings),
        Statement::ShowVariables {
            filter,
            global,
            session,
        } => translate_show_variables(filter.as_ref(), *global, *session, warnings),
        Statement::ShowStatus {
            filter,
            global,
            session,
        } => translate_show_status(filter.as_ref(), *global, *session, warnings),
        Statement::ShowColumns { extended, full, show_options } => {
            translate_show_columns(*extended, *full, show_options, warnings)
        }
        Statement::ShowCreate { obj_type, obj_name } => translate_show_create(obj_type, obj_name, warnings),
        Statement::ExplainTable { table_name, .. } => translate_describe_table(table_name, warnings),
        Statement::AlterTable(alter) => translate_alter_table(alter, warnings),
        _ => Ok(stmt.to_string()),
    }
}

fn translate_insert(
    insert: &sqlparser::ast::Insert,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    let mut insert = insert.clone();

    if insert.ignore {
        if insert.on.is_some() {
            return Err(MiddlewareError::Translation(
                "MySQL INSERT IGNORE with an additional conflict clause is not supported".to_string(),
            ));
        }
        insert.ignore = false;
        insert.on = Some(OnInsert::OnConflict(OnConflict {
            conflict_target: None,
            action: OnConflictAction::DoNothing,
        }));
        warnings.push(
            "rewrote MySQL INSERT IGNORE to PostgreSQL INSERT ... ON CONFLICT DO NOTHING"
                .to_string(),
        );
    }

    Ok(insert.to_string())
}

fn translate_describe_table(
    table_name: &ObjectName,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    translate_describe_like_query(table_name, None, false, warnings)
}

fn translate_describe_like_query(
    table_name: &ObjectName,
    filter: Option<&ShowStatementFilter>,
    full: bool,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    let (schema_name, relation_name) = split_object_name(table_name)?;
    let schema_expr = schema_name
        .as_deref()
        .map(sql_string_literal)
        .unwrap_or_else(|| "current_schema()".to_string());
    let relation_expr = sql_string_literal(&relation_name);

    let mut sql = format!(
        "SELECT c.column_name AS \"Field\", \
                pg_catalog.format_type(a.atttypid, a.atttypmod) AS \"Type\", \
                CASE WHEN c.is_nullable = 'YES' THEN 'YES' ELSE 'NO' END AS \"Null\", \
                CASE \
                    WHEN EXISTS ( \
                        SELECT 1 \
                        FROM pg_constraint pc \
                        JOIN pg_attribute pa ON pa.attrelid = pc.conrelid AND pa.attnum = ANY(pc.conkey) \
                        WHERE pc.conrelid = cls.oid AND pa.attname = c.column_name AND pc.contype = 'p' \
                    ) THEN 'PRI' \
                    WHEN EXISTS ( \
                        SELECT 1 \
                        FROM pg_constraint pc \
                        JOIN pg_attribute pa ON pa.attrelid = pc.conrelid AND pa.attnum = ANY(pc.conkey) \
                        WHERE pc.conrelid = cls.oid AND pa.attname = c.column_name AND pc.contype = 'u' AND cardinality(pc.conkey) = 1 \
                    ) THEN 'UNI' \
                    WHEN EXISTS ( \
                        SELECT 1 \
                        FROM pg_index pi \
                        JOIN pg_attribute pa ON pa.attrelid = pi.indrelid AND pa.attnum = ANY(pi.indkey) \
                        WHERE pi.indrelid = cls.oid AND pa.attname = c.column_name \
                    ) OR EXISTS ( \
                        SELECT 1 \
                        FROM pg_constraint pc \
                        JOIN pg_attribute pa ON pa.attrelid = pc.conrelid AND pa.attnum = ANY(pc.conkey) \
                        WHERE pc.conrelid = cls.oid AND pa.attname = c.column_name AND pc.contype = 'f' \
                    ) THEN 'MUL' \
                    ELSE '' \
                END AS \"Key\", \
                c.column_default AS \"Default\", \
                CASE \
                    WHEN c.is_identity = 'YES' THEN 'auto_increment' \
                    WHEN pg_get_expr(ad.adbin, ad.adrelid) LIKE 'nextval(%' THEN 'auto_increment' \
                    ELSE '' \
                END AS \"Extra\" \
         FROM information_schema.columns c \
         JOIN pg_namespace ns ON ns.nspname = c.table_schema \
         JOIN pg_class cls ON cls.relname = c.table_name AND cls.relnamespace = ns.oid \
         JOIN pg_attribute a ON a.attrelid = cls.oid AND a.attname = c.column_name \
         LEFT JOIN pg_attrdef ad ON ad.adrelid = cls.oid AND ad.adnum = a.attnum \
         WHERE c.table_schema = {schema_expr} AND c.table_name = {relation_expr}"
    );

    if let Some(filter_sql) = translate_named_filter(filter, "c.column_name")? {
        sql.push_str(" AND ");
        sql.push_str(&filter_sql);
    }

    if full {
        sql = sql.replacen(
            " AS \"Extra\" ",
            " AS \"Extra\", NULL::text AS \"Privileges\", NULL::text AS \"Comment\" ",
            1,
        );
    }

    sql.push_str(" ORDER BY c.ordinal_position");
    warnings.push("rewrote MySQL DESC/DESCRIBE/SHOW COLUMNS to PostgreSQL catalog query".to_string());
    Ok(sql)
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
    translate_named_filter(extract_show_filter(show_options), "table_name")
}

fn translate_show_databases(
    terse: bool,
    history: bool,
    show_options: &ShowStatementOptions,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if terse || history {
        return Err(MiddlewareError::Translation(
            "SHOW DATABASES TERSE/HISTORY options are not supported yet".to_string(),
        ));
    }

    if show_options.show_in.is_some() || show_options.starts_with.is_some() || show_options.limit.is_some() || show_options.limit_from.is_some() {
        return Err(MiddlewareError::Translation(
            "SHOW DATABASES scope/STARTS WITH/LIMIT options are not supported yet".to_string(),
        ));
    }

    let mut sql = "SELECT datname AS \"Database\" FROM pg_database WHERE datistemplate = false ORDER BY datname".to_string();
    if let Some(filter_sql) = translate_named_filter(extract_show_filter(show_options), "datname")? {
        sql = format!(
            "SELECT datname AS \"Database\" FROM pg_database WHERE datistemplate = false AND {filter_sql} ORDER BY datname"
        );
    }
    warnings.push("rewrote MySQL SHOW DATABASES to pg_database query".to_string());
    Ok(sql)
}

fn translate_show_schemas(
    terse: bool,
    history: bool,
    show_options: &ShowStatementOptions,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if terse || history {
        return Err(MiddlewareError::Translation(
            "SHOW SCHEMAS TERSE/HISTORY options are not supported yet".to_string(),
        ));
    }

    if show_options.show_in.is_some() || show_options.starts_with.is_some() || show_options.limit.is_some() || show_options.limit_from.is_some() {
        return Err(MiddlewareError::Translation(
            "SHOW SCHEMAS scope/STARTS WITH/LIMIT options are not supported yet".to_string(),
        ));
    }

    let mut sql =
        "SELECT schema_name AS \"Database\" FROM information_schema.schemata ORDER BY schema_name".to_string();
    if let Some(filter_sql) = translate_named_filter(extract_show_filter(show_options), "schema_name")? {
        sql = format!(
            "SELECT schema_name AS \"Database\" FROM information_schema.schemata WHERE {filter_sql} ORDER BY schema_name"
        );
    }
    warnings.push("rewrote MySQL SHOW SCHEMAS to information_schema.schemata query".to_string());
    Ok(sql)
}

fn translate_show_views(
    terse: bool,
    materialized: bool,
    show_options: &ShowStatementOptions,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if terse || materialized {
        return Err(MiddlewareError::Translation(
            "SHOW VIEWS TERSE/MATERIALIZED options are not supported yet".to_string(),
        ));
    }
    if show_options.starts_with.is_some() || show_options.limit.is_some() || show_options.limit_from.is_some() {
        return Err(MiddlewareError::Translation(
            "SHOW VIEWS STARTS WITH/LIMIT options are not supported yet".to_string(),
        ));
    }

    let (schema_expr, column_alias) = resolve_show_tables_schema(show_options)?;
    let mut sql = format!(
        "SELECT table_name AS \"{column_alias}\" FROM information_schema.views WHERE table_schema = {schema_expr}"
    );
    if let Some(filter_sql) = translate_show_tables_filter(show_options)? {
        sql.push_str(" AND ");
        sql.push_str(&filter_sql);
    }
    sql.push_str(" ORDER BY table_name");
    warnings.push("rewrote MySQL SHOW VIEWS to information_schema.views query".to_string());
    Ok(sql)
}

fn translate_show_functions(
    filter: Option<&ShowStatementFilter>,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    let base_sql = "SELECT routine_name AS \"Function\", routine_type AS \"Type\" \
                    FROM information_schema.routines \
                    WHERE routine_schema = current_schema() AND routine_type = 'FUNCTION'";
    let sql = apply_wrapped_show_filter(base_sql, filter, "Function")?;
    warnings.push("rewrote MySQL SHOW FUNCTIONS to information_schema.routines query".to_string());
    Ok(format!("{sql} ORDER BY \"Function\""))
}

fn translate_show_collation(
    filter: Option<&ShowStatementFilter>,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    let base_sql = "SELECT * FROM (VALUES \
                        ('utf8mb4_general_ci', 'utf8mb4', 45, 'Yes', 'Yes', 1), \
                        ('utf8mb4_unicode_ci', 'utf8mb4', 224, '', 'Yes', 8), \
                        ('utf8_general_ci', 'utf8', 33, '', 'Yes', 1), \
                        ('latin1_swedish_ci', 'latin1', 8, '', 'Yes', 1) \
                    ) AS collation_rows(\"Collation\", \"Charset\", \"Id\", \"Default\", \"Compiled\", \"Sortlen\")";
    let sql = apply_wrapped_show_filter(base_sql, filter, "Collation")?;
    warnings.push("rewrote MySQL SHOW COLLATION to compatibility rows".to_string());
    Ok(format!("{sql} ORDER BY \"Collation\""))
}

fn translate_show_charset(
    show_charset: &ShowCharset,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    let base_sql = "SELECT * FROM (VALUES \
                        ('utf8mb4', 'UTF-8 Unicode', 'utf8mb4_unicode_ci', 4), \
                        ('utf8', 'UTF-8 Unicode', 'utf8_general_ci', 3), \
                        ('latin1', 'cp1252 West European', 'latin1_swedish_ci', 1) \
                    ) AS charset_rows(\"Charset\", \"Description\", \"Default collation\", \"Maxlen\")";
    let sql = apply_wrapped_show_filter(base_sql, show_charset.filter.as_ref(), "Charset")?;
    warnings.push("rewrote MySQL SHOW CHARSET/CHARACTER SET to compatibility rows".to_string());
    Ok(format!("{sql} ORDER BY \"Charset\""))
}

fn translate_show_variables(
    filter: Option<&ShowStatementFilter>,
    global: bool,
    session: bool,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if global && session {
        return Err(MiddlewareError::Translation(
            "SHOW GLOBAL SESSION VARIABLES is not supported".to_string(),
        ));
    }

    let base_sql = "WITH vars AS ( \
                        SELECT * FROM (VALUES \
                            ('autocommit', 'ON'), \
                            ('character_set_client', 'utf8mb4'), \
                            ('character_set_connection', 'utf8mb4'), \
                            ('character_set_database', 'utf8mb4'), \
                            ('character_set_results', 'utf8mb4'), \
                            ('collation_connection', 'utf8mb4_unicode_ci'), \
                            ('collation_database', 'utf8mb4_unicode_ci'), \
                            ('lower_case_table_names', '0'), \
                            ('max_allowed_packet', '67108864'), \
                            ('sql_mode', 'NO_AUTO_VALUE_ON_ZERO'), \
                            ('system_time_zone', current_setting('TimeZone')), \
                            ('time_zone', current_setting('TimeZone')), \
                            ('transaction_isolation', current_setting('transaction_isolation')), \
                            ('tx_isolation', current_setting('transaction_isolation')), \
                            ('version', '11.8.7-MariaDB-ubu2404'), \
                            ('version_comment', 'MariaDB Server') \
                        ) AS v(\"Variable_name\", \"Value\") \
                    ) \
                    SELECT \"Variable_name\", \"Value\" FROM vars";
    let sql = apply_wrapped_show_filter(base_sql, filter, "Variable_name")?;
    if global || session {
        warnings.push("SHOW GLOBAL/SESSION VARIABLES is mapped to the current PostgreSQL session view".to_string());
    }
    warnings.push("rewrote MySQL SHOW VARIABLES to compatibility rows".to_string());
    Ok(format!("{sql} ORDER BY \"Variable_name\""))
}

fn translate_show_status(
    filter: Option<&ShowStatementFilter>,
    global: bool,
    session: bool,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if global && session {
        return Err(MiddlewareError::Translation(
            "SHOW GLOBAL SESSION STATUS is not supported".to_string(),
        ));
    }

    let base_sql = "WITH status_rows AS ( \
                        SELECT 'Connections'::text AS \"Variable_name\", COALESCE((SELECT sum(numbackends)::bigint::text FROM pg_stat_database), '0') AS \"Value\" \
                        UNION ALL \
                        SELECT 'Threads_connected', COALESCE((SELECT count(*)::text FROM pg_stat_activity WHERE datname = current_database()), '0') \
                        UNION ALL \
                        SELECT 'Threads_running', COALESCE((SELECT count(*)::text FROM pg_stat_activity WHERE datname = current_database() AND state = 'active'), '0') \
                        UNION ALL \
                        SELECT 'Uptime', extract(epoch FROM CURRENT_TIMESTAMP - pg_postmaster_start_time())::bigint::text \
                    ) \
                    SELECT \"Variable_name\", \"Value\" FROM status_rows";
    let sql = apply_wrapped_show_filter(base_sql, filter, "Variable_name")?;
    if global || session {
        warnings.push("SHOW GLOBAL/SESSION STATUS is mapped to PostgreSQL activity statistics".to_string());
    }
    warnings.push("rewrote MySQL SHOW STATUS to PostgreSQL activity query".to_string());
    Ok(format!("{sql} ORDER BY \"Variable_name\""))
}

fn translate_show_columns(
    extended: bool,
    full: bool,
    show_options: &ShowStatementOptions,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if extended {
        return Err(MiddlewareError::Translation(
            "SHOW EXTENDED COLUMNS is not supported yet".to_string(),
        ));
    }
    if show_options.starts_with.is_some() || show_options.limit.is_some() || show_options.limit_from.is_some() {
        return Err(MiddlewareError::Translation(
            "SHOW COLUMNS STARTS WITH/LIMIT options are not supported yet".to_string(),
        ));
    }

    let table_name = resolve_show_columns_target(show_options)?;
    translate_describe_like_query(&table_name, extract_show_filter(show_options), full, warnings)
}

fn resolve_show_columns_target(show_options: &ShowStatementOptions) -> Result<ObjectName, MiddlewareError> {
    let show_in = show_options.show_in.as_ref().ok_or_else(|| {
        MiddlewareError::Translation("SHOW COLUMNS requires a target table".to_string())
    })?;

    if let Some(parent_type) = &show_in.parent_type {
        if !matches!(parent_type, ShowStatementInParentType::Table) {
            return Err(MiddlewareError::Translation(format!(
                "SHOW COLUMNS {} is not supported yet",
                parent_type
            )));
        }
    }

    show_in.parent_name.clone().ok_or_else(|| {
        MiddlewareError::Translation("SHOW COLUMNS requires a concrete table name".to_string())
    })
}

fn translate_show_create(
    obj_type: &ShowCreateObject,
    obj_name: &ObjectName,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    match obj_type {
        ShowCreateObject::Table => translate_show_create_table(obj_name, warnings),
        ShowCreateObject::View => translate_show_create_view(obj_name, warnings),
        _ => Err(MiddlewareError::Translation(format!(
            "SHOW CREATE {} is not supported yet",
            obj_type
        ))),
    }
}

fn translate_show_create_table(
    obj_name: &ObjectName,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    let (schema_name, relation_name) = split_object_name(obj_name)?;
    let schema_expr = sql_string_literal(schema_name.as_deref().unwrap_or("public"));
    let relation_expr = sql_string_literal(&relation_name);
    let relation_label = sql_string_literal(&relation_name);

    warnings.push("rewrote MySQL SHOW CREATE TABLE to PostgreSQL catalog query".to_string());

    Ok(format!(
        "WITH target AS ( \
             SELECT c.oid, n.nspname AS schema_name, c.relname AS table_name \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {schema_expr} AND c.relname = {relation_expr} AND c.relkind IN ('r','p') \
         ), pieces AS ( \
             SELECT a.attnum AS ord, \
                    format('  %I %s%s%s%s', \
                        a.attname, \
                        pg_catalog.format_type(a.atttypid, a.atttypmod), \
                        CASE a.attidentity WHEN 'a' THEN ' GENERATED ALWAYS AS IDENTITY' WHEN 'd' THEN ' GENERATED BY DEFAULT AS IDENTITY' ELSE '' END, \
                        CASE WHEN a.attnotnull THEN ' NOT NULL' ELSE '' END, \
                        CASE WHEN ad.adbin IS NOT NULL AND a.attidentity = '' THEN ' DEFAULT ' || pg_get_expr(ad.adbin, ad.adrelid) ELSE '' END \
                    ) AS line \
             FROM target t \
             JOIN pg_attribute a ON a.attrelid = t.oid \
             LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
             WHERE a.attnum > 0 AND NOT a.attisdropped \
             UNION ALL \
             SELECT 100000 + row_number() OVER (ORDER BY con.oid), \
                    CASE WHEN con.contype = 'p' THEN '  ' || pg_get_constraintdef(con.oid) ELSE format('  CONSTRAINT %I %s', con.conname, pg_get_constraintdef(con.oid)) END \
             FROM target t \
             JOIN pg_constraint con ON con.conrelid = t.oid \
         ) \
         SELECT {relation_label} AS \"Table\", \
                format('CREATE TABLE %I (\\n%s\\n)', (SELECT table_name FROM target), string_agg(line, E',\\n' ORDER BY ord)) AS \"Create Table\" \
         FROM pieces"
    ))
}

fn translate_show_create_view(
    obj_name: &ObjectName,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    let (schema_name, relation_name) = split_object_name(obj_name)?;
    let schema_expr = sql_string_literal(schema_name.as_deref().unwrap_or("public"));
    let relation_expr = sql_string_literal(&relation_name);
    let relation_label = sql_string_literal(&relation_name);

    warnings.push("rewrote MySQL SHOW CREATE VIEW to PostgreSQL catalog query".to_string());
    Ok(format!(
        "SELECT {relation_label} AS \"View\", format('CREATE VIEW %I AS %s', c.relname, pg_get_viewdef(c.oid, true)) AS \"Create View\" \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = {schema_expr} AND c.relname = {relation_expr} AND c.relkind = 'v'"
    ))
}

fn split_object_name(table_name: &ObjectName) -> Result<(Option<String>, String), MiddlewareError> {
    let parts = table_name
        .0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|ident| ident.value.clone())
                .ok_or_else(|| MiddlewareError::Translation("function-style object names are not supported here".to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    match parts.as_slice() {
        [table] => Ok((None, table.clone())),
        [schema, table] => Ok((Some(schema.clone()), table.clone())),
        _ => Err(MiddlewareError::Translation(format!(
            "unsupported object name `{table_name}`"
        ))),
    }
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn extract_show_filter(show_options: &ShowStatementOptions) -> Option<&ShowStatementFilter> {
    match &show_options.filter_position {
        Some(ShowStatementFilterPosition::Infix(filter)) | Some(ShowStatementFilterPosition::Suffix(filter)) => Some(filter),
        None => None,
    }
}

fn apply_wrapped_show_filter(
    base_query: &str,
    filter: Option<&ShowStatementFilter>,
    like_field: &str,
) -> Result<String, MiddlewareError> {
    let Some(filter) = filter else {
        return Ok(base_query.to_string());
    };

    let predicate = match filter {
        ShowStatementFilter::Like(pattern)
        | ShowStatementFilter::ILike(pattern)
        | ShowStatementFilter::NoKeyword(pattern) => format!(
            "\"{like_field}\" LIKE '{}'",
            pattern.replace('\'', "''")
        ),
        ShowStatementFilter::Where(expr) => expr.to_string(),
    };

    Ok(format!("SELECT * FROM ({base_query}) AS show_meta WHERE {predicate}"))
}

fn translate_named_filter(
    filter: Option<&ShowStatementFilter>,
    field_name: &str,
) -> Result<Option<String>, MiddlewareError> {
    let Some(filter) = filter else {
        return Ok(None);
    };

    match filter {
        ShowStatementFilter::Like(pattern)
        | ShowStatementFilter::ILike(pattern)
        | ShowStatementFilter::NoKeyword(pattern) => Ok(Some(format!(
            "{field_name} LIKE '{}'",
            pattern.replace('\'', "''")
        ))),
        ShowStatementFilter::Where(_) => Err(MiddlewareError::Translation(
            "SHOW ... WHERE is not supported yet".to_string(),
        )),
    }
}

fn translate_show_index_direct(sql: &str) -> Result<Option<(String, Vec<String>)>, MiddlewareError> {
    let pattern = Regex::new(
        r#"(?is)^\s*SHOW\s+(?:INDEX|INDEXES|KEYS)\s+FROM\s+(?:`([A-Za-z_][A-Za-z0-9_]*)`|([A-Za-z_][A-Za-z0-9_]*))(?:\s+WHERE\s+Key_name\s*=\s*(\?|'.*?'|".*?"))?\s*;?\s*$"#,
    )
    .expect("valid show index regex");
    let Some(caps) = pattern.captures(sql) else {
        return Ok(None);
    };

    let table_name = caps
        .get(1)
        .or_else(|| caps.get(2))
        .map(|m| m.as_str())
        .ok_or_else(|| MiddlewareError::Translation("failed to extract SHOW INDEX table name".to_string()))?;
    let key_name_filter = caps.get(3).map(|m| m.as_str().trim());
    let filter_expr = match key_name_filter {
        Some("?") => "\"Key_name\" = $1".to_string(),
        Some(value) if (value.starts_with('\'') && value.ends_with('\'')) || (value.starts_with('"') && value.ends_with('"')) => {
            format!("\"Key_name\" = {}", sql_string_literal(&value[1..value.len() - 1]))
        }
        Some(value) => {
            return Err(MiddlewareError::Translation(format!(
                "unsupported SHOW INDEX filter value `{value}`"
            )))
        }
        None => "TRUE".to_string(),
    };

    let translated = format!(
        "SELECT * FROM ( \
            SELECT \
                cls.relname AS \"Table\", \
                CASE WHEN idx.indisunique THEN 0 ELSE 1 END AS \"Non_unique\", \
                CASE WHEN idx.indisprimary THEN 'PRIMARY' ELSE ci.relname END AS \"Key_name\", \
                key_columns.ordinality AS \"Seq_in_index\", \
                att.attname AS \"Column_name\", \
                'A' AS \"Collation\", \
                NULL::BIGINT AS \"Cardinality\", \
                NULL::BIGINT AS \"Sub_part\", \
                NULL::TEXT AS \"Packed\", \
                CASE WHEN att.attnotnull THEN '' ELSE 'YES' END AS \"Null\", \
                'BTREE' AS \"Index_type\", \
                '' AS \"Comment\", \
                '' AS \"Index_comment\", \
                'YES' AS \"Visible\", \
                NULL::TEXT AS \"Expression\" \
            FROM pg_class cls \
            JOIN pg_namespace ns ON ns.oid = cls.relnamespace \
            JOIN pg_index idx ON idx.indrelid = cls.oid \
            JOIN pg_class ci ON ci.oid = idx.indexrelid \
            JOIN LATERAL unnest(idx.indkey) WITH ORDINALITY AS key_columns(attnum, ordinality) ON TRUE \
            JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = key_columns.attnum \
            WHERE ns.nspname = current_schema() AND cls.relname = {} \
        ) AS show_index_rows WHERE {filter_expr} ORDER BY \"Key_name\", \"Seq_in_index\"",
        sql_string_literal(table_name),
    );

    Ok(Some((
        translated,
        vec!["translated SHOW INDEX to PostgreSQL catalog query".to_string()],
    )))
}

fn translate_insert_on_duplicate_key_direct(
    sql: &str,
) -> Result<Option<(String, Vec<String>)>, MiddlewareError> {
    let pattern = Regex::new(
        r#"(?is)^\s*INSERT\s+INTO\s+(?:`([A-Za-z_][A-Za-z0-9_]*)`|([A-Za-z_][A-Za-z0-9_]*))\s*\((.+?)\)\s*VALUES\s*\((.+?)\)\s*ON\s+DUPLICATE\s+KEY\s+UPDATE\s+(.+?)\s*;?\s*$"#,
    )
    .expect("valid on duplicate key regex");
    let Some(caps) = pattern.captures(sql) else {
        return Ok(None);
    };

    let table_name = caps
        .get(1)
        .or_else(|| caps.get(2))
        .map(|m| m.as_str())
        .ok_or_else(|| MiddlewareError::Translation("failed to extract INSERT table name".to_string()))?;
    let raw_columns = caps
        .get(3)
        .map(|m| m.as_str())
        .ok_or_else(|| MiddlewareError::Translation("failed to extract INSERT columns".to_string()))?;
    let raw_values = caps
        .get(4)
        .map(|m| m.as_str())
        .ok_or_else(|| MiddlewareError::Translation("failed to extract INSERT values".to_string()))?;
    let raw_updates = caps
        .get(5)
        .map(|m| m.as_str())
        .ok_or_else(|| MiddlewareError::Translation("failed to extract INSERT updates".to_string()))?;

    let columns = split_sql_csv(raw_columns)?
        .into_iter()
        .map(|column| normalize_identifier_token(&column))
        .collect::<Result<Vec<_>, _>>()?;
    let values = split_sql_csv(raw_values)?
        .into_iter()
        .map(|value| normalize_mysql_string_literals(&value))
        .collect::<Vec<_>>();
    if columns.is_empty() || columns.len() != values.len() {
        return Err(MiddlewareError::Translation(
            "INSERT ... ON DUPLICATE KEY UPDATE requires matching column and value counts"
                .to_string(),
        ));
    }

    let update_assignments = split_sql_csv(raw_updates)?
        .into_iter()
        .map(|assignment| parse_update_assignment(&assignment))
        .collect::<Result<Vec<_>, _>>()?;
    if update_assignments.is_empty() {
        return Err(MiddlewareError::Translation(
            "INSERT ... ON DUPLICATE KEY UPDATE requires at least one assignment".to_string(),
        ));
    }

    let conflict_target = infer_on_conflict_target(table_name, &columns, &update_assignments)?;
    let rendered_columns = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let rendered_values = values.join(", ");
    let rendered_updates = update_assignments
        .iter()
        .map(|(column, value)| format!("{} = {}", quote_ident(column), value))
        .collect::<Vec<_>>()
        .join(", ");
    let rendered_conflict_target = conflict_target
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");

    let translated = format!(
        "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {}",
        quote_ident(table_name),
        rendered_columns,
        rendered_values,
        rendered_conflict_target,
        rendered_updates
    );

    Ok(Some((
        translated,
        vec![format!(
            "rewrote MySQL ON DUPLICATE KEY UPDATE using inferred PostgreSQL conflict target ({})",
            conflict_target.join(", ")
        )],
    )))
}

fn split_sql_csv(input: &str) -> Result<Vec<String>, MiddlewareError> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut depth = 0usize;

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if in_single || in_double => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' if !in_double && !in_backtick => {
                current.push(ch);
                if in_single {
                    if chars.peek() == Some(&'\'') {
                        if let Some(next) = chars.next() {
                            current.push(next);
                        }
                    } else {
                        in_single = false;
                    }
                } else {
                    in_single = true;
                }
            }
            '"' if !in_single && !in_backtick => {
                current.push(ch);
                in_double = !in_double;
            }
            '`' if !in_single && !in_double => {
                current.push(ch);
                in_backtick = !in_backtick;
            }
            '(' if !in_single && !in_double && !in_backtick => {
                depth += 1;
                current.push(ch);
            }
            ')' if !in_single && !in_double && !in_backtick => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if !in_single && !in_double && !in_backtick && depth == 0 => {
                let part = current.trim();
                if part.is_empty() {
                    return Err(MiddlewareError::Translation(
                        "unexpected empty CSV segment while translating SQL".to_string(),
                    ));
                }
                parts.push(part.to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    let part = current.trim();
    if part.is_empty() {
        return Err(MiddlewareError::Translation(
            "unexpected empty trailing CSV segment while translating SQL".to_string(),
        ));
    }
    parts.push(part.to_string());
    Ok(parts)
}

fn normalize_identifier_token(token: &str) -> Result<String, MiddlewareError> {
    let trimmed = token.trim();
    if let Some(identifier) = trimmed.strip_prefix('`').and_then(|value| value.strip_suffix('`')) {
        return Ok(identifier.to_string());
    }
    if let Some(identifier) = trimmed.strip_prefix('"').and_then(|value| value.strip_suffix('"')) {
        return Ok(identifier.to_string());
    }
    if Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")
        .expect("valid identifier regex")
        .is_match(trimmed)
    {
        return Ok(trimmed.to_string());
    }

    Err(MiddlewareError::Translation(format!(
        "unsupported identifier token `{trimmed}` in direct SQL translation"
    )))
}

fn parse_update_assignment(assignment: &str) -> Result<(String, String), MiddlewareError> {
    let mut parts = assignment.splitn(2, '=');
    let left = parts
        .next()
        .ok_or_else(|| MiddlewareError::Translation("missing update assignment column".to_string()))?;
    let right = parts
        .next()
        .ok_or_else(|| MiddlewareError::Translation("missing update assignment value".to_string()))?;
    Ok((
        normalize_identifier_token(left)?,
        normalize_mysql_string_literals(right.trim()),
    ))
}

fn normalize_mysql_string_literals(sql: &str) -> String {
    let mut normalized = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\'' && ch != '"' {
            normalized.push(ch);
            continue;
        }

        let quote = ch;
        let mut value = String::new();

        while let Some(inner) = chars.next() {
            if inner == '\\' {
                match chars.next() {
                    Some('0') => value.push('\0'),
                    Some('b') => value.push('\u{0008}'),
                    Some('n') => value.push('\n'),
                    Some('r') => value.push('\r'),
                    Some('t') => value.push('\t'),
                    Some('Z') => value.push('\u{001a}'),
                    Some(next @ ('\'' | '"' | '\\')) => value.push(next),
                    Some(next) => {
                        value.push('\\');
                        value.push(next);
                    }
                    None => value.push('\\'),
                }
                continue;
            }

            if inner == quote {
                if chars.peek() == Some(&quote) {
                    chars.next();
                    value.push(quote);
                    continue;
                }
                break;
            }

            value.push(inner);
        }

        normalized.push_str(&postgres_string_literal(&value));
    }

    normalized
}

fn postgres_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn infer_on_conflict_target(
    table_name: &str,
    columns: &[String],
    update_assignments: &[(String, String)],
) -> Result<Vec<String>, MiddlewareError> {
    if table_name.eq_ignore_ascii_case("user_language")
        && columns.iter().any(|column| column.eq_ignore_ascii_case("login"))
    {
        return Ok(vec!["login".to_string()]);
    }

    if columns.len() == 1 {
        return Ok(vec![columns[0].clone()]);
    }

    if let Some(first_column) = columns.first() {
        if update_assignments
            .iter()
            .all(|(column, _)| !column.eq_ignore_ascii_case(first_column))
        {
            return Ok(vec![first_column.clone()]);
        }
    }

    Err(MiddlewareError::Translation(
        "ON DUPLICATE KEY UPDATE needs table/key-specific ON CONFLICT translation".to_string(),
    ))
}

fn translate_create_table(
    create: &CreateTable,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if create.or_replace || create.external || create.dynamic || create.global.is_some()
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
    let mut post_statements = Vec::new();

    for column in &create.columns {
        let (rendered, constraints) = translate_column(column, warnings)?;
        rendered_items.push(rendered);
        extra_constraints.extend(constraints);
    }

    for constraint in &create.constraints {
        match constraint {
            TableConstraint::Index(index) => {
                post_statements.push(translate_inline_index(index, &create.name)?);
                warnings.push(format!(
                    "rewrote MySQL inline KEY/INDEX `{}` to a separate PostgreSQL CREATE INDEX statement",
                    index.name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "<unnamed>".to_string())
                ));
            }
            TableConstraint::Unique(unique) if unique_constraint_requires_post_index(unique) => {
                post_statements.push(translate_unique_constraint_as_index(unique, &create.name)?);
                warnings.push(format!(
                    "rewrote MySQL UNIQUE KEY `{}` with index expressions to a separate PostgreSQL CREATE UNIQUE INDEX statement",
                    unique
                        .index_name
                        .as_ref()
                        .or(unique.name.as_ref())
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "<unnamed>".to_string())
                ));
            }
            _ => rendered_items.push(translate_table_constraint(constraint)?),
        }
    }
    rendered_items.extend(extra_constraints);

    if !matches!(create.table_options, sqlparser::ast::CreateTableOptions::None) {
        warnings.push("stripped MySQL-specific CREATE TABLE options".to_string());
    }

    let temporary = if create.temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if create.if_not_exists { "IF NOT EXISTS " } else { "" };
    let create_table_sql = format!(
        "CREATE {temporary}TABLE {if_not_exists}{} ({})",
        quote_object_name(&create.name),
        rendered_items.join(", ")
    );

    if post_statements.is_empty() {
        Ok(create_table_sql)
    } else {
        let mut statements = vec![create_table_sql];
        statements.extend(post_statements);
        Ok(statements.join("; "))
    }
}

fn translate_alter_table(
    alter: &AlterTable,
    warnings: &mut Vec<String>,
) -> Result<String, MiddlewareError> {
    if alter.table_type.is_some() || alter.location.is_some() || alter.on_cluster.is_some() {
        return Err(MiddlewareError::Translation(
            "complex ALTER TABLE variants are not yet supported for PostgreSQL translation".to_string(),
        ));
    }

    let mut operations = Vec::new();
    let mut post_statements = Vec::new();

    for operation in &alter.operations {
        match operation {
            AlterTableOperation::AddConstraint {
                constraint: TableConstraint::Index(index),
                not_valid,
            } => {
                if *not_valid {
                    return Err(MiddlewareError::Translation(format!(
                        "MySQL ADD INDEX `{}` cannot be marked NOT VALID in PostgreSQL translation",
                        index.name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "<unnamed>".to_string())
                    )));
                }
                post_statements.push(translate_inline_index(index, &alter.name)?);
                warnings.push(format!(
                    "rewrote MySQL ALTER TABLE ADD KEY/INDEX `{}` to a separate PostgreSQL CREATE INDEX statement",
                    index.name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "<unnamed>".to_string())
                ));
            }
            AlterTableOperation::AddConstraint {
                constraint,
                not_valid,
            } => {
                let not_valid = if *not_valid { " NOT VALID" } else { "" };
                operations.push(format!("ADD {}{not_valid}", translate_table_constraint(constraint)?));
            }
            AlterTableOperation::AddColumn {
                if_not_exists,
                column_def,
                column_position,
                ..
            } => {
                let (rendered_column, constraints) = translate_column(column_def, warnings)?;
                let if_not_exists = if *if_not_exists { "IF NOT EXISTS " } else { "" };
                operations.push(format!("ADD COLUMN {if_not_exists}{rendered_column}"));
                for constraint in constraints {
                    operations.push(format!("ADD {constraint}"));
                }
                if column_position.is_some() {
                    warnings.push(format!(
                        "dropped MySQL column position clause from ALTER TABLE ADD COLUMN `{}`",
                        column_def.name
                    ));
                }
            }
            AlterTableOperation::ModifyColumn {
                col_name,
                data_type,
                options,
                column_position,
            } => {
                operations.extend(translate_modify_column(
                    col_name,
                    data_type,
                    options,
                    column_position.as_ref(),
                    warnings,
                )?);
            }
            AlterTableOperation::DropIndex { name } => {
                post_statements.push(format!(
                    "DROP INDEX {}",
                    quote_ident(&prefixed_index_name(&alter.name, Some(&name.to_string()), "idx"))
                ));
                warnings.push(format!(
                    "rewrote MySQL ALTER TABLE DROP INDEX `{}` to a separate PostgreSQL DROP INDEX statement",
                    name
                ));
            }
            other => operations.push(other.to_string()),
        }
    }

    let if_exists = if alter.if_exists { "IF EXISTS " } else { "" };
    let only = if alter.only { "ONLY " } else { "" };

    let mut statements = Vec::new();
    if !operations.is_empty() {
        statements.push(format!(
            "ALTER TABLE {if_exists}{only}{} {}",
            quote_object_name(&alter.name),
            operations.join(", ")
        ));
    }
    statements.extend(post_statements);

    if statements.is_empty() {
        return Err(MiddlewareError::Translation(
            "ALTER TABLE statement did not contain translatable operations".to_string(),
        ));
    }

    Ok(statements.join("; "))
}

fn translate_modify_column(
    col_name: &sqlparser::ast::Ident,
    data_type: &DataType,
    options: &[ColumnOption],
    column_position: Option<&sqlparser::ast::MySQLColumnPosition>,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>, MiddlewareError> {
    let mut extra_constraints = Vec::new();
    let translated_type = translate_data_type(
        &col_name.value,
        data_type,
        &mut extra_constraints,
        warnings,
    );
    let quoted_name = quote_ident(&col_name.value);
    let mut operations = vec![format!("ALTER COLUMN {quoted_name} TYPE {translated_type}")];
    let mut nullability = None;
    let mut default = None;
    let mut saw_default = false;

    for option in options {
        match option {
            ColumnOption::Null => nullability = Some(true),
            ColumnOption::NotNull => nullability = Some(false),
            ColumnOption::Default(expr) => {
                saw_default = true;
                default = Some(expr.to_string());
            }
            ColumnOption::CharacterSet(_) | ColumnOption::Collation(_) => {
                warnings.push(format!(
                    "dropped MySQL character set/collation option from ALTER TABLE MODIFY COLUMN `{}`",
                    col_name
                ));
            }
            ColumnOption::Comment(_) => {
                warnings.push(format!(
                    "dropped MySQL column comment from ALTER TABLE MODIFY COLUMN `{}`",
                    col_name
                ));
            }
            ColumnOption::OnUpdate(_) => {
                warnings.push(format!(
                    "dropped MySQL ON UPDATE clause from ALTER TABLE MODIFY COLUMN `{}`; PostgreSQL requires a trigger for equivalent behavior",
                    col_name
                ));
            }
            ColumnOption::DialectSpecific(tokens) if is_auto_increment(tokens) => {
                return Err(MiddlewareError::Translation(format!(
                    "AUTO_INCREMENT cannot be applied by ALTER TABLE MODIFY COLUMN `{}` without recreating identity metadata",
                    col_name
                )));
            }
            other => {
                return Err(MiddlewareError::Translation(format!(
                    "ALTER TABLE MODIFY COLUMN `{}` option `{}` is not supported yet",
                    col_name, other
                )));
            }
        }
    }

    match nullability {
        Some(false) => operations.push(format!("ALTER COLUMN {quoted_name} SET NOT NULL")),
        Some(true) => operations.push(format!("ALTER COLUMN {quoted_name} DROP NOT NULL")),
        None => operations.push(format!("ALTER COLUMN {quoted_name} DROP NOT NULL")),
    }

    if saw_default {
        operations.push(format!(
            "ALTER COLUMN {quoted_name} SET DEFAULT {}",
            default.unwrap_or_else(|| "NULL".to_string())
        ));
    } else {
        operations.push(format!("ALTER COLUMN {quoted_name} DROP DEFAULT"));
    }

    for constraint in extra_constraints {
        operations.push(format!("ADD {constraint}"));
    }

    if column_position.is_some() {
        warnings.push(format!(
            "dropped MySQL column position clause from ALTER TABLE MODIFY COLUMN `{}`",
            col_name
        ));
    }

    Ok(operations)
}

fn translate_table_constraint(constraint: &TableConstraint) -> Result<String, MiddlewareError> {
    match constraint {
        TableConstraint::Unique(unique) => {
            let name = unique
                .name
                .as_ref()
                .map(|name| format!("CONSTRAINT {} ", quote_ident(&name.to_string())))
                .unwrap_or_default();
            let columns = unique
                .columns
                .iter()
                .map(render_index_column)
                .collect::<Vec<_>>()
                .join(", ");
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
                .map(|name| format!("CONSTRAINT {} ", quote_ident(&name.to_string())))
                .unwrap_or_default();
            let columns = primary_key
                .columns
                .iter()
                .map(render_index_column)
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
                .map(|name| format!("CONSTRAINT {} ", quote_ident(&name.to_string())))
                .unwrap_or_default();
            let columns = foreign_key
                .columns
                .iter()
                .map(quote_ident_name)
                .collect::<Vec<_>>()
                .join(", ");
            let referred_columns = foreign_key
                .referred_columns
                .iter()
                .map(quote_ident_name)
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
                quote_object_name(&foreign_key.foreign_table)
            ))
        }
        TableConstraint::Check(check) => {
            let name = check
                .name
                .as_ref()
                .map(|name| format!("CONSTRAINT {} ", quote_ident(&name.to_string())))
                .unwrap_or_default();
            Ok(format!("{name}CHECK ({})", check.expr))
        }
        TableConstraint::Index(_) => Err(MiddlewareError::Translation(
            "MySQL KEY/INDEX constraint should be handled before table constraint rendering".to_string(),
        )),
        TableConstraint::FulltextOrSpatial(index) => Err(MiddlewareError::Translation(format!(
            "MySQL FULLTEXT/SPATIAL constraint `{}` is not supported in PostgreSQL translation",
            index.opt_index_name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "<unnamed>".to_string())
        ))),
    }
}

fn translate_inline_index(index: &IndexConstraint, table_name: &ObjectName) -> Result<String, MiddlewareError> {
    if !index.index_options.is_empty() {
        return Err(MiddlewareError::Translation(format!(
            "MySQL inline KEY/INDEX `{}` with index options is not supported yet",
            index.name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "<unnamed>".to_string())
        )));
    }

    if let Some(index_type) = &index.index_type {
        return Err(MiddlewareError::Translation(format!(
            "MySQL inline KEY/INDEX `{}` USING {index_type} is not supported yet",
            index.name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "<unnamed>".to_string())
        )));
    }

    let index_name = index
        .name
        .as_ref()
        .map(ToString::to_string);
    let index_name = prefixed_index_name(table_name, index_name.as_deref(), "idx");
    let columns = index
        .columns
        .iter()
        .map(render_index_for_create_index)
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");

    Ok(format!(
        "CREATE INDEX {} ON {} ({columns})",
        quote_ident(&index_name),
        quote_object_name(table_name)
    ))
}

fn translate_unique_constraint_as_index(
    unique: &UniqueConstraint,
    table_name: &ObjectName,
) -> Result<String, MiddlewareError> {
    if !unique.index_options.is_empty() {
        return Err(MiddlewareError::Translation(format!(
            "MySQL UNIQUE KEY `{}` with index options is not supported yet",
            unique
                .index_name
                .as_ref()
                .or(unique.name.as_ref())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unnamed>".to_string())
        )));
    }

    if unique.index_type.is_some() {
        return Err(MiddlewareError::Translation(format!(
            "MySQL UNIQUE KEY `{}` USING <index type> is not supported yet",
            unique
                .index_name
                .as_ref()
                .or(unique.name.as_ref())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unnamed>".to_string())
        )));
    }

    if unique.characteristics.is_some()
        || !matches!(unique.nulls_distinct, sqlparser::ast::NullsDistinctOption::None)
    {
        return Err(MiddlewareError::Translation(format!(
            "MySQL UNIQUE KEY `{}` with PostgreSQL-specific constraint options is not supported in expression-index rewriting",
            unique
                .index_name
                .as_ref()
                .or(unique.name.as_ref())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unnamed>".to_string())
        )));
    }

    let index_name = unique
        .index_name
        .as_ref()
        .or(unique.name.as_ref())
        .map(ToString::to_string);
    let index_name = prefixed_index_name(table_name, index_name.as_deref(), "uniq_idx");
    let columns = unique
        .columns
        .iter()
        .map(render_index_for_create_index)
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");

    Ok(format!(
        "CREATE UNIQUE INDEX {} ON {} ({columns})",
        quote_ident(&index_name),
        quote_object_name(table_name)
    ))
}

fn object_name_tail(name: &ObjectName) -> String {
    name.0
        .last()
        .map(ToString::to_string)
        .unwrap_or_else(|| name.to_string())
}

fn prefixed_index_name(table_name: &ObjectName, index_name: Option<&str>, fallback_suffix: &str) -> String {
    let table_index_prefix = sanitize_identifier_for_index_name(&object_name_tail(table_name));
    index_name
        .map(sanitize_identifier_for_index_name)
        .map(|sanitized| {
            if sanitized.starts_with(&format!("{table_index_prefix}_")) {
                sanitized
            } else {
                format!("{table_index_prefix}_{sanitized}")
            }
        })
        .unwrap_or_else(|| format!("{table_index_prefix}_{fallback_suffix}"))
}

fn sanitize_identifier_for_index_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
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

    let mut rendered = format!("{} {}", quote_ident(&column.name.value), translated_type);
    if !rendered_options.is_empty() {
        rendered.push(' ');
        rendered.push_str(&rendered_options.join(" "));
    }

    Ok((rendered, extra_constraints))
}

fn is_auto_increment(tokens: &[sqlparser::tokenizer::Token]) -> bool {
    tokens.len() == 1 && tokens[0].to_string().eq_ignore_ascii_case("AUTO_INCREMENT")
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_object_name(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|ident| quote_ident(&ident.value))
                .unwrap_or_else(|| part.to_string())
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn quote_ident_name(name: &sqlparser::ast::Ident) -> String {
    quote_ident(&name.value)
}

fn render_index_column(name: &sqlparser::ast::IndexColumn) -> String {
    let column = quote_ident(
        &name
            .column
            .expr
            .to_string()
            .trim_matches('`')
            .trim_matches('"')
            .to_string(),
    );
    let order = name
        .column
        .options
        .asc
        .map(|asc| if asc { " ASC" } else { " DESC" })
        .unwrap_or_default();
    let operator_class = name
        .operator_class
        .as_ref()
        .map(|class| format!(" {}", quote_object_name(class)))
        .unwrap_or_default();
    format!("{column}{order}{operator_class}")
}

fn unique_constraint_requires_post_index(unique: &UniqueConstraint) -> bool {
    unique.columns.iter().any(index_column_uses_expression)
}

fn index_column_uses_expression(name: &sqlparser::ast::IndexColumn) -> bool {
    !matches!(name.column.expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
}

fn render_index_for_create_index(
    name: &sqlparser::ast::IndexColumn,
) -> Result<String, MiddlewareError> {
    let (target, is_expression) = render_index_target_expr(&name.column.expr)?;
    let target = if is_expression {
        format!("({target})")
    } else {
        target
    };
    let order = name
        .column
        .options
        .asc
        .map(|asc| if asc { " ASC" } else { " DESC" })
        .unwrap_or_default();
    let operator_class = name
        .operator_class
        .as_ref()
        .map(|class| format!(" {}", quote_object_name(class)))
        .unwrap_or_default();
    Ok(format!("{target}{order}{operator_class}"))
}

fn render_index_target_expr(expr: &Expr) -> Result<(String, bool), MiddlewareError> {
    match expr {
        Expr::Identifier(ident) => Ok((quote_ident(&ident.value), false)),
        Expr::CompoundIdentifier(parts) => Ok((
            parts
                .iter()
                .map(|part| quote_ident(&part.value))
                .collect::<Vec<_>>()
                .join("."),
            false,
        )),
        Expr::Function(function) => render_mysql_prefix_index_expr(function),
        other => Err(MiddlewareError::Translation(format!(
            "MySQL index expression `{other}` is not supported yet"
        ))),
    }
}

fn render_mysql_prefix_index_expr(
    function: &sqlparser::ast::Function,
) -> Result<(String, bool), MiddlewareError> {
    if function.uses_odbc_syntax
        || !matches!(function.parameters, sqlparser::ast::FunctionArguments::None)
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || function.over.is_some()
        || !function.within_group.is_empty()
    {
        return Err(MiddlewareError::Translation(format!(
            "MySQL index expression `{function}` is not supported yet"
        )));
    }

    let [name_part] = function.name.0.as_slice() else {
        return Err(MiddlewareError::Translation(format!(
            "MySQL index expression `{function}` is not supported yet"
        )));
    };
    let Some(column_ident) = name_part.as_ident() else {
        return Err(MiddlewareError::Translation(format!(
            "MySQL index expression `{function}` is not supported yet"
        )));
    };
    let sqlparser::ast::FunctionArguments::List(args) = &function.args else {
        return Err(MiddlewareError::Translation(format!(
            "MySQL index expression `{function}` is not supported yet"
        )));
    };
    if args.duplicate_treatment.is_some() || !args.clauses.is_empty() || args.args.len() != 1 {
        return Err(MiddlewareError::Translation(format!(
            "MySQL index expression `{function}` is not supported yet"
        )));
    }
    let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(length_expr)) = &args.args[0] else {
        return Err(MiddlewareError::Translation(format!(
            "MySQL index expression `{function}` is not supported yet"
        )));
    };
    let length = length_expr.to_string();
    if !length.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(MiddlewareError::Translation(format!(
            "MySQL index prefix length `{length}` in `{function}` is not supported yet"
        )));
    }

    Ok((
        format!("left({}, {length})", quote_ident(&column_ident.value)),
        true,
    ))
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
        (r"(?i)\bDATABASE\s*\(", "CURRENT_DATABASE("),
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

    let get_lock_re = Regex::new(r"(?i)\bGET_LOCK\s*\(\s*([^,]+?)\s*,\s*([^)]+?)\s*\)").expect("valid regex");
    if get_lock_re.is_match(&out) {
        warnings.push("rewrote MySQL GET_LOCK(name, timeout) to PostgreSQL advisory locking".to_string());
        out = get_lock_re
            .replace_all(&out, |caps: &Captures| {
                let name_expr = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                format!(
                    "CASE WHEN pg_try_advisory_lock(hashtextextended(CAST({name_expr} AS text), 0)) THEN 1 ELSE 0 END"
                )
            })
            .to_string();
    }

    let release_lock_re = Regex::new(r"(?i)\bRELEASE_LOCK\s*\(\s*([^)]+?)\s*\)").expect("valid regex");
    if release_lock_re.is_match(&out) {
        warnings.push("rewrote MySQL RELEASE_LOCK(name) to PostgreSQL advisory unlock".to_string());
        out = release_lock_re
            .replace_all(&out, |caps: &Captures| {
                let name_expr = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                format!(
                    "CASE WHEN pg_advisory_unlock(hashtextextended(CAST({name_expr} AS text), 0)) THEN 1 ELSE 0 END"
                )
            })
            .to_string();
    }

    out
}

fn strip_mysql_select_modifiers(sql: &str, warnings: &mut Vec<String>) -> String {
    let re = Regex::new(r"(?i)\bSELECT\s+(DISTINCT\s+)?SQL_NO_CACHE\s+").expect("valid regex");
    if re.is_match(sql) {
        warnings.push("stripped MySQL SELECT modifier SQL_NO_CACHE".to_string());
    }
    re.replace_all(sql, |caps: &Captures| {
        let distinct = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        format!("SELECT {distinct}")
    })
    .to_string()
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
    let sql_without_quoted_values = mask_quoted_sql_fragments(sql);
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
        if re.is_match(&sql_without_quoted_values) {
            return Err(MiddlewareError::Translation(message.to_string()));
        }
    }

    Ok(())
}

fn mask_quoted_sql_fragments(sql: &str) -> String {
    let mut masked = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' | '"' => {
                let quote = ch;
                masked.push(' ');

                while let Some(inner) = chars.next() {
                    masked.push(' ');

                    if inner == '\\' {
                        if chars.next().is_some() {
                            masked.push(' ');
                        }
                        continue;
                    }

                    if inner == quote {
                        if chars.peek() == Some(&quote) {
                            chars.next();
                            masked.push(' ');
                            continue;
                        }
                        break;
                    }
                }
            }
            _ => masked.push(ch),
        }
    }

    masked
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
            "SELECT IFNULL(name, 'x'), FROM_UNIXTIME(created_at), UNIX_TIMESTAMP(updated_at), RAND(), DATABASE() FROM users",
            &TranslatorConfig::default(),
        ).unwrap();
        assert!(result.translated_sql.contains("COALESCE(name, 'x')"));
        assert!(result.translated_sql.contains("TO_TIMESTAMP(created_at)"));
        assert!(result.translated_sql.contains("EXTRACT(EPOCH FROM updated_at)"));
        assert!(result.translated_sql.contains("RANDOM()"));
        assert!(result.translated_sql.contains("CURRENT_DATABASE()"));
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

        assert!(result.translated_sql.contains("CREATE TABLE IF NOT EXISTS \"order_details\""));
        assert!(result.translated_sql.contains("\"order_id\" BIGINT NOT NULL GENERATED BY DEFAULT AS IDENTITY"));
        assert!(result.translated_sql.contains("\"product_id\" BIGINT NOT NULL"));
        assert!(result.translated_sql.contains("\"customer_id\" BIGINT NOT NULL"));
        assert!(result.translated_sql.contains("\"status\" TEXT DEFAULT 'pending'"));
        assert!(result.translated_sql.contains("CHECK (status IN ('pending', 'shipped', 'delivered', 'cancelled'))"));
        assert!(result.translated_sql.contains("CHECK (product_id >= 0 AND product_id <= 4294967295)"));
        assert!(result.translated_sql.contains("CHECK (customer_id >= 0 AND customer_id <= 4294967295)"));
        assert!(!result.translated_sql.contains("AUTO_INCREMENT"));
        assert!(!result.translated_sql.contains("UNSIGNED"));
        assert!(!result.translated_sql.contains("ON UPDATE CURRENT_TIMESTAMP"));
        assert!(!result.translated_sql.contains("ENGINE = InnoDB"));
    }

    #[test]
    fn rewrites_mysql_create_table_reserved_identifiers_for_postgres() {
        let result = translate_sql(
            "CREATE TABLE user (`group` INT, option_value TEXT, PRIMARY KEY (`group`))",
            &TranslatorConfig::default(),
        )
        .unwrap();

        assert!(result.translated_sql.contains("CREATE TABLE \"user\""));
        assert!(result.translated_sql.contains("\"group\" INTEGER"));
        assert!(result.translated_sql.contains("\"option_value\" TEXT"));
        assert!(result.translated_sql.contains("PRIMARY KEY (\"group\")"));
    }
}
