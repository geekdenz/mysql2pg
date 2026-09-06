use std::{
    num::NonZeroUsize,
    ops::ControlFlow,
    sync::{Mutex, OnceLock},
};

use lru::LruCache;
use regex::{Captures, Regex};
use serde::Serialize;
use sqlparser::ast::{
    AlterTable, AlterTableOperation, ColumnDef, ColumnOption, CreateTable, DataType,
    helpers::attached_token::AttachedToken, BinaryOperator, CaseWhen, CastKind, ExactNumberInfo, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArgumentList,
    FunctionArguments, GroupByExpr, Ident, ObjectName, OnConflict, OnConflictAction, OnInsert,
    OrderByKind, Query, Select, SelectItem, SetExpr, ShowCharset, ShowCreateObject, ShowStatementFilter,
    ShowStatementFilterPosition, ShowStatementInParentType, ShowStatementOptions, Statement,
    TableConstraint, TableFactor, TimezoneInfo, UnaryOperator, Value, Visit, VisitMut, Visitor, VisitorMut,
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

/// Number of distinct (config, SQL text) translations kept in the cache. MySQL-native
/// apps like Matomo re-issue the same query shape, often byte-identical, on almost
/// every request, so caching the (pure, deterministic) translation step avoids
/// repeating the AST parse and regex rewrite passes for repeated queries.
const TRANSLATION_CACHE_CAPACITY: usize = 1024;

type TranslationCacheKey = (TranslatorConfig, String);

fn translation_cache() -> &'static Mutex<LruCache<TranslationCacheKey, TranslationResult>> {
    static CACHE: OnceLock<Mutex<LruCache<TranslationCacheKey, TranslationResult>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(TRANSLATION_CACHE_CAPACITY).expect("cache capacity is nonzero"),
        ))
    })
}

pub fn translate_sql(sql: &str, cfg: &TranslatorConfig) -> Result<TranslationResult, MiddlewareError> {
    let cache_key = (cfg.clone(), sql.to_string());
    if let Some(cached) = translation_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
    {
        return Ok(cached.clone());
    }

    let result = translate_sql_uncached(sql, cfg)?;

    translation_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .put(cache_key, result.clone());

    Ok(result)
}

