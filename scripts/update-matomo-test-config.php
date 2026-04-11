<?php

$configPath = '/var/www/html/config/config.ini.php';
$config = file_exists($configPath) ? file_get_contents($configPath) : '';

$sections = <<<'INI'

[tests]
http_host = "matomo-tests-web"
remote_addr = "127.0.0.1"
request_uri = "/"
port = 80
enable_logging = 1

[database_tests]
host = "matomo-tests-middleware"
username = "anyuser"
password = "matomo"
dbname = matomo_tests
tables_prefix =
port = 3306
adapter = MYSQLI
type = InnoDB
schema = Mysql
charset = utf8mb4
collation = utf8mb4_general_ci
enable_ssl = 0
ssl_no_verify = 1
INI;

if (strpos($config, '[tests]') !== false) {
    $config = preg_replace('/\n\[tests\][\s\S]*$/', '', $config);
}

file_put_contents($configPath, rtrim($config) . PHP_EOL . $sections . PHP_EOL);
