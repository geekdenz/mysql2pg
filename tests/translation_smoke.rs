use mysql2pg_middleware::{config::TranslatorConfig, translator::translate_sql};

#[test]
fn select_translation_smoke() {
    let sql = "SELECT `id`, IFNULL(`name`, 'n/a') FROM `users` LIMIT 0, 5";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("\"id\""));
    assert!(result.translated_sql.contains("COALESCE"));
    assert!(result.translated_sql.contains("LIMIT 5 OFFSET 0"));
}

#[test]
fn sql_no_cache_select_modifier_translation_smoke() {
    let sql = "SELECT SQL_NO_CACHE `value` FROM `matomo_locks` WHERE `key` = 'UsersManager.changePermissions' AND UNIX_TIMESTAMP() <= expiry_time LIMIT 1";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.starts_with("SELECT \"value\" FROM \"matomo_locks\""));
    assert!(result
        .translated_sql
        .contains("EXTRACT(EPOCH FROM CURRENT_TIMESTAMP) <= expiry_time"));
    assert!(!result.translated_sql.contains("SQL_NO_CACHE"));
}

#[test]
fn on_duplicate_key_update_translation_smoke() {
    let sql = "INSERT INTO user_language (login, language) VALUES (?, ?) ON DUPLICATE KEY UPDATE language = ?";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
    assert!(result
        .translated_sql
        .contains("INSERT INTO \"user_language\" (\"login\", \"language\") VALUES (?, ?) ON CONFLICT (\"login\") DO UPDATE SET \"language\" = ?"));
}

#[test]
fn insert_ignore_combined_with_on_duplicate_key_update_is_supported() {
    // Matomo writes archive rows with both clauses. In MySQL the ON DUPLICATE clause
    // handles the duplicate key and IGNORE only downgrades other errors, so the
    // upsert alone expresses the intent.
    let sql = "INSERT IGNORE INTO `matomo_archive_numeric_2026_09` (idarchive, idsite, name, value) \
               VALUES ('375','1','done.Goals','2') ON DUPLICATE KEY UPDATE value = '2'";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("INSERT INTO \"matomo_archive_numeric_2026_09\""));
    assert!(result.translated_sql.contains("DO UPDATE SET \"value\" = '2'"));
    assert!(!result.translated_sql.to_uppercase().contains("IGNORE"));
}

#[test]
fn insert_translation_does_not_invent_columns() {
    // The translator must not add columns the statement did not name — it cannot see
    // the schema, and guessing corrupts the statement for any application whose table
    // happens to match a hardcoded name. A NOT NULL column the INSERT omits is
    // supplied from the catalog at execution time instead (MySQL's implicit default).
    let sql = "INSERT INTO app_user_language (login, use_12_hour_clock) VALUES ('root','0') ON DUPLICATE KEY UPDATE use_12_hour_clock='0'";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains(
        "INSERT INTO \"app_user_language\" (\"login\", \"use_12_hour_clock\") VALUES ('root', '0')"
    ));
    assert!(!result.translated_sql.contains("language\") VALUES"));
    assert!(result
        .translated_sql
        .contains("DO UPDATE SET \"use_12_hour_clock\" = '0'"));
}

#[test]
fn on_duplicate_key_update_normalizes_mysql_escaped_string_literals() {
    let sql = r#"INSERT INTO matomo_session (id, modified, lifetime, data) VALUES ('abc', '1780099497', '1209600', 'a:1:{s:4:\"data\";s:5:\"hello\";}') ON DUPLICATE KEY UPDATE modified = '1780099497', lifetime = '1209600', data = 'a:1:{s:4:\"data\";s:5:\"hello\";}'"#;
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result
        .translated_sql
        .contains("'a:1:{s:4:\"data\";s:5:\"hello\";}'"));
    assert!(!result.translated_sql.contains(r#"\"data\""#));
    assert!(result
        .translated_sql
        .contains("ON CONFLICT (\"id\") DO UPDATE SET"));
}

#[test]
fn on_duplicate_key_update_keeps_escaped_literal_commas_in_one_value() {
    let sql = r#"INSERT INTO matomo_session (id, data) VALUES ('abc', 'can\'t, split') ON DUPLICATE KEY UPDATE data = 'can\'t, split'"#;
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("'can''t, split'"));
    assert!(result
        .translated_sql
        .contains("ON CONFLICT (\"id\") DO UPDATE SET"));
}