fn translate_sql_uncached(sql: &str, cfg: &TranslatorConfig) -> Result<TranslationResult, MiddlewareError> {
    let original_sql = sql.to_string();
    let (sql, dropped_statement_timeout_hint) = strip_mariadb_max_statement_time_hint(sql);
    let (sql, rewrote_drop_temporary) = strip_drop_temporary_keyword(sql);
    let (sql, rewrote_with_rollup) = rewrite_with_rollup(&sql);
    let (sql, rewrote_ranking_query) = rewrite_mysql_ranking_query(&sql);
    let (sql, inlined_session_variables) = inline_session_variable_expressions(&sql);

    let mysql_sql = normalize_mysql_double_quoted_string_literals(&sql);
    let direct_translation = translate_unparsed_sql(&mysql_sql)?;
    let statements = match direct_translation.as_ref() {
        Some(_) => Vec::new(),
        None => parse_mysql_sql(&mysql_sql)?,
    };
    if statements.is_empty() {
        if let Some((canonical_mysql_sql, mut translated, mut warnings)) = direct_translation {
            if cfg.normalize_mysql_backticks {
                translated = replace_backticks(&translated);
            }
            translated = rewrite_mysql_system_variables(&translated, &mut warnings);
            translated = rewrite_mysql_hex_literals(&translated);
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
            if dropped_statement_timeout_hint {
                warnings.push(MARIADB_STATEMENT_TIMEOUT_HINT_WARNING.to_string());
            }
            if rewrote_ranking_query {
                warnings.push(RANKING_QUERY_REWRITE_WARNING.to_string());
            }
            if inlined_session_variables {
                warnings.push(SESSION_VARIABLE_INLINE_WARNING.to_string());
            }
            if rewrote_with_rollup {
                warnings.push(WITH_ROLLUP_REWRITE_WARNING.to_string());
            }
            if rewrote_drop_temporary {
                warnings.push(DROP_TEMPORARY_REWRITE_WARNING.to_string());
            }

            return Ok(TranslationResult {
                original_sql,
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
    translated = rewrite_mysql_hex_literals(&translated);
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
    if dropped_statement_timeout_hint {
        warnings.push(MARIADB_STATEMENT_TIMEOUT_HINT_WARNING.to_string());
    }
    if rewrote_ranking_query {
        warnings.push(RANKING_QUERY_REWRITE_WARNING.to_string());
    }
    if inlined_session_variables {
        warnings.push(SESSION_VARIABLE_INLINE_WARNING.to_string());
    }
    if rewrote_with_rollup {
        warnings.push(WITH_ROLLUP_REWRITE_WARNING.to_string());
    }
    if rewrote_drop_temporary {
        warnings.push(DROP_TEMPORARY_REWRITE_WARNING.to_string());
    }

    if requires_unsupported_rejection {
        reject_unsupported(&translated)?;
    }

    Ok(TranslationResult {
        original_sql,
        canonical_mysql_sql,
        translated_sql: translated,
        warnings,
    })
}

const MARIADB_STATEMENT_TIMEOUT_HINT_WARNING: &str =
    "dropped MariaDB's `SET STATEMENT max_statement_time=... FOR` per-statement timeout hint; PostgreSQL has no equivalent single-statement timeout syntax";

/// Rewrites MySQL's `GROUP BY a, b WITH ROLLUP` into standard `GROUP BY ROLLUP(a, b)`,
/// which PostgreSQL supports natively. sqlparser rejects the MySQL spelling outright,
/// so this has to happen before parsing.
///
/// Each `WITH ROLLUP` is matched back to its own nearest preceding `GROUP BY` rather
/// than with one spanning regex: a query can contain several `GROUP BY` clauses at
/// different nesting levels, and pairing the wrong two would rewrite across a
/// subquery boundary.
fn rewrite_with_rollup(sql: &str) -> (String, bool) {
    static ROLLUP_RE: OnceLock<Regex> = OnceLock::new();
    static GROUP_BY_RE: OnceLock<Regex> = OnceLock::new();
    let rollup_re = ROLLUP_RE.get_or_init(|| Regex::new(r"(?is)\bWITH\s+ROLLUP\b").expect("valid rollup regex"));
    let group_by_re = GROUP_BY_RE.get_or_init(|| Regex::new(r"(?is)\bGROUP\s+BY\s+").expect("valid group by regex"));

    let matches: Vec<_> = rollup_re.find_iter(sql).map(|m| (m.start(), m.end())).collect();
    if matches.is_empty() {
        return (sql.to_string(), false);
    }

    let mut out = sql.to_string();
    let mut changed = false;
    for (rollup_start, rollup_end) in matches.into_iter().rev() {
        let Some(group_by) = group_by_re.find_iter(&out[..rollup_start]).last() else {
            continue;
        };
        let exprs = out[group_by.end()..rollup_start].trim();
        if exprs.is_empty() {
            continue;
        }
        let replacement = format!("GROUP BY ROLLUP({exprs})");
        out.replace_range(group_by.start()..rollup_end, &replacement);
        changed = true;
    }
    (out, changed)
}

/// MySQL distinguishes `DROP TEMPORARY TABLE` from `DROP TABLE`; PostgreSQL drops a
/// temporary table with plain `DROP TABLE` and rejects the keyword.
fn strip_drop_temporary_keyword(sql: &str) -> (String, bool) {
    static DROP_TEMP_RE: OnceLock<Regex> = OnceLock::new();
    let re = DROP_TEMP_RE
        .get_or_init(|| Regex::new(r"(?is)\bDROP\s+TEMPORARY\s+TABLE\b").expect("valid drop temporary regex"));
    if !re.is_match(sql) {
        return (sql.to_string(), false);
    }
    (re.replace_all(sql, "DROP TABLE").into_owned(), true)
}

const DROP_TEMPORARY_REWRITE_WARNING: &str =
    "rewrote MySQL's `DROP TEMPORARY TABLE` to `DROP TABLE`; PostgreSQL drops temporary tables with the same statement as permanent ones";

const WITH_ROLLUP_REWRITE_WARNING: &str =
    "rewrote MySQL's `GROUP BY ... WITH ROLLUP` to standard `GROUP BY ROLLUP(...)`";

const SESSION_VARIABLE_INLINE_WARNING: &str =
    "inlined MySQL session-variable assignments (`@var := expr`) used within a single expression";

/// Inlines MySQL session variables that are assigned and reused inside one
/// expression.
///
/// Apps use `(@v := <expr>)` to name a sub-expression and refer to it again later in
/// the same statement, e.g.
/// `CASE WHEN (@i := SUBSTRING(...)) = '' THEN -1 ELSE @i END`. PostgreSQL has no
/// session variables, but this idiom carries no state between rows: substituting the
/// assigned expression for both the assignment and its later references preserves
/// the meaning exactly (at the cost of evaluating it more than once).
///
/// Self-referential assignments such as `@counter := @counter + 1` are accumulators
/// across rows, not sub-expression names, so they are left untouched — those are
/// handled by [`rewrite_mysql_ranking_query`] where the shape is recognised, and
/// otherwise fail loudly rather than being mistranslated.
fn inline_session_variable_expressions(sql: &str) -> (String, bool) {
    if !sql.contains(":=") {
        return (sql.to_string(), false);
    }

    static ASSIGN_RE: OnceLock<Regex> = OnceLock::new();
    let assign_re = ASSIGN_RE.get_or_init(|| {
        Regex::new(r"(?is)\(\s*@([A-Za-z_][A-Za-z0-9_]*)\s*:=\s*").expect("valid session variable regex")
    });

    let mut out = sql.to_string();
    let mut changed = false;

    // Repeatedly rewrite the first assignment until none is left; each pass may
    // expose another one nested inside the expression it inlined.
    for _ in 0..MAX_SESSION_VARIABLE_INLINE_PASSES {
        let Some(caps) = assign_re.captures(&out) else {
            break;
        };
        let whole = caps.get(0).expect("match 0 always present");
        let name = caps.get(1).expect("capture 1 always present").as_str().to_string();

        // The assignment is wrapped in parentheses; find the one that closes it.
        let open_paren = whole.start();
        let Some(close_paren) = matching_paren_in(&out, open_paren) else {
            break;
        };
        let value = out[whole.end()..close_paren].trim().to_string();

        // An accumulator refers to itself; leave it for the ranking-query rewrite.
        if value.contains(&format!("@{name}")) {
            break;
        }

        let replacement = format!("({value})");
        let mut rewritten = String::with_capacity(out.len());
        rewritten.push_str(&out[..open_paren]);
        rewritten.push_str(&replacement);
        rewritten.push_str(&out[close_paren + 1..]);

        // Substitute the variable's later *references*. Inlining duplicates the
        // expression, which can duplicate an assignment nested inside it, so an
        // occurrence followed by `:=` is another assignment and must be left for a
        // later pass rather than overwritten.
        out = replace_session_variable_references(&rewritten, &name, &replacement);
        changed = true;
    }

    (out, changed)
}


/// Replaces `@name` where it *reads* the variable, leaving `@name :=` assignments
/// for a later inlining pass.
fn replace_session_variable_references(sql: &str, name: &str, replacement: &str) -> String {
    let needle = format!("@{name}");
    let mut out = String::with_capacity(sql.len());
    let mut cursor = 0usize;

    while let Some(found) = sql[cursor..].find(&needle) {
        let start = cursor + found;
        let end = start + needle.len();

        // `@foo` must not match the prefix of `@foobar`.
        let boundary_ok = sql[end..]
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric() && c != '_')
            .unwrap_or(true);
        let is_assignment = sql[end..].trim_start().starts_with(":=");

        out.push_str(&sql[cursor..start]);
        if boundary_ok && !is_assignment {
            out.push_str(replacement);
        } else {
            out.push_str(&sql[start..end]);
        }
        cursor = end;
    }

    out.push_str(&sql[cursor..]);
    out
}

/// Bounded so a pathological statement cannot loop; deeply nested assignments beyond
/// this are left in place and fail loudly.
const MAX_SESSION_VARIABLE_INLINE_PASSES: usize = 16;

/// Index of the `)` matching the `(` at `open`, ignoring parentheses inside quotes.
fn matching_paren_in(sql: &str, open: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    if bytes.get(open)? != &b'(' {
        return None;
    }
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    for (idx, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'\'' if !in_double && !in_backtick => in_single = !in_single,
            b'"' if !in_single && !in_backtick => in_double = !in_double,
            b'`' if !in_single && !in_double => in_backtick = !in_backtick,
            b'(' if !in_single && !in_double && !in_backtick => depth += 1,
            b')' if !in_single && !in_double && !in_backtick => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

const RANKING_QUERY_REWRITE_WARNING: &str =
    "rewrote MySQL ranking-query session counters (`@counter := @counter + 1`) to PostgreSQL ROW_NUMBER() window functions";

/// Rewrites the "ranking query" MySQL emits via user session variables into
/// PostgreSQL window functions.
///
/// Matomo's `Piwik\RankingQuery` builds top-N reports by cross-joining counter
/// initializers into the FROM clause and incrementing them per row:
///
/// ```sql
/// FROM ( SELECT @counter1:=0 ) initCounter1, ( SELECT @counter2:=0 ) initCounter2,
///      ( <inner query> ORDER BY `12` DESC, name ASC ) actualQuery
/// -- with, in the SELECT list:
/// CASE WHEN `type` = 1 AND @counter1 = 50001 THEN 50001
///      WHEN `type` = 1 THEN @counter1:=@counter1+1
///      ... one pair per partition value ...
///      ELSE 0 END AS counter
/// ```
///
/// PostgreSQL has no user session variables, but the intent — number the rows of
/// each partition in the inner query's sort order, then clamp at the limit so
/// everything past the top N collapses into one "others" bucket — is exactly
/// `LEAST(ROW_NUMBER() OVER (PARTITION BY <col> ORDER BY <inner order>), <limit>)`.
/// Clamping matches MySQL: the counter stops incrementing once it reaches the
/// limit, so every row from the limit-th onward shares that value.
///
/// Returns the rewritten SQL and whether anything changed. Queries using the
/// `WITH ROLLUP` variant (`@counterRollup`) are left untouched: their NULL-marking
/// interacts with the counters in ways this rewrite does not model, and failing
/// loudly beats silently returning wrong report numbers.
fn rewrite_mysql_ranking_query(sql: &str) -> (String, bool) {
    if !sql.contains("@counter") {
        return (sql.to_string(), false);
    }
    let Some(order_by) = ranking_query_inner_order_by(sql) else {
        return (sql.to_string(), false);
    };

    let mut out = strip_ranking_query_counter_initializers(sql);
    let mut changed = false;

    // With ROLLUP, the counters skip the rollup rows (which get -1 or 0 instead of a
    // number), so plain row numbering would over-count. Numbering inside a partition
    // keyed on the row's rollup class reproduces MySQL exactly.
    let rollup_class = rollup_row_class_expression(&out);

    if let Some(rewritten) = rewrite_rollup_ranking_counter(&out, &order_by, rollup_class.as_deref()) {
        out = rewritten;
        changed = true;
    }

    let main_partition = main_counter_partition_expression(&out);
    if let Some(rewritten) = rewrite_partitioned_ranking_counters(&out, &order_by) {
        out = rewritten;
        changed = true;
    } else if let Some(rewritten) =
        rewrite_single_ranking_counter(&out, &order_by, main_partition.as_deref())
    {
        out = rewritten;
        changed = true;
    }

    if out.contains("@counter") {
        // Something in this query's counter shape was not recognised; leave the
        // statement untouched so it fails loudly rather than returning wrong numbers.
        return (sql.to_string(), false);
    }

    if changed {
        (coerce_ranking_query_others_labels(&out), true)
    } else {
        (sql.to_string(), false)
    }
}

/// Makes the "others" bucket's CASE branches agree on a type.
///
/// RankingQuery labels the overflow bucket by swapping in a string:
/// `CASE WHEN counter = <limit> THEN '__mtm_ranking_query_others__' ELSE `idaction` END`.
/// MySQL happily unifies the text literal with a numeric column; PostgreSQL rejects
/// the mismatch. Casting the column branch to TEXT matches what MySQL returns here —
/// Matomo compares these values against the label string. `CHAR` would be wrong:
/// PostgreSQL reads it as `character(1)` and would truncate.
fn coerce_ranking_query_others_labels(sql: &str) -> String {
    static OTHERS_RE: OnceLock<Regex> = OnceLock::new();
    let re = OTHERS_RE.get_or_init(|| {
        Regex::new(r"(?is)(WHEN\s+counter\s*=\s*\d+\s+THEN\s+'(?:[^']|'')*'\s+ELSE\s+)(`[^`]+`|\w+)(\s+END)")
            .expect("valid ranking others-label regex")
    });
    re.replace_all(sql, "${1}CAST(${2} AS TEXT)${3}").into_owned()
}

/// The inner `( ... ORDER BY <expr list> ) actualQuery` sort order, which the
/// window functions must reproduce — MySQL's counters increment in exactly that
/// order.
fn ranking_query_inner_order_by(sql: &str) -> Option<String> {
    static ORDER_RE: OnceLock<Regex> = OnceLock::new();
    let re = ORDER_RE.get_or_init(|| {
        Regex::new(r"(?is)ORDER\s+BY\s+(.+?)\s*\)\s*actualQuery").expect("valid ranking order-by regex")
    });
    let caps = re.captures(sql)?;
    let order_by = caps.get(1)?.as_str().trim();
    if order_by.is_empty() {
        return None;
    }
    // Only the text before this ORDER BY can define the subquery's output columns.
    let preceding = &sql[..caps.get(0)?.start()];
    Some(resolve_order_terms_to_inner_aliases(
        &normalize_whitespace(order_by),
        preceding,
    ))
}

/// Rewrites `<table>.<column>` sort terms to the alias the inner query exposes them
/// under.
///
/// The window function lives in the query *outside* `actualQuery`, where only that
/// subquery's output columns are in scope — a qualified reference like
/// `log_action.name` would resolve to nothing. The bare column name is the right
/// reference, whether the subquery aliased it explicitly (`... AS name`) or
/// projected it bare (`log_action.name`, whose implicit output name is `name`), so
/// substitute only when the subquery actually projects that column.
fn resolve_order_terms_to_inner_aliases(order_by: &str, preceding_sql: &str) -> String {
    static QUALIFIED_RE: OnceLock<Regex> = OnceLock::new();
    let qualified = QUALIFIED_RE.get_or_init(|| {
        Regex::new(r"(?is)^(`?[A-Za-z_][A-Za-z0-9_]*`?)\.(`?)([A-Za-z_][A-Za-z0-9_]*)`?(\s+(?:ASC|DESC))?$")
            .expect("valid qualified order term regex")
    });

    order_by
        .split(',')
        .map(|term| {
            let term = term.trim();
            let Some(caps) = qualified.captures(term) else {
                return term.to_string();
            };
            let qualifier = caps[1].trim_matches('`');
            let column = &caps[3];
            let direction = caps.get(4).map(|m| m.as_str()).unwrap_or("");
            if inner_query_projects_column(preceding_sql, qualifier, column) {
                format!("{column}{direction}")
            } else {
                term.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// True when the subquery exposes `column` as an output name: either aliased
/// explicitly, or projected as a bare `qualifier.column` reference (whose implicit
/// output name is the column name).
fn inner_query_projects_column(preceding_sql: &str, qualifier: &str, column: &str) -> bool {
    let aliased = format!(r"(?is)\bAS\s+`?{}`?\s*(?:,|\bFROM\b)", regex::escape(column));
    let projected = format!(
        r"(?is)\b`?{}`?\s*\.\s*`?{}`?",
        regex::escape(qualifier),
        regex::escape(column)
    );
    [aliased, projected]
        .iter()
        .any(|pattern| Regex::new(pattern).map(|re| re.is_match(preceding_sql)).unwrap_or(false))
}

fn strip_ranking_query_counter_initializers(sql: &str) -> String {
    static INIT_RE: OnceLock<Regex> = OnceLock::new();
    let re = INIT_RE.get_or_init(|| {
        Regex::new(r"(?is)\(\s*SELECT\s+@counter[A-Za-z0-9_]*\s*:=\s*0\s*\)\s*initCounter[A-Za-z0-9_]*\s*,\s*")
            .expect("valid counter initializer regex")
    });
    re.replace_all(sql, "").into_owned()
}

/// Collapses the per-partition `WHEN ... THEN @counterN:=@counterN+1` pairs into a
/// single partitioned ROW_NUMBER() branch.
fn rewrite_partitioned_ranking_counters(sql: &str, order_by: &str) -> Option<String> {
    // The guard is matched as the exact ``​`col` = <int>`` shape RankingQuery emits
    // rather than a general expression: a loose wildcard here latches onto the
    // *outer* `CASE WHEN counter = <limit>` (which reads the counter column, not a
    // session variable) and swallows the rest of the query.
    static PAIR_RE: OnceLock<Regex> = OnceLock::new();
    let re = PAIR_RE.get_or_init(|| {
        Regex::new(
            r"(?is)WHEN\s+(`[^`]+`|\w+)\s*=\s*(-?\d+)\s+AND\s+@counter[A-Za-z0-9_]+\s*=\s*(\d+)\s+THEN\s+\d+\s+WHEN\s+(`[^`]+`|\w+)\s*=\s*(-?\d+)\s+THEN\s+@counter[A-Za-z0-9_]+\s*:=\s*@counter[A-Za-z0-9_]+\s*\+\s*1",
        )
        .expect("valid partitioned ranking counter regex")
    });

    let mut partition_column: Option<String> = None;
    let mut values: Vec<String> = Vec::new();
    let mut limit: Option<String> = None;
    let mut span: Option<(usize, usize)> = None;

    for caps in re.captures_iter(sql) {
        let column = caps.get(1)?.as_str().to_string();
        let value = caps.get(2)?.as_str().to_string();
        // Both WHEN arms of a pair must test the same partition value; if MySQL
        // emitted something else, this is not the shape we know how to rewrite.
        if column != caps.get(4)?.as_str() || value != caps.get(5)?.as_str() {
            return None;
        }
        match &partition_column {
            Some(existing) if *existing != column => return None,
            Some(_) => {}
            None => partition_column = Some(column),
        }
        let pair_limit = caps.get(3)?.as_str().to_string();
        match &limit {
            Some(existing) if *existing != pair_limit => return None,
            Some(_) => {}
            None => limit = Some(pair_limit),
        }
        values.push(value);

        let whole = caps.get(0)?;
        span = Some(match span {
            Some((start, _)) => (start, whole.end()),
            None => (whole.start(), whole.end()),
        });
    }

    let (column, limit, (start, end)) = (partition_column?, limit?, span?);
    let replacement = format!(
        "WHEN {column} IN ({values}) THEN LEAST(ROW_NUMBER() OVER (PARTITION BY {column} ORDER BY {order_by}), {limit})",
        values = values.join(", ")
    );
    let mut out = String::with_capacity(sql.len());
    out.push_str(&sql[..start]);
    out.push_str(&replacement);
    out.push_str(&sql[end..]);
    Some(out)
}

/// Columns the rollup counter watches, in the order RankingQuery emits them.
fn rollup_columns(sql: &str) -> Vec<String> {
    static ROLLUP_COL_RE: OnceLock<Regex> = OnceLock::new();
    let re = ROLLUP_COL_RE.get_or_init(|| {
        Regex::new(r"(?is)WHEN\s+(`[^`]+`|\w+)\s+IS\s+NULL\s+AND\s+@counterRollup\s*=\s*\d+\s+THEN\s+\d+")
            .expect("valid rollup column regex")
    });
    let mut columns = Vec::new();
    for caps in re.captures_iter(sql) {
        if let Some(column) = caps.get(1) {
            let column = column.as_str().to_string();
            if !columns.contains(&column) {
                columns.push(column);
            }
        }
    }
    columns
}

/// Classifies each row as grand total (all rollup columns NULL), a partial rollup
/// row (some NULL), or a detail row — the partition MySQL's single rollup counter
/// implicitly walks.
fn rollup_row_class_expression(sql: &str) -> Option<String> {
    let columns = rollup_columns(sql);
    if columns.is_empty() {
        return None;
    }
    let all_null = columns
        .iter()
        .map(|column| format!("{column} IS NULL"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let any_null = columns
        .iter()
        .map(|column| format!("{column} IS NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    Some(format!(
        "(CASE WHEN {all_null} THEN 2 WHEN {any_null} THEN 1 ELSE 0 END)"
    ))
}

/// The main counter in a rollup query returns -1 for rollup rows without consuming a
/// number, so it must be numbered within the non-rollup rows only.
fn main_counter_partition_expression(sql: &str) -> Option<String> {
    static MINUS_ONE_RE: OnceLock<Regex> = OnceLock::new();
    let re = MINUS_ONE_RE.get_or_init(|| {
        Regex::new(r"(?is)WHEN\s+(`[^`]+`|\w+)\s+IS\s+NULL\s+THEN\s+-\s*1\b")
            .expect("valid rollup minus-one regex")
    });
    let mut columns: Vec<String> = Vec::new();
    for caps in re.captures_iter(sql) {
        if let Some(column) = caps.get(1) {
            let column = column.as_str().to_string();
            if !columns.contains(&column) {
                columns.push(column);
            }
        }
    }
    if columns.is_empty() {
        return None;
    }
    let any_null = columns
        .iter()
        .map(|column| format!("{column} IS NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    Some(format!("(CASE WHEN {any_null} THEN 1 ELSE 0 END)"))
}

/// Collapses the `@counterRollup` arms into one partitioned ROW_NUMBER() branch.
fn rewrite_rollup_ranking_counter(
    sql: &str,
    order_by: &str,
    row_class: Option<&str>,
) -> Option<String> {
    let row_class = row_class?;
    let columns = rollup_columns(sql);
    if columns.is_empty() {
        return None;
    }

    static PAIR_RE: OnceLock<Regex> = OnceLock::new();
    let re = PAIR_RE.get_or_init(|| {
        Regex::new(
            r"(?is)WHEN\s+(?:`[^`]+`|\w+)\s+IS\s+NULL\s+AND\s+@counterRollup\s*=\s*(\d+)\s+THEN\s+\d+\s+WHEN\s+(?:`[^`]+`|\w+)\s+IS\s+NULL\s+THEN\s+@counterRollup\s*:=\s*@counterRollup\s*\+\s*1",
        )
        .expect("valid rollup counter regex")
    });

    let mut limit: Option<String> = None;
    let mut span: Option<(usize, usize)> = None;
    for caps in re.captures_iter(sql) {
        let pair_limit = caps.get(1)?.as_str().to_string();
        match &limit {
            Some(existing) if *existing != pair_limit => return None,
            Some(_) => {}
            None => limit = Some(pair_limit),
        }
        let whole = caps.get(0)?;
        span = Some(match span {
            Some((start, _)) => (start, whole.end()),
            None => (whole.start(), whole.end()),
        });
    }

    let (limit, (start, end)) = (limit?, span?);
    let any_null = columns
        .iter()
        .map(|column| format!("{column} IS NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let replacement = format!(
        "WHEN {any_null} THEN LEAST(ROW_NUMBER() OVER (PARTITION BY {row_class} ORDER BY {order_by}), {limit})"
    );
    let mut out = String::with_capacity(sql.len());
    out.push_str(&sql[..start]);
    out.push_str(&replacement);
    out.push_str(&sql[end..]);
    Some(out)
}

/// If the span at `start..end` is the entire body of a `CASE ... END` (i.e. removing
/// it would leave no WHEN arms), returns the byte range covering that whole
/// `CASE ... END`.
fn enclosing_whenless_case(sql: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let before = sql[..start].trim_end();
    if before.len() < 4 {
        return None;
    }
    let case_start = before.len() - 4;
    if !before[case_start..].eq_ignore_ascii_case("CASE") {
        return None;
    }

    let after = &sql[end..];
    let trimmed = after.trim_start();
    if trimmed.len() < 3 || !trimmed.get(..3)?.eq_ignore_ascii_case("END") {
        return None;
    }
    let leading_ws = after.len() - trimmed.len();
    Some((case_start, end + leading_ws + 3))
}

/// The unpartitioned form: a single `@counter` for the whole result set.
fn rewrite_single_ranking_counter(
    sql: &str,
    order_by: &str,
    partition: Option<&str>,
) -> Option<String> {
    static SINGLE_RE: OnceLock<Regex> = OnceLock::new();
    let re = SINGLE_RE.get_or_init(|| {
        Regex::new(
            r"(?is)WHEN\s+@counter\s*=\s*(\d+)\s+THEN\s+\d+\s+ELSE\s+@counter\s*:=\s*@counter\s*\+\s*1",
        )
        .expect("valid single ranking counter regex")
    });
    let caps = re.captures(sql)?;
    let limit = caps.get(1)?.as_str();
    let over = match partition {
        Some(partition) => format!("PARTITION BY {partition} ORDER BY {order_by}"),
        None => format!("ORDER BY {order_by}"),
    };
    let numbering = format!("LEAST(ROW_NUMBER() OVER ({over}), {limit})");

    // When these were the CASE's only arms, replacing them in place would leave
    // `CASE ELSE <expr> END`, which is not valid SQL — the whole CASE has to go.
    let whole = caps.get(0)?;
    if let Some((case_start, end_end)) = enclosing_whenless_case(sql, whole.start(), whole.end()) {
        let mut out = String::with_capacity(sql.len());
        out.push_str(&sql[..case_start]);
        out.push_str(&numbering);
        out.push_str(&sql[end_end..]);
        return Some(out);
    }

    let replacement = format!("ELSE {numbering}");
    let whole = caps.get(0)?;
    let mut out = String::with_capacity(sql.len());
    out.push_str(&sql[..whole.start()]);
    out.push_str(&replacement);
    out.push_str(&sql[whole.end()..]);
    Some(out)
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Matomo (via `Piwik\Db\Schema\Mariadb::getMaxExecutionTimeSql`, and MariaDB-aware
/// apps generally) prefixes some SELECTs with MariaDB's `SET STATEMENT
/// max_statement_time=<n> FOR <query>` to cap that one query's execution time. This
/// syntax doesn't parse as SQL at all outside MariaDB, so it must be stripped
/// before translation even attempts to parse the statement — simply running the
/// inner query without a hard timeout is the closest available behavior.
fn strip_mariadb_max_statement_time_hint(sql: &str) -> (&str, bool) {
    static HINT_RE: OnceLock<Regex> = OnceLock::new();
    let re = HINT_RE.get_or_init(|| {
        Regex::new(r"(?is)^\s*SET\s+STATEMENT\s+max_statement_time\s*=\s*\d+\s+FOR\s+").expect("valid statement-timeout hint regex")
    });
    match re.find(sql) {
        Some(m) => (&sql[m.end()..], true),
        None => (sql, false),
    }
}

fn rewrite_mysql_hex_literals(sql: &str) -> String {
    let hex_literal = Regex::new(r"(?i)\bX'([0-9a-f]*)'").expect("valid hex literal regex");
    hex_literal
        .replace_all(sql, |captures: &Captures<'_>| {
            format!("decode('{}', 'hex')", &captures[1])
        })
        .into_owned()
}

fn translate_unparsed_sql(
    sql: &str,
) -> Result<Option<(String, String, Vec<String>)>, MiddlewareError> {
    if let Some((translated_sql, warnings)) = translate_load_data_direct(sql)? {
        return Ok(Some((sql.trim().to_string(), translated_sql, warnings)));
    }

    if let Some((translated_sql, warnings)) = translate_insert_on_duplicate_key_direct(sql)? {
        return Ok(Some((sql.trim().to_string(), translated_sql, warnings)));
    }

    if let Some((translated_sql, warnings)) = translate_show_index_direct(sql)? {
        return Ok(Some((sql.trim().to_string(), translated_sql, warnings)));
    }

    Ok(None)
}

fn translate_load_data_direct(
    sql: &str,
) -> Result<Option<(String, Vec<String>)>, MiddlewareError> {
    let re = Regex::new(
        r#"(?is)^\s*LOAD\s+DATA\s+(LOCAL\s+)?INFILE\s+(?:'([^']*)'|\"([^\"]*)\")\s+INTO\s+TABLE\s+([`\"A-Za-z0-9_.]+)(.*?);?\s*$"#,
    )
    .expect("valid LOAD DATA regex");
    let Some(caps) = re.captures(sql) else {
        return Ok(None);
    };

    if caps.get(1).is_some() {
        return Err(MiddlewareError::Translation(
            "LOAD DATA LOCAL INFILE requires MySQL client-local-infile wire support; server-side LOAD DATA INFILE is supported".to_string(),
        ));
    }

    let path = caps
        .get(2)
        .or_else(|| caps.get(3))
        .map(|m| m.as_str())
        .unwrap_or_default();
    let table = quote_object_name_from_text(caps.get(4).map(|m| m.as_str()).unwrap_or_default());
    let options = caps.get(5).map(|m| m.as_str()).unwrap_or_default();
    let mut delimiter = "\t".to_string();
    let mut line_ending = "\\n".to_string();
    let mut columns = None;

    let field_re = Regex::new(r#"(?is)FIELDS\s+TERMINATED\s+BY\s+(?:'([^']*)'|\"([^\"]*)\")"#).expect("valid fields regex");
    if let Some(field_caps) = field_re.captures(options) {
        delimiter = field_caps
            .get(1)
            .or_else(|| field_caps.get(2))
            .map(|m| m.as_str())
            .map(decode_mysql_copy_escape)
            .unwrap_or_else(|| "\t".to_string());
    }
    let line_re = Regex::new(r#"(?is)LINES\s+TERMINATED\s+BY\s+(?:'([^']*)'|\"([^\"]*)\")"#).expect("valid lines regex");
    if let Some(line_caps) = line_re.captures(options) {
        line_ending = line_caps
            .get(1)
            .or_else(|| line_caps.get(2))
            .map(|m| m.as_str())
            .unwrap_or("\\n")
            .to_string();
    }
    let column_re = Regex::new(r"(?is)\(([^()]*)\)\s*$").expect("valid column list regex");
    if let Some(column_caps) = column_re.captures(options) {
        let rendered = column_caps
            .get(1)
            .map(|m| m.as_str())
            .unwrap_or_default()
            .split(',')
            .map(|column| quote_ident(column.trim().trim_matches('`').trim_matches('"')))
            .collect::<Vec<_>>();
        if !rendered.is_empty() {
            columns = Some(format!(" ({})", rendered.join(", ")));
        }
    }

    let translated = format!(
        "COPY {}{} FROM {} WITH (FORMAT csv, DELIMITER {}, NULL '\\\\N', HEADER false)",
        table,
        columns.unwrap_or_default(),
        quote_string_literal(path),
        quote_copy_literal(&delimiter),
    );
    let mut warnings = vec![
        "rewrote server-side MySQL LOAD DATA INFILE to PostgreSQL COPY".to_string(),
    ];
    if line_ending != "\\n" {
        warnings.push("LOAD DATA line terminators other than newline require PostgreSQL COPY preprocessing".to_string());
    }
    if options.to_ascii_uppercase().contains("IGNORE ") || options.to_ascii_uppercase().contains("REPLACE") {
        warnings.push("LOAD DATA IGNORE/REPLACE semantics are not equivalent to PostgreSQL COPY".to_string());
    }
    Ok(Some((translated, warnings)))
}

fn quote_copy_literal(value: &str) -> String {
    format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

fn decode_mysql_copy_escape(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }
        match chars.next() {
            Some('t') => decoded.push('\t'),
            Some('n') => decoded.push('\n'),
            Some('r') => decoded.push('\r'),
            Some('0') => decoded.push('\0'),
            Some('\\') => decoded.push('\\'),
            Some(other) => {
                decoded.push('\\');
                decoded.push(other);
            }
            None => decoded.push('\\'),
        }
    }
    decoded
}

fn quote_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

fn quote_object_name_from_text(value: &str) -> String {
    value
        .split('.')
        .map(|part| quote_ident(part.trim().trim_matches('`').trim_matches('"')))
        .collect::<Vec<_>>()
        .join(".")
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

fn normalize_mysql_double_quoted_string_literals(sql: &str) -> String {
    let mut normalized = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' | '`' => {
                let quote = ch;
                normalized.push(ch);
                while let Some(inner) = chars.next() {
                    normalized.push(inner);
                    if inner == '\\' {
                        if let Some(next) = chars.next() {
                            normalized.push(next);
                        }
                        continue;
                    }
                    if inner == quote {
                        if chars.peek() == Some(&quote) {
                            if let Some(next) = chars.next() {
                                normalized.push(next);
                            }
                            continue;
                        }
                        break;
                    }
                }
            }
            '"' => {
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
                    if inner == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            value.push('"');
                            continue;
                        }
                        break;
                    }
                    value.push(inner);
                }
                normalized.push_str(&postgres_string_literal(&value));
            }
            _ => normalized.push(ch),
        }
    }

    normalized
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
        r"(?i)@@(?:(?:SESSION|GLOBAL)\.)?(sql_mode|version_comment|version|collation_connection|transaction_isolation|tx_isolation|secure_file_priv)\b",
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
            "secure_file_priv" => "NULL::text".to_string(),
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
        Statement::Query(query) => translate_query(query, warnings),
        _ => Ok(stmt.to_string()),
    }
}

/// MySQL's default (non-`ONLY_FULL_GROUP_BY`) mode lets a `SELECT` list reference a
/// plain column that is neither in `GROUP BY` nor wrapped in an aggregate; MySQL
/// just returns an arbitrary value for it per group. PostgreSQL always enforces the
/// standard-SQL rule and rejects such queries. Matomo's own schema (and other
/// MySQL-native apps) rely on the permissive behavior — its archive-invalidation
/// bookkeeping query selects `report` without grouping or aggregating it — so the
/// middleware rewrites those columns to `MIN(column)` here, which is a valid,
/// deterministic stand-in for MySQL's "any value from the group" semantics.
fn translate_query(query: &Query, warnings: &mut Vec<String>) -> Result<String, MiddlewareError> {
    let mut query = query.clone();
    align_alias_casing(&mut query);
    let mut fixer = RelaxedGroupByFixer { changed: false };
    let _: ControlFlow<()> = VisitMut::visit(&mut query, &mut fixer);
    if fixer.changed {
        warnings.push(
            "wrapped ungrouped, non-aggregated SELECT columns in MIN(...) to satisfy \
             PostgreSQL's stricter GROUP BY rules (MySQL allows this by default)"
                .to_string(),
        );
    }
    Ok(query.to_string())
}

/// Preserves alias casing across the MySQL/PostgreSQL identifier-folding mismatch.
///
/// MySQL identifiers are case-insensitive and a result column keeps the exact case
/// of its alias, so Matomo writes `log_link_visit_action.server_time AS
/// serverTimePretty` and later reads `$row['serverTimePretty']`. PostgreSQL folds
/// *unquoted* identifiers to lower case, so that column comes back as
/// `servertimepretty` and the lookup silently misses.
///
/// Quoting every alias makes PostgreSQL return the original spelling. References are
/// then matched case-insensitively (as MySQL would) and rewritten to the alias's
/// exact spelling, so both sides agree.
fn align_alias_casing(query: &mut Query) {
    let mut aligner = AliasCasingAligner;
    let _: ControlFlow<()> = VisitMut::visit(query, &mut aligner);
}

struct AliasCasingAligner;

impl VisitorMut for AliasCasingAligner {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        let mut own_aliases = Vec::new();
        let mut derived_aliases = Vec::new();

        if let SetExpr::Select(select) = query.body.as_mut() {
            derived_aliases = derived_table_output_aliases(select);

            for item in &mut select.projection {
                if let SelectItem::ExprWithAlias { alias, .. } = item {
                    own_aliases.push(alias.value.clone());
                    alias.quote_style = Some('"');
                }
            }

            // Columns produced by a subquery in FROM are referenceable anywhere at
            // this level, including the projection and WHERE.
            for item in &mut select.projection {
                match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        canonicalize_identifiers(expr, &derived_aliases)
                    }
                    _ => {}
                }
            }
            if let Some(selection) = select.selection.as_mut() {
                canonicalize_identifiers(selection, &derived_aliases);
            }

            // This level's *own* aliases may only be referenced from the grouping
            // clauses. In the projection an identifier naming one is a source column
            // — Matomo writes `idvisit AS idVisit`, and rewriting the left-hand side
            // would point at a column that does not exist.
            let grouping_aliases = [derived_aliases.clone(), own_aliases.clone()].concat();
            if let GroupByExpr::Expressions(exprs, _) = &mut select.group_by {
                for expr in exprs {
                    canonicalize_identifiers(expr, &grouping_aliases);
                }
            }
            if let Some(having) = select.having.as_mut() {
                canonicalize_identifiers(having, &grouping_aliases);
            }
        }

        if let Some(order_by) = query.order_by.as_mut() {
            let grouping_aliases = [derived_aliases, own_aliases].concat();
            if let OrderByKind::Expressions(order_exprs) = &mut order_by.kind {
                for order_expr in order_exprs {
                    canonicalize_identifiers(&mut order_expr.expr, &grouping_aliases);
                }
            }
        }

        ControlFlow::Continue(())
    }
}

/// Aliases produced by subqueries in this SELECT's FROM clause.
fn derived_table_output_aliases(select: &Select) -> Vec<String> {
    let mut aliases = Vec::new();
    for table in &select.from {
        collect_derived_aliases(&table.relation, &mut aliases);
        for join in &table.joins {
            collect_derived_aliases(&join.relation, &mut aliases);
        }
    }
    aliases
}

fn collect_derived_aliases(factor: &TableFactor, aliases: &mut Vec<String>) {
    let TableFactor::Derived { subquery, .. } = factor else {
        return;
    };
    if let SetExpr::Select(select) = subquery.body.as_ref() {
        for item in &select.projection {
            if let SelectItem::ExprWithAlias { alias, .. } = item {
                aliases.push(alias.value.clone());
            }
        }
    }
}

/// Rewrites identifiers that name one of `aliases` to that alias's exact spelling,
/// quoted. The alias definitions are quoted to survive PostgreSQL's case folding, so
/// references have to be quoted identically or they stop resolving.
fn canonicalize_identifiers(expr: &mut Expr, aliases: &[String]) {
    if aliases.is_empty() {
        return;
    }
    let mut canonicalizer = IdentifierCanonicalizer { aliases };
    let _: ControlFlow<()> = VisitMut::visit(expr, &mut canonicalizer);
}

struct IdentifierCanonicalizer<'a> {
    aliases: &'a [String],
}

impl VisitorMut for IdentifierCanonicalizer<'_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::Identifier(ident) = expr {
            let canonical = self
                .aliases
                .iter()
                .find(|alias| alias.eq_ignore_ascii_case(&ident.value));
            if let Some(canonical) = canonical {
                // A reference already double-quoted stated its casing explicitly.
                let rewritable = ident.quote_style.is_none() || ident.quote_style == Some('`');
                if rewritable {
                    ident.value = canonical.clone();
                    ident.quote_style = Some('"');
                }
            }
        }
        ControlFlow::Continue(())
    }
}

struct RelaxedGroupByFixer {
    changed: bool,
}

impl VisitorMut for RelaxedGroupByFixer {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        let group_exprs = match query.body.as_ref() {
            SetExpr::Select(select) => grouped_expressions(&select.group_by),
            _ => Vec::new(),
        };

        if let SetExpr::Select(select) = query.body.as_mut() {
            self.changed |= relax_mysql_group_by_projection(select);
            integerize_boolean_projections(select);
        }
        normalize_mysql_cast_targets(query);

        // `ORDER BY` gets the same treatment as the projection: MySQL lets a grouped
        // query sort by an ungrouped column (taking an arbitrary value per group),
        // PostgreSQL does not.
        if !group_exprs.is_empty() {
            let output_names = match query.body.as_ref() {
                SetExpr::Select(select) => select_output_names(select),
                _ => Vec::new(),
            };
            if let Some(order_by) = query.order_by.as_mut() {
                if let OrderByKind::Expressions(order_exprs) = &mut order_by.kind {
                    for order_expr in order_exprs {
                        // A bare reference to a SELECT-list alias is already legal in
                        // PostgreSQL — it names an output column, not a source column,
                        // so aggregating it here would break the reference.
                        if references_output_name(&order_expr.expr, &output_names) {
                            continue;
                        }
                        if group_exprs.contains(&order_expr.expr)
                            || !needs_group_by_relaxation(&order_expr.expr, &group_exprs)
                        {
                            continue;
                        }
                        order_expr.expr = wrap_in_min(order_expr.expr.clone());
                        self.changed = true;
                    }
                }
            }
        }

        ControlFlow::Continue(())
    }
}


/// Maps MySQL's `CAST` target types onto PostgreSQL ones.
///
/// MySQL casts to pseudo-types PostgreSQL does not have — `SIGNED`, `UNSIGNED` — and
/// to `CHAR`, which in PostgreSQL means `character(1)` and would silently truncate
/// the value to a single character.
fn normalize_mysql_cast_targets(query: &mut Query) {
    let mut normalizer = CastTargetNormalizer;
    let _: ControlFlow<()> = VisitMut::visit(query, &mut normalizer);
}

struct CastTargetNormalizer;

impl VisitorMut for CastTargetNormalizer {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::Cast { data_type, .. } = expr {
            match data_type {
                DataType::Signed | DataType::SignedInteger => *data_type = DataType::BigInt(None),
                DataType::Unsigned | DataType::UnsignedInteger => *data_type = DataType::BigInt(None),
                DataType::Char(_) => *data_type = DataType::Text,
                _ => {}
            }
        }
        // Convert MySQL's IF() here rather than leaving it to the later text pass:
        // branch-type unification below only understands CASE, and a CASE produced
        // after this stage would never be checked.
        convert_mysql_if_to_case(expr);
        unify_mysql_case_branch_types(expr);
        ControlFlow::Continue(())
    }
}



/// Rewrites MySQL's `IF(condition, then, else)` into a standard `CASE` expression.
fn convert_mysql_if_to_case(expr: &mut Expr) {
    let Expr::Function(function) = expr else {
        return;
    };
    let is_if = function
        .name
        .0
        .last()
        .and_then(|part| part.as_ident())
        .map(|ident| ident.value.eq_ignore_ascii_case("IF"))
        .unwrap_or(false);
    if !is_if || function.over.is_some() {
        return;
    }
    let FunctionArguments::List(list) = &function.args else {
        return;
    };
    if list.args.len() != 3 {
        return;
    }
    let mut parts = Vec::with_capacity(3);
    for arg in &list.args {
        match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(inner)) => parts.push(inner.clone()),
            _ => return,
        }
    }
    let mut parts = parts.into_iter();
    let (condition, then_result, else_result) = (
        parts.next().expect("three arguments"),
        parts.next().expect("three arguments"),
        parts.next().expect("three arguments"),
    );

    *expr = Expr::Case {
        case_token: AttachedToken::empty(),
        end_token: AttachedToken::empty(),
        operand: None,
        conditions: vec![CaseWhen { condition, result: then_result }],
        else_result: Some(Box::new(else_result)),
    };
}

/// Makes a `CASE`'s branches agree on a type the way MySQL would.
///
/// MySQL has no strict typing here: `CASE WHEN ... THEN -1 ELSE SUBSTRING(x) END`
/// yields a string, because a numeric and a string branch unify to string.
/// PostgreSQL rejects the mismatch outright ("CASE types text and integer cannot be
/// matched"). Casting the numeric branches to text reproduces MySQL's result — and
/// where the caller wraps the CASE in `CAST(... AS <number>)`, as MySQL code
/// typically does, the value converts back exactly.
///
/// This only fires when a branch is *recognisably* text — a string literal or a call
/// to a known string function — so a genuinely numeric CASE is left alone.
fn unify_mysql_case_branch_types(expr: &mut Expr) {
    let Expr::Case { conditions, else_result, .. } = expr else {
        return;
    };

    let mut results: Vec<&Expr> = conditions.iter().map(|when| &when.result).collect();
    if let Some(else_result) = else_result.as_deref() {
        results.push(else_result);
    }
    let has_text = results.iter().any(|result| is_text_typed_expression(result));
    let has_number = results.iter().any(|result| is_numeric_literal(result));
    if !has_text || !has_number {
        return;
    }

    for when in conditions.iter_mut() {
        if is_numeric_literal(&when.result) {
            when.result = cast_to_text(when.result.clone());
        }
    }
    if let Some(else_result) = else_result.as_deref_mut() {
        if is_numeric_literal(else_result) {
            *else_result = cast_to_text(else_result.clone());
        }
    }
}

fn cast_to_text(expr: Expr) -> Expr {
    Expr::Cast {
        kind: CastKind::Cast,
        expr: Box::new(expr),
        data_type: DataType::Text,
        array: false,
        format: None,
    }
}

fn is_numeric_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(value.value, Value::Number(_, _)),
        // `-1` parses as a unary minus over a number.
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } | Expr::UnaryOp { op: UnaryOperator::Plus, expr } => {
            is_numeric_literal(expr)
        }
        _ => false,
    }
}

/// Conservative: only expressions whose text type is unambiguous from their form.
fn is_text_typed_expression(expr: &Expr) -> bool {
    const TEXT_FUNCTIONS: &[&str] = &[
        "SUBSTRING", "SUBSTR", "CONCAT", "CONCAT_WS", "LEFT", "RIGHT", "TRIM", "LTRIM",
        "RTRIM", "LOWER", "UPPER", "REPLACE", "LPAD", "RPAD", "MD5", "SHA1", "SHA2", "HEX",
    ];
    match expr {
        Expr::Value(value) => matches!(value.value, Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)),
        Expr::Nested(inner) => is_text_typed_expression(inner),
        // sqlparser models a few string functions with dedicated variants rather
        // than as generic calls.
        Expr::Substring { .. } | Expr::Trim { .. } => true,
        Expr::Function(function) => function
            .name
            .0
            .last()
            .and_then(|part| part.as_ident())
            .map(|ident| TEXT_FUNCTIONS.iter().any(|name| ident.value.eq_ignore_ascii_case(name)))
            .unwrap_or(false),
        _ => false,
    }
}

/// MySQL has no boolean type: a comparison in a SELECT list yields `1`/`0`, and
/// callers aggregate those as numbers (Matomo's archiver does
/// `MAX(search_count) = 0 AS \`28\``, then `min(\`28\`)` in an outer query).
/// PostgreSQL yields a real `boolean`, which has no `min()`/`sum()`, so the outer
/// aggregate fails. Casting to int restores MySQL's shape — and unlike a CASE
/// wrapper it also preserves NULL, which is what MySQL returns for a comparison
/// against NULL.
fn integerize_boolean_projections(select: &mut Select) -> bool {
    let mut changed = false;
    for item in &mut select.projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) => expr,
            SelectItem::ExprWithAlias { expr, .. } => expr,
            _ => continue,
        };
        if !yields_boolean_in_postgres(expr) {
            continue;
        }
        *expr = Expr::Cast {
            kind: CastKind::Cast,
            expr: Box::new(expr.clone()),
            data_type: DataType::Int(None),
            array: false,
            format: None,
        };
        changed = true;
    }
    changed
}

