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
fn unsupported_mysql_constructs_fail_fast() {
    let sql = "INSERT INTO users(id, name) VALUES (1, 'a') ON DUPLICATE KEY UPDATE name='b'";
    let err = translate_sql(sql, &TranslatorConfig::default()).unwrap_err();
    assert!(err.to_string().contains("ON DUPLICATE KEY UPDATE"));
}