#[test]
fn insert_ignore_translation_smoke() {
    let sql = "INSERT IGNORE INTO `option` (option_name, option_value, autoload) VALUES ('a', 'b', 'c')";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
    assert!(result
        .translated_sql
        .contains("INSERT INTO \"option\" (option_name, option_value, autoload) VALUES ('a', 'b', 'c') ON CONFLICT DO NOTHING"));
}

#[test]
fn create_table_type_mappings_translation_smoke() {
    let sql = r#"
        CREATE TABLE metrics (
            id INT NOT NULL AUTO_INCREMENT,
            tiny_value TINYINT,
            medium_value MEDIUMINT,
            small_unsigned SMALLINT UNSIGNED,
            amount DECIMAL(12, 4) UNSIGNED,
            ratio FLOAT,
            score DOUBLE,
            payload JSON,
            flags SET('new', 'sale'),
            notes MEDIUMTEXT,
            raw_data LONGBLOB,
            happened_at DATETIME(6),
            touched_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
            PRIMARY KEY (id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
    "#;

    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("\"id\" INTEGER NOT NULL GENERATED BY DEFAULT AS IDENTITY"));
    assert!(result.translated_sql.contains("\"tiny_value\" SMALLINT"));
    assert!(result.translated_sql.contains("\"medium_value\" INTEGER"));
    assert!(result.translated_sql.contains("\"small_unsigned\" INTEGER"));
    assert!(result.translated_sql.contains("\"amount\" NUMERIC(12,4)"));
    assert!(result.translated_sql.contains("\"ratio\" REAL"));
    assert!(result.translated_sql.contains("\"score\" DOUBLE PRECISION"));
    assert!(result.translated_sql.contains("\"payload\" JSONB"));
    assert!(result.translated_sql.contains("\"flags\" TEXT"));
    assert!(result.translated_sql.contains("\"notes\" TEXT"));
    assert!(result.translated_sql.contains("\"raw_data\" BYTEA"));
    assert!(result.translated_sql.contains("\"happened_at\" TIMESTAMP(6)"));
    assert!(!result.translated_sql.contains("ON UPDATE CURRENT_TIMESTAMP"));
    assert!(!result.translated_sql.contains("ENGINE"));
}

#[test]
fn mysql_hex_binary_literal_uses_postgres_decode() {
    let result = translate_sql(
        "SELECT idvisit FROM matomo_log_visit WHERE config_id = X'2df086e629bb0ff4'",
        &TranslatorConfig::default(),
    )
    .unwrap();

    assert!(result
        .translated_sql
        .contains("config_id = decode('2df086e629bb0ff4', 'hex')"));
    assert!(!result.translated_sql.contains("X'2df086e629bb0ff4'"));
}

#[test]
fn show_tables_translation_smoke() {
    let result = translate_sql("SHOW TABLES LIKE 'ord%'", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("FROM information_schema.tables"));
    assert!(result.translated_sql.contains("table_schema = current_schema()"));
    assert!(result.translated_sql.contains("table_name LIKE 'ord%'"));
    assert!(result.translated_sql.contains("AS \"Tables_in_current_schema\""));
}

#[test]
fn describe_table_translation_smoke() {
    let result = translate_sql("DESC order_details", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("AS \"Field\""));
    assert!(result.translated_sql.contains("AS \"Type\""));
    assert!(result.translated_sql.contains("AS \"Null\""));
    assert!(result.translated_sql.contains("AS \"Key\""));
    assert!(result.translated_sql.contains("AS \"Default\""));
    assert!(result.translated_sql.contains("AS \"Extra\""));
    assert!(result.translated_sql.contains("WHERE c.table_schema = current_schema() AND c.table_name = 'order_details'"));
}

#[test]
fn show_databases_translation_smoke() {
    let result = translate_sql("SHOW DATABASES LIKE 'ap%'", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("FROM pg_database"));
    assert!(result.translated_sql.contains("datistemplate = FALSE"));
    assert!(result.translated_sql.contains("datname LIKE 'ap%'"));
    assert!(result.translated_sql.contains("AS \"Database\""));
}

#[test]
fn show_columns_translation_smoke() {
    let result = translate_sql("SHOW COLUMNS FROM order_details LIKE 'order%'", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("AS \"Field\""));
    assert!(result.translated_sql.contains("c.table_name = 'order_details'"));
    assert!(result.translated_sql.contains("c.column_name LIKE 'order%'"));
}

#[test]
fn show_create_table_translation_smoke() {
    let result = translate_sql("SHOW CREATE TABLE order_details", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("AS \"Create Table\""));
    assert!(result.translated_sql.contains("pg_constraint"));
    assert!(result.translated_sql.contains("CREATE TABLE %I"));
}

#[test]
fn show_variables_translation_smoke() {
    let result = translate_sql("SHOW VARIABLES LIKE 'version%'", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("\"Variable_name\""));
    assert!(result.translated_sql.contains("\"Value\""));
    assert!(result.translated_sql.contains("version_comment"));
    assert!(result.translated_sql.contains("\"Variable_name\" LIKE 'version%'"));
}

#[test]
fn mysql_system_variable_translation_smoke() {
    let result = translate_sql(
        "SELECT VERSION() AS version, CONCAT('[', @@sql_mode, ']') AS sql_mode, @@SESSION.version_comment AS comment",
        &TranslatorConfig::default(),
    )
    .unwrap();
    assert!(result.translated_sql.contains("'11.8.7-MariaDB-ubu2404'"));
    assert!(result.translated_sql.contains("'NO_AUTO_VALUE_ON_ZERO'"));
    assert!(result.translated_sql.contains("'MariaDB Server'"));
    assert!(!result.translated_sql.contains("@@sql_mode"));
}

#[test]
fn mysql_secure_file_priv_variable_translation_smoke() {
    let result = translate_sql("SELECT @@secure_file_priv", &TranslatorConfig::default()).unwrap();
    assert_eq!(result.translated_sql, "SELECT NULL::text");
}

#[test]
fn show_status_translation_smoke() {
    let result = translate_sql("SHOW STATUS LIKE 'Threads%'", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("pg_stat_activity"));
    assert!(result.translated_sql.contains("\"Variable_name\" LIKE 'Threads%'"));
}

#[test]
fn mysql_double_quoted_strings_are_translated_as_strings() {
    let result = translate_sql(
        "SELECT count(login) FROM `matomo_user` WHERE login <> \"anonymous\"",
        &TranslatorConfig::default(),
    )
    .unwrap();

    assert!(result
        .translated_sql
        .contains("WHERE login <> 'anonymous'"));
    assert!(!result.translated_sql.contains("\"anonymous\""));
}

#[test]
fn mysql_archive_invalidation_functions_are_translated() {
    let sql = "SELECT COUNT(*) as `count`, IF(INSTR(`name`, '.') > 0, SUBSTRING_INDEX(`name`, '.', -1), NULL) AS plugin, CHAR_LENGTH(IF(INSTR(`name`, '.') > 0, SUBSTRING_INDEX(`name`, '.', 1), `name`)) > 32 AS is_segment_archive FROM `matomo_archive_invalidations` GROUP BY plugin, is_segment_archive";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("CASE WHEN"));
    assert!(result.translated_sql.contains("POSITION(CAST('.' AS text) IN CAST(\"name\" AS text))"));
    assert!(result.translated_sql.contains("reverse(split_part(reverse(CAST(\"name\" AS text)), reverse(CAST('.' AS text)), 1))"));
    assert!(result.translated_sql.contains("split_part(CAST(\"name\" AS text), CAST('.' AS text), 1)"));
    assert!(!result.translated_sql.contains("INSTR("));
    assert!(!result.translated_sql.contains("SUBSTRING_INDEX("));
}