fn yields_boolean_in_postgres(expr: &Expr) -> bool {
    match expr {
        Expr::BinaryOp { op, .. } => matches!(
            op,
            BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::And
                | BinaryOperator::Or
        ),
        Expr::IsNull(_) | Expr::IsNotNull(_) => true,
        Expr::UnaryOp { op: UnaryOperator::Not, .. } => true,
        _ => false,
    }
}

fn relax_mysql_group_by_projection(select: &mut Select) -> bool {
    let group_exprs = grouped_expressions(&select.group_by);
    if group_exprs.is_empty() {
        return false;
    }

    let mut changed = false;
    for item in &mut select.projection {
        // Anything already aggregated, windowed, or built from a subquery is left
        // alone — PostgreSQL accepts those ungrouped, and wrapping them would either
        // nest aggregates (an error) or change what the query means.
        match item {
            SelectItem::UnnamedExpr(expr) => {
                if group_exprs.contains(expr) || !needs_group_by_relaxation(expr, &group_exprs) {
                    continue;
                }
                if is_plain_column(expr) {
                    // MySQL callers (e.g. PHP code reading `$row['report']`) rely on
                    // the unaliased column keeping its original name; wrapping it in
                    // MIN(...) without an alias would rename it to `min`.
                    let implicit_alias = implicit_column_alias(expr);
                    *item = SelectItem::ExprWithAlias {
                        expr: wrap_in_min(expr.clone()),
                        alias: implicit_alias,
                    };
                } else {
                    *expr = wrap_in_min(expr.clone());
                }
                changed = true;
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                if alias_is_grouped(alias, &group_exprs)
                    || group_exprs.contains(expr)
                    || !needs_group_by_relaxation(expr, &group_exprs)
                {
                    continue;
                }
                *expr = wrap_in_min(expr.clone());
                changed = true;
            }
            _ => continue,
        }
    }
    changed
}