#[test]
fn show_collation_translation_smoke() {
    let result = translate_sql("SHOW COLLATION LIKE 'utf8%'", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("utf8mb4_unicode_ci"));
    assert!(result.translated_sql.contains("collation_rows(\"Collation\""));
    assert!(result.translated_sql.contains("\"Collation\" LIKE 'utf8%'"));
}

#[test]
fn show_charset_translation_smoke() {
    let result = translate_sql("SHOW CHARSET LIKE 'utf8%'", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("utf8mb4"));
    assert!(result.translated_sql.contains("AS charset_rows"));
    assert!(result.translated_sql.contains("\"Charset\" LIKE 'utf8%'"));
}

#[test]
fn show_index_where_translation_smoke() {
    let result = translate_sql(
        "SHOW INDEX FROM `log_visit` WHERE Key_name = ?",
        &TranslatorConfig::default(),
    )
    .unwrap();
    assert!(result.translated_sql.contains("FROM pg_class cls"));
    assert!(result.translated_sql.contains("cls.relname = 'log_visit'"));
    assert!(result.translated_sql.contains("WHERE \"Key_name\" = $1"));
    assert!(result.translated_sql.contains("AS \"Key_name\""));
}

#[test]
fn show_views_translation_smoke() {
    let result = translate_sql("SHOW VIEWS", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("FROM information_schema.views"));
    assert!(result.translated_sql.contains("AS \"Tables_in_current_schema\""));
}

#[test]
fn reserved_user_table_is_quoted_in_selects() {
    let result = translate_sql(
        "SELECT * FROM user WHERE login = ?",
        &TranslatorConfig::default(),
    )
    .unwrap();
    assert!(result.translated_sql.contains("FROM \"user\""));
    assert!(result.translated_sql.contains("WHERE login = ?"));
}

#[test]
fn create_table_with_inline_key_translation_smoke() {
    let sql = r#"
        CREATE TABLE piwik_test_table (
            id INT AUTO_INCREMENT,
            value INT,
            PRIMARY KEY (id),
            KEY index_value (value)
        )
    "#;
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("CREATE TABLE \"piwik_test_table\""));
    assert!(result.translated_sql.contains("GENERATED BY DEFAULT AS IDENTITY"));
    assert!(result.translated_sql.contains("CREATE INDEX \"piwik_test_table_index_value\" ON \"piwik_test_table\" (\"value\")"));
}

#[test]
fn alter_table_add_column_unsigned_translation_smoke() {
    let sql = "ALTER TABLE matomo_log_visit ADD COLUMN last_idlink_va BIGINT UNSIGNED DEFAULT NULL, ADD COLUMN custom_dimension_1 VARCHAR(255) DEFAULT NULL, ADD COLUMN custom_dimension_rank SMALLINT UNSIGNED DEFAULT 0;";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("ALTER TABLE \"matomo_log_visit\""));
    assert!(result
        .translated_sql
        .contains("ADD COLUMN \"last_idlink_va\" BIGINT DEFAULT NULL"));
    assert!(result
        .translated_sql
        .contains("ADD COLUMN \"custom_dimension_1\" VARCHAR(255) DEFAULT NULL"));
    assert!(result
        .translated_sql
        .contains("ADD COLUMN \"custom_dimension_rank\" INTEGER DEFAULT 0"));
    assert!(result
        .translated_sql
        .contains("ADD CHECK (custom_dimension_rank >= 0 AND custom_dimension_rank <= 65535)"));
    assert!(!result.translated_sql.contains("UNSIGNED"));
}

#[test]
fn alter_table_modify_column_unsigned_translation_smoke() {
    let sql = "ALTER TABLE `matomo_log_visit` MODIFY COLUMN `visitor_seconds_since_first` INT(11) UNSIGNED NULL, MODIFY COLUMN `visitor_count_visits` INT(11) UNSIGNED NOT NULL DEFAULT 0, MODIFY COLUMN `config_device_model` VARCHAR(100) CHARACTER SET utf8 COLLATE utf8_general_ci NULL DEFAULT NULL;";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("ALTER TABLE \"matomo_log_visit\""));
    assert!(result
        .translated_sql
        .contains("ALTER COLUMN \"visitor_seconds_since_first\" TYPE BIGINT"));
    assert!(result
        .translated_sql
        .contains("ALTER COLUMN \"visitor_seconds_since_first\" DROP NOT NULL"));
    assert!(result
        .translated_sql
        .contains("ALTER COLUMN \"visitor_count_visits\" SET NOT NULL"));
    assert!(result
        .translated_sql
        .contains("ALTER COLUMN \"visitor_count_visits\" SET DEFAULT 0"));
    assert!(result
        .translated_sql
        .contains("ADD CHECK (visitor_count_visits >= 0 AND visitor_count_visits <= 4294967295)"));
    assert!(result
        .translated_sql
        .contains("ALTER COLUMN \"config_device_model\" TYPE VARCHAR(100)"));
    assert!(!result.translated_sql.contains("UNSIGNED"));
    assert!(!result.translated_sql.contains("CHARACTER SET"));
    assert!(!result.translated_sql.contains("COLLATE"));
}