/// Whether an expression references a column that is neither grouped nor already
/// inside an aggregate — i.e. something MySQL allows but PostgreSQL rejects. Note
/// this walks the whole expression, so it also catches ungrouped columns buried
/// inside a `CASE`, which is how Matomo's ranking queries label their "others" row.
fn needs_group_by_relaxation(expr: &Expr, group_exprs: &[Expr]) -> bool {
    let mut inspector = GroupByInspector {
        group_exprs,
        has_aggregate_window_or_subquery: false,
        has_ungrouped_column: false,
    };
    let _: ControlFlow<()> = Visit::visit(expr, &mut inspector);
    inspector.has_ungrouped_column && !inspector.has_aggregate_window_or_subquery
}

struct GroupByInspector<'a> {
    group_exprs: &'a [Expr],
    has_aggregate_window_or_subquery: bool,
    has_ungrouped_column: bool,
}

impl Visitor for GroupByInspector<'_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        match expr {
            Expr::Function(function) => {
                if function.over.is_some() || is_aggregate_function(&function.name) {
                    self.has_aggregate_window_or_subquery = true;
                }
            }
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                self.has_aggregate_window_or_subquery = true;
            }
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                if !self.group_exprs.contains(expr) {
                    self.has_ungrouped_column = true;
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

fn is_aggregate_function(name: &ObjectName) -> bool {
    const AGGREGATES: &[&str] = &[
        "COUNT", "SUM", "AVG", "MIN", "MAX", "GROUP_CONCAT", "STRING_AGG", "ARRAY_AGG",
        "BIT_AND", "BIT_OR", "BIT_XOR", "STD", "STDDEV", "STDDEV_POP", "STDDEV_SAMP",
        "VARIANCE", "VAR_POP", "VAR_SAMP", "JSON_ARRAYAGG", "JSON_OBJECTAGG", "BOOL_AND",
        "BOOL_OR", "EVERY",
    ];
    name.0
        .last()
        .and_then(|part| part.as_ident())
        .map(|ident| AGGREGATES.iter().any(|agg| ident.value.eq_ignore_ascii_case(agg)))
        .unwrap_or(false)
}

/// The columns a `GROUP BY` actually groups on, flattening `ROLLUP(...)`,
/// `CUBE(...)` and `GROUPING SETS(...)` so their arguments count as grouped —
/// otherwise every one of them looks ungrouped and gets wrapped in MIN().
fn grouped_expressions(group_by: &GroupByExpr) -> Vec<Expr> {
    let GroupByExpr::Expressions(exprs, _) = group_by else {
        return Vec::new();
    };
    let mut flattened = Vec::new();
    for expr in exprs {
        match expr {
            Expr::Function(function) if is_grouping_construct(&function.name) => {
                if let FunctionArguments::List(list) = &function.args {
                    for arg in &list.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(inner)) = arg {
                            flattened.push(inner.clone());
                        }
                    }
                }
            }
            other => flattened.push(other.clone()),
        }
    }
    flattened
}