#[test]
fn alter_table_add_index_translation_smoke() {
    let sql = "ALTER TABLE `matomo_log_link_visit_action` ADD COLUMN `server_time` DATETIME NOT NULL, ADD INDEX index_idsite_servertime ( idsite, server_time ), ADD COLUMN `idpageview` CHAR(6) NULL DEFAULT NULL, ADD COLUMN `idaction_name` INTEGER(10) UNSIGNED, ADD COLUMN `time_dom_completion` MEDIUMINT(10) UNSIGNED NULL;";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result
        .translated_sql
        .contains("ALTER TABLE \"matomo_log_link_visit_action\" ADD COLUMN \"server_time\" TIMESTAMP NOT NULL"));
    assert!(result
        .translated_sql
        .contains("ADD COLUMN \"idpageview\" CHAR(6) NULL DEFAULT NULL"));
    assert!(result
        .translated_sql
        .contains("ADD COLUMN \"idaction_name\" BIGINT"));
    assert!(result
        .translated_sql
        .contains("ADD CHECK (\"idaction_name\" >= 0 AND \"idaction_name\" <= 4294967295)"));
    assert!(result
        .translated_sql
        .contains("ADD CHECK (\"time_dom_completion\" >= 0 AND \"time_dom_completion\" <= 16777215)"));
    assert!(result.translated_sql.contains(
        "CREATE INDEX \"matomo_log_link_visit_action_index_idsite_servertime\" ON \"matomo_log_link_visit_action\" (\"idsite\", \"server_time\")"
    ));
    assert!(!result.translated_sql.contains("ADD INDEX"));
    assert!(!result.translated_sql.contains("UNSIGNED"));
}

#[test]
fn alter_table_drop_index_translation_smoke() {
    let sql = "ALTER TABLE `matomo_log_link_visit_action` DROP INDEX index_idsite_servertime;";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert_eq!(
        result.translated_sql,
        "DROP INDEX \"matomo_log_link_visit_action_index_idsite_servertime\""
    );
    assert!(!result.translated_sql.contains("ALTER TABLE"));
}

#[test]
fn matomo_dimension_version_update_allows_mysql_type_text() {
    let sql = "UPDATE `matomo_option` SET option_value = 'INTEGER(10) UNSIGNED DEFAULT NULL', autoload = '1' WHERE option_name = 'version_log_link_visit_action.idaction_url'";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result
        .translated_sql
        .contains("\"matomo_option\" SET option_value = 'INTEGER(10) UNSIGNED DEFAULT NULL'"));
    assert!(result
        .translated_sql
        .contains("option_name = 'version_log_link_visit_action.idaction_url'"));
}

#[test]
fn create_table_with_desc_inline_key_translation_smoke() {
    let sql = r#"
        CREATE TABLE log_visit (
            idvisit BIGINT,
            idsite INT,
            config_id VARBINARY(16),
            visit_last_action_time DATETIME,
            idvisitor VARBINARY(8),
            KEY index_idsite_config_datetime (idsite, config_id, visit_last_action_time),
            KEY index_idsite_datetime (idsite, visit_last_action_time),
            KEY index_idsite_idvisitor_time (idsite, idvisitor, visit_last_action_time DESC)
        )
    "#;
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains(
        "CREATE INDEX \"log_visit_index_idsite_idvisitor_time\" ON \"log_visit\" (\"idsite\", \"idvisitor\", \"visit_last_action_time\" DESC)"
    ));
}

#[test]
fn create_table_with_unique_prefix_key_translation_smoke() {
    let sql = r#"
        CREATE TABLE log_profiling (
            query TEXT NOT NULL,
            count INTEGER UNSIGNED NULL,
            sum_time_ms FLOAT NULL,
            idprofiling BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
            PRIMARY KEY (idprofiling),
            UNIQUE KEY query(query(100))
        )
    "#;
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("CREATE TABLE \"log_profiling\""));
    assert!(result
        .translated_sql
        .contains("CREATE UNIQUE INDEX \"log_profiling_query\" ON \"log_profiling\" ((left(\"query\", 100)))"));
}

#[test]
fn create_temporary_table_translation_smoke() {
    let sql = r#"
        CREATE TEMPORARY TABLE piwik_test_table_temp (
            id INT AUTO_INCREMENT,
            PRIMARY KEY (id)
        )
    "#;
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("CREATE TEMPORARY TABLE \"piwik_test_table_temp\""));
    assert!(result.translated_sql.contains("GENERATED BY DEFAULT AS IDENTITY"));
}

#[test]
fn load_data_infile_translation_smoke() {
    let result = translate_sql(
        r#"LOAD DATA INFILE '/tmp/matomo.tsv' INTO TABLE matomo_log_visit FIELDS TERMINATED BY '\t' LINES TERMINATED BY '\n' (idvisit, idsite)"#,
        &TranslatorConfig::default(),
    )
    .unwrap();
    assert!(result.translated_sql.contains("COPY \"matomo_log_visit\" (\"idvisit\", \"idsite\") FROM '/tmp/matomo.tsv'"));
    assert!(result.translated_sql.contains("FORMAT csv"));
}

#[test]
fn local_load_data_has_explicit_wire_protocol_error() {
    let error = translate_sql(
        "LOAD DATA LOCAL INFILE '/tmp/matomo.tsv' INTO TABLE matomo_log_visit",
        &TranslatorConfig::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("client-local-infile wire support"));
}

fn ansi_quotes_config() -> TranslatorConfig {
    TranslatorConfig { ansi_quotes: true, ..TranslatorConfig::default() }
}

#[test]
fn ansi_quotes_mode_treats_double_quotes_as_identifiers() {
    // SilverStripe sets sql_mode = 'ANSI', after which "Title" names a column.
    let sql = r#"SELECT "Title" FROM "ProofRecord" WHERE "Active" = 1"#;
    let result = translate_sql(sql, &ansi_quotes_config()).unwrap();

    assert!(result.translated_sql.contains("\"Title\""));
    assert!(result.translated_sql.contains("FROM \"ProofRecord\""));
    // It must not have become a string literal.
    assert!(!result.translated_sql.contains("'Title'"));
}

#[test]
fn ansi_quotes_mode_still_parses_create_table() {
    let sql = r#"CREATE TABLE "ProofRecord" ("ID" int(11) not null auto_increment, "Title" varchar(255), primary key ("ID")) ENGINE=InnoDB"#;
    let result = translate_sql(sql, &ansi_quotes_config()).unwrap();

    assert!(result.translated_sql.contains("\"ProofRecord\""));
    assert!(result.translated_sql.contains("\"ID\""));
    assert!(!result.translated_sql.to_uppercase().contains("ENGINE"));
}

#[test]
fn ansi_quotes_mode_leaves_string_literals_alone() {
    let sql = r#"SELECT "Title" FROM "ProofRecord" WHERE "Title" = 'a "quoted" word'"#;
    let result = translate_sql(sql, &ansi_quotes_config()).unwrap();

    assert!(result.translated_sql.contains(r#"'a "quoted" word'"#));
}

#[test]
fn without_ansi_quotes_double_quotes_remain_string_literals() {
    // Default MySQL behaviour, which Matomo and most clients rely on.
    let sql = r#"SELECT "literal" AS v"#;
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("'literal'"));
}

#[test]
fn show_full_tables_with_where_filters_on_table_type() {
    // SilverStripe and phpMyAdmin both list base tables this way.
    let sql = "SHOW FULL TABLES WHERE Table_Type != 'VIEW'";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("information_schema.tables"));
    assert!(result.translated_sql.contains("Table_type"));
    // The predicate must survive, and resolve against a lowercase alias so that
    // PostgreSQL's identifier folding matches MySQL's mixed-case column name.
    // sqlparser renders `!=` as the standard `<>`.
    assert!(result.translated_sql.contains("Table_Type <> 'VIEW'"));
    assert!(result.translated_sql.contains("AS table_type"));
}

#[test]
fn show_tables_without_where_keeps_the_simple_form() {
    let result = translate_sql("SHOW TABLES", &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("information_schema.tables"));
    assert!(!result.translated_sql.contains("AS show_tables"));
}

#[test]
fn show_table_status_reports_innodb_and_a_utf8mb4_collation() {
    let result = translate_sql("SHOW TABLE STATUS LIKE 'SiteTree'", &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("pg_class"));
    assert!(result.translated_sql.contains("'InnoDB' AS \"Engine\""));
    assert!(result.translated_sql.contains("utf8mb4_unicode_ci"));
    assert!(result.translated_sql.contains("LIKE 'SiteTree'"));
}

#[test]
fn show_table_status_without_a_pattern_lists_every_table() {
    let result = translate_sql("SHOW TABLE STATUS", &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("pg_class"));
    assert!(!result.translated_sql.contains("LIKE"));
}

#[test]
fn show_indexes_accepts_in_as_well_as_from() {
    for sql in ["SHOW INDEX FROM `ProofRecord`", "SHOW KEYS IN ProofRecord"] {
        let result = translate_sql(sql, &TranslatorConfig::default())
            .unwrap_or_else(|e| panic!("{sql} failed: {e}"));
        assert!(result.translated_sql.contains("Key_name"), "{sql}");
        assert!(result.translated_sql.contains("ProofRecord"), "{sql}");
    }
}

#[test]
fn show_indexes_accepts_ansi_quoted_table_names() {
    // Only under ANSI_QUOTES is "ProofRecord" a table rather than a string.
    let result = translate_sql("SHOW INDEXES IN \"ProofRecord\"", &ansi_quotes_config()).unwrap();
    assert!(result.translated_sql.contains("Key_name"));
    assert!(result.translated_sql.contains("ProofRecord"));
}