fn is_grouping_construct(name: &ObjectName) -> bool {
    name.0
        .last()
        .and_then(|part| part.as_ident())
        .map(|ident| {
            ["ROLLUP", "CUBE", "GROUPING SETS", "GROUPING"]
                .iter()
                .any(|construct| ident.value.eq_ignore_ascii_case(construct))
        })
        .unwrap_or(false)
}

/// A projection aliased to a name the GROUP BY references is already grouped —
/// PostgreSQL resolves `GROUP BY action_name` against the SELECT-list alias.
fn alias_is_grouped(alias: &Ident, group_exprs: &[Expr]) -> bool {
    group_exprs.iter().any(|expr| match expr {
        Expr::Identifier(ident) => ident.value.eq_ignore_ascii_case(&alias.value),
        _ => false,
    })
}

fn is_plain_column(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
}

/// The names this SELECT exposes to an enclosing `ORDER BY`: explicit aliases, plus
/// the implicit name a bare column projects under.
fn select_output_names(select: &Select) -> Vec<String> {
    select
        .projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
            SelectItem::UnnamedExpr(expr) if is_plain_column(expr) => {
                Some(implicit_column_alias(expr).value)
            }
            _ => None,
        })
        .collect()
}

fn references_output_name(expr: &Expr, output_names: &[String]) -> bool {
    match expr {
        Expr::Identifier(ident) => output_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&ident.value)),
        _ => false,
    }
}