#[test]
fn show_table_status_accepts_a_placeholder_pattern() {
    // Zend's mysqli adapter prepares this rather than inlining the pattern.
    let result = translate_sql("SHOW TABLE STATUS LIKE ?", &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("pg_class"));
    assert!(result.translated_sql.contains("c.relname LIKE $1"));
}

#[test]
fn schema_function_resolves_to_the_current_schema() {
    // Laravel's MySQL schema grammar probes for tables with schema().
    let sql = "select exists (select 1 from information_schema.tables where table_schema = schema() and table_name = 'migrations') as `exists`";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("current_schema()"));
    assert!(!result.translated_sql.to_lowercase().contains("schema()  and"));
    assert!(!result.translated_sql.contains("CURRENT_DATABASE"));
}

#[test]
fn named_constraints_are_quoted_exactly_once() {
    // A backticked constraint name must not end up as ""name"": rendering the
    // Ident keeps its backticks, which the backtick pass then turns into quotes.
    for sql in [
        "ALTER TABLE `role_user` ADD CONSTRAINT `fk_user` FOREIGN KEY (`user_id`) REFERENCES `users` (`id`) ON DELETE CASCADE",
        "ALTER TABLE `t` ADD CONSTRAINT `uq_t` UNIQUE (`a`)",
        "ALTER TABLE `t` ADD CONSTRAINT `pk_t` PRIMARY KEY (`a`)",
    ] {
        let result = translate_sql(sql, &TranslatorConfig::default())
            .unwrap_or_else(|e| panic!("{sql} failed: {e}"));
        assert!(!result.translated_sql.contains("\"\""), "{sql} -> {}", result.translated_sql);
        assert!(result.translated_sql.contains("CONSTRAINT \""), "{sql}");
    }
}

#[test]
fn adding_a_not_null_column_gets_mysqls_implicit_default() {
    // MySQL backfills existing rows; PostgreSQL would reject the statement.
    let varchar = translate_sql("ALTER TABLE `users` ADD `external_auth_id` varchar(191) NOT NULL", &TranslatorConfig::default()).unwrap();
    assert!(varchar.translated_sql.contains("DEFAULT ''"), "{}", varchar.translated_sql);

    let int = translate_sql("ALTER TABLE `users` ADD `hits` int NOT NULL", &TranslatorConfig::default()).unwrap();
    assert!(int.translated_sql.contains("DEFAULT 0"), "{}", int.translated_sql);

    // An explicit default must be left alone.
    let explicit = translate_sql("ALTER TABLE `users` ADD `role` varchar(20) NOT NULL DEFAULT 'guest'", &TranslatorConfig::default()).unwrap();
    assert!(explicit.translated_sql.contains("DEFAULT 'guest'"));
    assert!(!explicit.translated_sql.contains("DEFAULT ''"));

    // Nullable columns need nothing.
    let nullable = translate_sql("ALTER TABLE `users` ADD `note` varchar(50) NULL", &TranslatorConfig::default()).unwrap();
    assert!(!nullable.translated_sql.contains("DEFAULT"));

    // Dates have no safe equivalent, so they are left to fail loudly.
    let dated = translate_sql("ALTER TABLE `users` ADD `seen_at` datetime NOT NULL", &TranslatorConfig::default()).unwrap();
    assert!(!dated.translated_sql.contains("DEFAULT"));
}

#[test]
fn rename_table_becomes_alter_table_rename_to() {
    let one = translate_sql("RENAME TABLE `permissions` TO `role_permissions`", &TranslatorConfig::default()).unwrap();
    assert_eq!(one.translated_sql, "ALTER TABLE \"permissions\" RENAME TO \"role_permissions\"");

    // MySQL allows a list; each pair becomes its own statement.
    let many = translate_sql("RENAME TABLE `a` TO `b`, `c` TO `d`", &TranslatorConfig::default()).unwrap();
    assert_eq!(many.translated_sql, "ALTER TABLE \"a\" RENAME TO \"b\"; ALTER TABLE \"c\" RENAME TO \"d\"");
}

#[test]
fn group_concat_becomes_string_agg() {
    let plain = translate_sql("SELECT GROUP_CONCAT(name) FROM t", &TranslatorConfig::default()).unwrap();
    assert!(plain.translated_sql.contains("string_agg"), "{}", plain.translated_sql);
    assert!(plain.translated_sql.contains("','"));

    let ordered = translate_sql("SELECT GROUP_CONCAT(col ORDER BY seq) FROM t", &TranslatorConfig::default()).unwrap();
    assert!(ordered.translated_sql.contains("ORDER BY seq"), "{}", ordered.translated_sql);

    let separated = translate_sql("SELECT GROUP_CONCAT(col SEPARATOR '|') FROM t", &TranslatorConfig::default()).unwrap();
    assert!(separated.translated_sql.contains("'|'"), "{}", separated.translated_sql);
}

#[test]
fn information_schema_statistics_is_served_from_the_catalog() {
    // Laravel's schema builder reads a table's indexes from this MySQL-only view.
    let sql = "select index_name as `name` from information_schema.statistics where table_schema = schema() and table_name = 'pages'";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(!result.translated_sql.contains("information_schema.statistics"));
    assert!(result.translated_sql.contains("pg_index"));
    assert!(result.translated_sql.contains(") AS statistics"));
    // A primary key is reported under MySQL's name, not PostgreSQL's.
    assert!(result.translated_sql.contains("'PRIMARY'"));
}

#[test]
fn update_set_targets_lose_their_table_qualifier() {
    // Valid MySQL; PostgreSQL rejects a qualified SET target.
    let sql = "UPDATE pages SET pages.revision_count=(SELECT count(*) FROM page_revisions WHERE page_revisions.page_id=pages.id)";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("SET revision_count ="), "{}", result.translated_sql);
    // The qualifier must survive everywhere it is still legal.
    assert!(result.translated_sql.contains("page_revisions.page_id"));
    assert!(result.translated_sql.contains("pages.id"));
}

#[test]
fn drop_primary_key_targets_postgres_default_constraint_name() {
    let result = translate_sql("ALTER TABLE `joint_permissions` DROP PRIMARY KEY", &TranslatorConfig::default()).unwrap();
    assert_eq!(
        result.translated_sql,
        "ALTER TABLE \"joint_permissions\" DROP CONSTRAINT \"joint_permissions_pkey\""
    );
    // No IF EXISTS: a differently named key must fail rather than be ignored.
    assert!(!result.translated_sql.contains("IF EXISTS"));
}

#[test]
fn insert_select_gets_the_select_compatibility_rewrites() {
    // MySQL's EXISTS yields 1/0, which it will happily store in a tinyint column.
    // PostgreSQL yields a boolean and refuses to assign it to an integer column.
    let sql = "insert into t (a, v) select id, (select exists(select 1 from u where u.id = t.id)) as v from t";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
    assert!(result.translated_sql.contains("CAST(EXISTS"), "{}", result.translated_sql);
}

#[test]
fn boolean_yielding_expressions_are_integerized() {
    for (sql, needle) in [
        ("SELECT a LIKE 'x%' FROM t", "CAST"),
        ("SELECT a IN (1, 2) FROM t", "CAST"),
        ("SELECT a BETWEEN 1 AND 2 FROM t", "CAST"),
        ("SELECT EXISTS(SELECT 1) FROM t", "CAST"),
    ] {
        let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();
        assert!(result.translated_sql.contains(needle), "{sql} -> {}", result.translated_sql);
    }
}

#[test]
fn information_schema_columns_gains_mysqls_extra_columns() {
    // Laravel reconstructs a column's declared type from column_type and extra,
    // neither of which PostgreSQL's information_schema provides.
    let sql = "select column_name as `name`, column_type as `type`, extra as `extra` from information_schema.columns where table_name = 'x'";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains("AS column_type"));
    assert!(result.translated_sql.contains("AS extra"));
    assert!(result.translated_sql.contains(") AS columns"));
    // The standard columns still come through.
    assert!(result.translated_sql.contains("c.*"));
}

#[test]
fn information_schema_rewrites_preserve_an_existing_alias() {
    // The middleware's own SHOW COLUMNS translation aliases the table as `c`;
    // a derived table takes exactly one alias, so the original must be kept.
    let sql = "SELECT c.column_name FROM information_schema.columns c WHERE c.table_name = 'x'";
    let result = translate_sql(sql, &TranslatorConfig::default()).unwrap();

    assert!(result.translated_sql.contains(") AS c"), "{}", result.translated_sql);
    assert!(!result.translated_sql.contains("AS columns c"));
}

#[test]
fn drop_foreign_key_becomes_drop_constraint() {
    let result = translate_sql("ALTER TABLE `books` DROP FOREIGN KEY `books_sort_rule_id_foreign`", &TranslatorConfig::default()).unwrap();
    assert_eq!(
        result.translated_sql,
        "ALTER TABLE \"books\" DROP CONSTRAINT \"books_sort_rule_id_foreign\""
    );
}