/// The column name a plain, unaliased column reference implicitly produces in its
/// result set (MySQL and PostgreSQL agree on this: the identifier itself, or the
/// last segment of a qualified `table.column` reference).
fn implicit_column_alias(expr: &Expr) -> Ident {
    match expr {
        Expr::Identifier(ident) => ident.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .cloned()
            .expect("compound identifier has at least one part"),
        _ => unreachable!("implicit_column_alias is only called for is_plain_column exprs"),
    }
}

fn wrap_in_min(expr: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName::from(vec![Ident::new("MIN")]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
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
                            ('secure_file_priv', NULL), \
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
    // `IGNORE` is optional: Matomo emits `INSERT IGNORE ... ON DUPLICATE KEY UPDATE`
    // when writing archive rows. In MySQL the ON DUPLICATE clause is what handles the
    // duplicate key, and IGNORE only downgrades *other* errors to warnings, so the
    // ON CONFLICT ... DO UPDATE below already expresses the intent.
    let pattern = Regex::new(
        r#"(?is)^\s*INSERT\s+(?:IGNORE\s+)?INTO\s+(?:`([A-Za-z_][A-Za-z0-9_]*)`|([A-Za-z_][A-Za-z0-9_]*))\s*\((.+?)\)\s*VALUES\s*\((.+?)\)\s*ON\s+DUPLICATE\s+KEY\s+UPDATE\s+(.+?)\s*;?\s*$"#,
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

    let conflict_target = infer_on_conflict_target(&columns, &update_assignments)?;
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

/// Guesses the conflict target for a translated `ON DUPLICATE KEY UPDATE`.
///
/// PostgreSQL requires `ON CONFLICT` to name a target, but which key a duplicate
/// would violate is a property of the schema, which the translator cannot see. The
/// guess here is a starting point only: if PostgreSQL rejects it, the executor reads
/// the table's real primary key and retries (`repair_on_conflict_target`). So prefer
/// returning a plausible target over failing the statement outright.
fn infer_on_conflict_target(
    columns: &[String],
    update_assignments: &[(String, String)],
) -> Result<Vec<String>, MiddlewareError> {
    if columns.len() == 1 {
        return Ok(vec![columns[0].clone()]);
    }

    // A column the UPDATE assigns cannot be the conflict key, so prefer the first
    // column that is only inserted.
    let untouched = columns.iter().find(|column| {
        update_assignments
            .iter()
            .all(|(assigned, _)| !assigned.eq_ignore_ascii_case(column))
    });
    if let Some(column) = untouched.or_else(|| columns.first()) {
        return Ok(vec![column.clone()]);
    }

    Err(MiddlewareError::Translation(
        "INSERT ... ON DUPLICATE KEY UPDATE requires at least one column".to_string(),
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

/// MySQL functions can nest inside one another; each rewrite pass only reaches the
/// outermost call, so passes repeat until none is left (bounded so a pathological
/// statement cannot loop).
const MAX_NESTED_FUNCTION_REWRITE_PASSES: usize = 8;

fn rewrite_mysql_functions(sql: &str, warnings: &mut Vec<String>) -> String {
    let mut out = sql.to_string();

    let replacements = [
        (r"(?i)\bIFNULL\s*\(", "COALESCE("),
        // PostgreSQL's CURRENT_TIMESTAMP is a bare keyword and rejects `()`, unlike
        // MySQL's NOW(); now() is PostgreSQL's actual callable equivalent.
        (r"(?i)\bNOW\s*\(", "now("),
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

    let (rewritten_instr, changed_instr) = rewrite_function_calls(&out, "INSTR", |args| {
        if args.len() != 2 {
            return None;
        }
        Some(format!(
            "POSITION(CAST({} AS text) IN CAST({} AS text))",
            args[1].trim(),
            args[0].trim()
        ))
    });
    if changed_instr {
        warnings.push("rewrote MySQL INSTR(str, substr) to PostgreSQL POSITION(substr IN str)".to_string());
        out = rewritten_instr;
    }

    let (rewritten_substring_index, changed_substring_index) =
        rewrite_function_calls(&out, "SUBSTRING_INDEX", |args| {
            if args.len() != 3 {
                return None;
            }
            let source = args[0].trim();
            let delimiter = args[1].trim();
            let count = args[2].trim();
            if let Some(abs_count) = count.strip_prefix('-') {
                return Some(format!(
                    "reverse(split_part(reverse(CAST({source} AS text)), reverse(CAST({delimiter} AS text)), {abs_count}))"
                ));
            }
            Some(format!(
                "split_part(CAST({source} AS text), CAST({delimiter} AS text), {count})"
            ))
        });
    if changed_substring_index {
        warnings.push("rewrote MySQL SUBSTRING_INDEX(str, delim, count) to PostgreSQL text operations".to_string());
        out = rewritten_substring_index;
    }

    // Applied repeatedly: each pass rewrites the outermost call, so a nested
    // `IF(a, IF(b, 1, 2), 3)` needs another pass to reach the inner one.
    let mut changed_if = false;
    for _ in 0..MAX_NESTED_FUNCTION_REWRITE_PASSES {
        let (rewritten, changed) = rewrite_function_calls(&out, "IF", |args| {
            if args.len() != 3 {
                return None;
            }
            Some(format!(
                "(CASE WHEN {} THEN {} ELSE {} END)",
                args[0].trim(),
                args[1].trim(),
                args[2].trim()
            ))
        });
        if !changed {
            break;
        }
        out = rewritten;
        changed_if = true;
    }
    if changed_if {
        warnings.push("rewrote MySQL IF(condition, then, else) to PostgreSQL CASE expression".to_string());
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

fn rewrite_function_calls<F>(sql: &str, function_name: &str, render: F) -> (String, bool)
where
    F: Fn(&[String]) -> Option<String>,
{
    let chars = sql.chars().collect::<Vec<_>>();
    let mut out = String::with_capacity(sql.len());
    let mut idx = 0usize;
    let mut changed = false;

    while idx < chars.len() {
        let ch = chars[idx];

        if matches!(ch, '\'' | '"' | '`') {
            copy_quoted_fragment(&chars, &mut idx, &mut out);
            continue;
        }

        if is_identifier_char(ch) {
            let token_start = idx;
            idx += 1;
            while idx < chars.len() && is_identifier_char(chars[idx]) {
                idx += 1;
            }

            let token = chars[token_start..idx].iter().collect::<String>();
            let mut open_idx = idx;
            while open_idx < chars.len() && chars[open_idx].is_whitespace() {
                open_idx += 1;
            }

            if token.eq_ignore_ascii_case(function_name)
                && open_idx < chars.len()
                && chars[open_idx] == '('
            {
                if let Some(close_idx) = find_matching_paren(&chars, open_idx) {
                    let args_sql = chars[open_idx + 1..close_idx].iter().collect::<String>();
                    if let Ok(args) = split_sql_csv(&args_sql) {
                        if let Some(replacement) = render(&args) {
                            out.push_str(&replacement);
                            idx = close_idx + 1;
                            changed = true;
                            continue;
                        }
                    }
                }
            }

            out.push_str(&token);
            continue;
        }

        out.push(ch);
        idx += 1;
    }

    (out, changed)
}

fn copy_quoted_fragment(chars: &[char], idx: &mut usize, out: &mut String) {
    let quote = chars[*idx];
    out.push(quote);
    *idx += 1;

    while *idx < chars.len() {
        let ch = chars[*idx];
        out.push(ch);
        *idx += 1;

        if ch == '\\' {
            if *idx < chars.len() {
                out.push(chars[*idx]);
                *idx += 1;
            }
            continue;
        }

        if ch == quote {
            if *idx < chars.len() && chars[*idx] == quote {
                out.push(chars[*idx]);
                *idx += 1;
                continue;
            }
            break;
        }
    }
}

fn find_matching_paren(chars: &[char], open_idx: usize) -> Option<usize> {
    let mut idx = open_idx + 1;
    let mut depth = 1usize;

    while idx < chars.len() {
        match chars[idx] {
            '\'' | '"' | '`' => {
                let mut sink = String::new();
                copy_quoted_fragment(chars, &mut idx, &mut sink);
            }
            '(' => {
                depth += 1;
                idx += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
                idx += 1;
            }
            _ => idx += 1,
        }
    }

    None
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
    fn rewrites_ranking_query_counters_to_window_functions() {
        let sql = "SELECT CASE WHEN counter = 5 THEN '__others__' ELSE `idaction` END AS `idaction`, sum(`hits`) AS `hits` \
                   FROM ( SELECT `idaction`, \
                   CASE \
                   WHEN `type` = 1 AND @counter1 = 5 THEN 5 \
                   WHEN `type` = 1 THEN @counter1:=@counter1+1 \
                   WHEN `type` = 2 AND @counter2 = 5 THEN 5 \
                   WHEN `type` = 2 THEN @counter2:=@counter2+1 \
                   ELSE 0 \
                   END AS counter, `hits`, `type` \
                   FROM ( SELECT @counter1:=0 ) initCounter1, ( SELECT @counter2:=0 ) initCounter2, \
                   ( SELECT idaction, type, count(*) AS hits FROM log GROUP BY idaction ORDER BY `hits` DESC ) actualQuery \
                   ) AS withCounter GROUP BY counter, `type`";
        let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

        assert!(!result.translated_sql.contains("@counter"));
        assert!(!result.translated_sql.contains("initCounter"));
        assert!(result.translated_sql.contains(
            "LEAST(ROW_NUMBER() OVER (PARTITION BY \"type\" ORDER BY \"hits\" DESC), 5)"
        ));
        // The overflow-label CASE must not mix a text literal with a numeric column.
        assert!(result.translated_sql.contains("CAST(\"idaction\" AS TEXT)"));
        assert!(result.warnings.iter().any(|w| w.contains("ranking-query")));
    }

    #[test]
    fn relaxes_order_by_on_ungrouped_column_but_not_output_aliases() {
        let result = translate_sql(
            "SELECT idaction, MIN(log_action.name) AS name, count(*) AS `12` FROM log \
             GROUP BY idaction ORDER BY `12` DESC, log_action.name ASC",
            &TranslatorConfig::default(),
        )
        .unwrap();
        // `12` is a SELECT-list alias: PostgreSQL resolves it, so it must be left bare.
        assert!(result.translated_sql.contains("ORDER BY \"12\" DESC"));
        assert!(!result.translated_sql.contains("MIN(\"12\")"));
        // `log_action.name` is a source column that is neither grouped nor aggregated.
        assert!(result.translated_sql.contains("MIN(log_action.name) ASC"));
    }

    #[test]
    fn window_order_by_uses_inner_output_alias_not_qualified_column() {
        // The window sits outside the subquery, where `log_action` is not in scope.
        let sql = "SELECT CASE WHEN counter = 5 THEN 'o' ELSE `idaction` END AS `idaction` \
                   FROM ( SELECT `idaction`, \
                   CASE WHEN `type` = 1 AND @counter1 = 5 THEN 5 \
                   WHEN `type` = 1 THEN @counter1:=@counter1+1 ELSE 0 END AS counter, `type` \
                   FROM ( SELECT @counter1:=0 ) initCounter1, \
                   ( SELECT idaction, MIN(log_action.name) AS name, count(*) AS `20` FROM log \
                     GROUP BY idaction ORDER BY `20` DESC, log_action.name ASC ) actualQuery \
                   ) AS withCounter GROUP BY counter, `type`";
        let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
        // The reference is quoted to match the subquery's (now quoted) alias.
        assert!(result.translated_sql.contains("ORDER BY \"20\" DESC, \"name\" ASC)"));
        assert!(!result.translated_sql.contains("OVER (PARTITION BY \"type\" ORDER BY \"20\" DESC, log_action.name"));
    }

    #[test]
    fn leaves_unrecognized_counter_shapes_untouched() {
        // If any `@counter` survives the rewrite the statement is left alone, so it
        // fails loudly instead of silently producing wrong rankings.
        let sql = "SELECT x FROM ( SELECT @counterRollup:=0 ) initCounterRollup, \
                   ( SELECT y, @counterRollup := @counterRollup + 99 FROM t ORDER BY y ) actualQuery";
        let (rewritten, changed) = rewrite_mysql_ranking_query(sql);
        assert!(!changed);
        assert_eq!(rewritten, sql);
    }

    #[test]
    fn drops_temporary_keyword_from_drop_table() {
        let result = translate_sql(
            "DROP TEMPORARY TABLE IF EXISTS matomo_logtmpsegment123",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert_eq!(
            result.translated_sql.trim(),
            "DROP TABLE IF EXISTS matomo_logtmpsegment123"
        );
    }

    #[test]
    fn preserves_alias_casing_so_clients_can_read_the_column_back() {
        // MySQL returns a column under the exact case of its alias; PostgreSQL folds
        // unquoted identifiers to lower case, so `$row['serverTimePretty']` misses.
        // Quoting the alias keeps the original spelling in the result set.
        let result = translate_sql(
            "SELECT log.server_time as serverTimePretty, count(*) as `2` FROM t GROUP BY x ORDER BY `2` DESC, `serverTimePretty`",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.contains("AS \"serverTimePretty\""));
        assert!(result.translated_sql.contains("ORDER BY \"2\" DESC, \"serverTimePretty\""));
    }

    #[test]
    fn rewrites_case_mismatched_alias_references() {
        // MySQL is case-insensitive, so an app may spell a reference differently from
        // the alias; PostgreSQL would not resolve it once the alias is quoted.
        let result = translate_sql(
            "SELECT log.server_time as serverTimePretty FROM t ORDER BY servertimepretty",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.contains("ORDER BY \"serverTimePretty\""));
    }

    #[test]
    fn does_not_rewrite_a_source_column_aliased_to_its_own_name_in_another_case() {
        // Matomo writes `idvisit AS idVisit`. Treating the projected `idvisit` as a
        // reference to the alias rewrites it to a column that does not exist.
        let result = translate_sql(
            "SELECT visit_first_action_time as serverTimePretty, idvisit AS idVisit FROM matomo_log_visit",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.contains("idvisit AS \"idVisit\""));
        assert!(!result.translated_sql.contains("\"idVisit\" AS \"idVisit\""));
    }

    #[test]
    fn leaves_plain_column_references_that_match_an_alias_alone() {
        // `visits` here is a source column, not a reference to the alias; rewriting it
        // would risk pointing at a differently-cased identifier.
        let result = translate_sql(
            "SELECT sum(visits) AS visits FROM t GROUP BY idsite",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.contains("sum(visits)"));
        assert!(result.translated_sql.contains("AS \"visits\""));
    }

    #[test]
    fn collapses_case_left_with_only_an_else_branch() {
        // The unpartitioned counter is often a CASE's only content; replacing it in
        // place would leave the invalid `CASE ELSE ... END`.
        let sql = "SELECT counter FROM ( SELECT \
                   CASE WHEN @counter = 7 THEN 7 ELSE @counter:=@counter+1 END AS counter, x \
                   FROM ( SELECT @counter:=0 ) initCounter, \
                   ( SELECT x, count(*) AS `2` FROM t GROUP BY x ORDER BY `2` DESC ) actualQuery \
                   ) AS withCounter GROUP BY counter";
        let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
        assert!(!result.translated_sql.to_uppercase().contains("CASE ELSE"));
        assert!(result
            .translated_sql
            .contains("LEAST(ROW_NUMBER() OVER (ORDER BY \"2\" DESC), 7) AS \"counter\""));
    }

    #[test]
    fn rewrites_with_rollup_to_standard_rollup() {
        let result = translate_sql(
            "SELECT a, b, count(*) AS c FROM t GROUP BY a, b WITH ROLLUP",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.contains("GROUP BY ROLLUP(a, b)"));
        assert!(result.warnings.iter().any(|w| w.contains("WITH ROLLUP")));
    }

    #[test]
    fn pairs_each_with_rollup_to_its_own_group_by() {
        // The outer GROUP BY has no ROLLUP; only the inner one must be rewritten.
        let result = translate_sql(
            "SELECT x FROM (SELECT a, b FROM t GROUP BY a, b WITH ROLLUP) s GROUP BY x",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.contains("GROUP BY ROLLUP(a, b)"));
        assert!(result.translated_sql.trim_end().ends_with("GROUP BY x"));
    }

    #[test]
    fn does_not_aggregate_columns_grouped_via_rollup() {
        // ROLLUP's arguments are grouped columns, so they must not be wrapped in MIN().
        let result = translate_sql(
            "SELECT a, count(*) AS c FROM t GROUP BY a WITH ROLLUP",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(!result.translated_sql.contains("MIN(a)"));
    }

    #[test]
    fn treats_group_by_alias_as_grouped() {
        let result = translate_sql(
            "SELECT COALESCE(name, '') AS action_name, count(*) AS c FROM t GROUP BY action_name",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(!result.translated_sql.contains("MIN(COALESCE"));
    }

    #[test]
    fn rewrites_rollup_counter_to_partitioned_row_number() {
        let sql = "SELECT counter, counterRollup FROM ( SELECT \
                   CASE WHEN `a` IS NULL THEN -1 WHEN `b` IS NULL THEN -1 \
                   WHEN @counter = 500 THEN 500 ELSE @counter:=@counter+1 END AS counter, \
                   CASE WHEN `a` IS NULL AND `b` IS NULL THEN -1 \
                   WHEN `a` IS NULL AND @counterRollup = 500 THEN 500 \
                   WHEN `a` IS NULL THEN @counterRollup := @counterRollup + 1 \
                   WHEN `b` IS NULL AND @counterRollup = 500 THEN 500 \
                   WHEN `b` IS NULL THEN @counterRollup := @counterRollup + 1 \
                   ELSE 0 END AS counterRollup \
                   FROM ( SELECT @counter:=0 ) initCounter, ( SELECT @counterRollup:=0 ) initCounterRollup, \
                   ( SELECT a, b, count(*) AS `2` FROM t GROUP BY a, b ORDER BY `2` DESC ) actualQuery \
                   ) AS withCounter GROUP BY counter, counterRollup";
        let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

        assert!(!result.translated_sql.contains("@counter"));
        // The rollup counter only advances on partial-rollup rows, so it is numbered
        // within its own row class.
        assert!(result.translated_sql.contains(
            "WHEN \"a\" IS NULL AND \"b\" IS NULL THEN 2 WHEN \"a\" IS NULL OR \"b\" IS NULL THEN 1 ELSE 0 END)"
        ));
        // The main counter skips rollup rows entirely, so it is numbered among the
        // detail rows only.
        assert!(result.translated_sql.contains(
            "PARTITION BY (CASE WHEN \"a\" IS NULL OR \"b\" IS NULL THEN 1 ELSE 0 END)"
        ));
    }

    #[test]
    fn casts_boolean_projections_to_int_like_mysql() {
        // MySQL returns 1/0 for a comparison, and callers aggregate it numerically.
        let result = translate_sql(
            "SELECT MAX(search_count) = 0 AS `28` FROM log_link_visit_action",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.contains("CAST(MAX(search_count) = 0 AS INT)"));
    }

    #[test]
    fn wraps_ungrouped_column_inside_case_expression() {
        let result = translate_sql(
            "SELECT CASE WHEN counter = 5 THEN 'others' ELSE name END AS name, counter FROM t GROUP BY counter",
            &TranslatorConfig::default(),
        )
        .unwrap();
        // `name` is ungrouped and not aggregated, so the whole CASE gets a MIN().
        assert!(result.translated_sql.contains("MIN(CASE"));
    }

    #[test]
    fn does_not_wrap_expressions_that_already_aggregate() {
        let result = translate_sql(
            "SELECT idsite, sum(visits) AS visits FROM t GROUP BY idsite",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(!result.translated_sql.contains("MIN(sum"));
        assert!(result.translated_sql.contains("sum(visits)"));
    }

    #[test]
    fn inlines_session_variables_used_within_one_expression() {
        // `(@v := expr)` names a sub-expression for reuse later in the same statement.
        // It carries no state between rows, so substituting the expression preserves
        // the meaning exactly.
        let result = translate_sql(
            "SELECT name FROM t ORDER BY CAST((CASE WHEN (@idsub := SUBSTRING(name, 20)) = '' THEN -1 ELSE @idsub END) AS SIGNED)",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(!result.translated_sql.contains("@idsub"));
        assert!(!result.translated_sql.contains(":="));
        assert_eq!(result.translated_sql.matches("SUBSTRING(name, 20)").count(), 2);
        assert!(result.warnings.iter().any(|w| w.contains("session-variable")));
    }

    #[test]
    fn leaves_self_referential_counters_to_the_ranking_rewrite() {
        // An accumulator is not a named sub-expression; inlining it would be wrong.
        let (out, changed) = inline_session_variable_expressions(
            "SELECT (@counter := @counter + 1) FROM t",
        );
        assert!(!changed);
        assert!(out.contains("@counter := @counter + 1"));
    }

    #[test]
    fn maps_mysql_cast_targets_onto_postgres_types() {
        for (mysql_target, expected) in [("SIGNED", "BIGINT"), ("UNSIGNED", "BIGINT"), ("CHAR", "TEXT")] {
            let result = translate_sql(
                &format!("SELECT CAST(x AS {mysql_target}) FROM t"),
                &TranslatorConfig::default(),
            )
            .unwrap();
            assert!(
                result.translated_sql.contains(&format!("AS {expected}")),
                "CAST(x AS {mysql_target}) should become {expected}, got {}",
                result.translated_sql
            );
        }
    }

    #[test]
    fn rewrites_nested_if_calls_at_every_depth() {
        let result = translate_sql(
            "SELECT IF(a, IF(b, IF(c, 1, 2), 3), 4) FROM t",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(!result.translated_sql.contains("IF("));
        assert_eq!(result.translated_sql.matches("CASE WHEN").count(), 3);
    }

    #[test]
    fn unifies_case_branches_mixing_text_and_numbers() {
        // MySQL returns a string here; PostgreSQL refuses to match the branch types.
        let result = translate_sql(
            "SELECT CASE WHEN x = '' THEN -1 ELSE SUBSTRING(x, 2) END FROM t",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.contains("CAST(-1 AS TEXT)"));
    }

    #[test]
    fn unifies_branch_types_of_an_if_not_just_a_case() {
        // Matomo writes `CAST(IF(cond, -1, <text>) AS SIGNED)`. IF() has to become a
        // CASE before branch-type unification runs, or the mismatch reaches
        // PostgreSQL unchecked.
        let result = translate_sql(
            "SELECT CAST(IF(SUBSTRING(name, 2) = '', -1, SUBSTRING(name, 2)) AS SIGNED) FROM t",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(!result.translated_sql.contains("IF("));
        assert!(result.translated_sql.contains("CAST(-1 AS TEXT)"));
        assert!(result.translated_sql.contains("AS BIGINT"));
    }

    #[test]
    fn leaves_purely_numeric_case_branches_alone() {
        let result = translate_sql(
            "SELECT CASE WHEN x = 1 THEN -1 ELSE qty END FROM t",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(!result.translated_sql.contains("AS TEXT"));
    }

    #[test]
    fn strips_mariadb_max_statement_time_hint() {
        // The exact prefix Matomo's Piwik\Db\Schema\Mariadb class generates.
        let result = translate_sql(
            "SET STATEMENT max_statement_time=60 FOR SELECT * FROM matomo_log_visit WHERE idsite = 1",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.starts_with("SELECT * FROM"));
        assert!(!result.translated_sql.to_uppercase().contains("SET STATEMENT"));
        assert!(result.warnings.iter().any(|w| w.contains("max_statement_time")));
    }

    #[test]
    fn rewrites_now_to_a_form_postgres_accepts_with_parens() {
        // PostgreSQL's CURRENT_TIMESTAMP is a bare keyword: CURRENT_TIMESTAMP() is a
        // syntax error there, unlike MySQL's NOW(). now() is PostgreSQL's callable
        // equivalent and must be used instead.
        let result = translate_sql(
            "UPDATE matomo_archive_invalidations SET ts_started = NOW() WHERE idinvalidation = 1",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(result.translated_sql.contains("now()"));
        assert!(!result.translated_sql.contains("CURRENT_TIMESTAMP()"));
    }

    #[test]
    fn repeated_identical_sql_returns_the_same_translation_from_the_cache() {
        let cfg = TranslatorConfig::default();
        let sql = "SELECT IFNULL(name, 'x') FROM users WHERE id = 1";
        let first = translate_sql(sql, &cfg).unwrap();
        let second = translate_sql(sql, &cfg).unwrap();
        assert_eq!(first.translated_sql, second.translated_sql);
    }

    #[test]
    fn same_sql_with_different_config_is_translated_independently() {
        let mut rewriting = TranslatorConfig::default();
        rewriting.rewrite_mysql_functions = true;
        let mut passthrough = TranslatorConfig::default();
        passthrough.rewrite_mysql_functions = false;

        let sql = "SELECT IFNULL(name, 'x') FROM users";
        let rewritten = translate_sql(sql, &rewriting).unwrap();
        let untouched = translate_sql(sql, &passthrough).unwrap();

        assert!(rewritten.translated_sql.contains("COALESCE"));
        assert!(untouched.translated_sql.contains("IFNULL"));
    }

    #[test]
    fn wraps_ungrouped_plain_columns_to_satisfy_postgres_group_by() {
        let result = translate_sql(
            "SELECT idsite, date1, date2, period, name, report, COUNT(*) as `count` \
             FROM matomo_archive_invalidations \
             WHERE idsite IN (1) AND name LIKE 'done%' \
             GROUP BY idsite, date1, date2, period, name",
            &TranslatorConfig::default(),
        )
        .unwrap();
        // The wrap must keep the column's original, unaliased output name (`report`)
        // so that callers reading the result row by that key (e.g. Matomo's PHP code)
        // still find it there instead of under PostgreSQL's default `min` label.
        assert!(result.translated_sql.contains("MIN(report) AS report"));
        assert!(!result.translated_sql.contains("MIN(idsite)"));
        assert!(result.translated_sql.contains("COUNT(*)"));
        assert!(!result.translated_sql.contains("MIN(COUNT(*))"));
    }

    #[test]
    fn does_not_touch_group_by_all_grouped_columns() {
        let result = translate_sql(
            "SELECT idsite, COUNT(*) FROM matomo_log_visit GROUP BY idsite",
            &TranslatorConfig::default(),
        )
        .unwrap();
        assert!(!result.translated_sql.contains("MIN("));
    }

    #[test]
    fn leaves_string_literals_unchanged_when_normalizing_booleans() {
        let result = translate_sql("SELECT true, false, 'true', \"false_value\" FROM flags", &TranslatorConfig::default()).unwrap();
        assert!(result.translated_sql.contains("SELECT TRUE, FALSE, 'true', 'false_value' FROM flags"));
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
