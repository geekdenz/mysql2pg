<?php
// Exercise PHP -> MySQL wire protocol -> PostgreSQL, as well as Apache.
try {
    $database = new mysqli(
        getenv('MATOMO_DATABASE_HOST'),
        getenv('MATOMO_DATABASE_USERNAME'),
        getenv('MATOMO_DATABASE_PASSWORD'),
        getenv('MATOMO_DATABASE_DBNAME')
    );
    $result = $database->query('SELECT 1')->fetch_row();
    if ((int) $result[0] !== 1) {
        throw new RuntimeException('Database health query failed');
    }
    $context = stream_context_create(['http' => ['timeout' => 5]]);
    $response = file_get_contents('http://127.0.0.1/matomo.js', false, $context);
    if ($response === false || strpos($response, 'Matomo') === false) {
        throw new RuntimeException('Apache tracker asset unavailable');
    }
} catch (Throwable $error) {
    // Do not put connection details in Docker health logs.
    fwrite(STDERR, "Matomo HTTP or middleware database health check failed\n");
    exit(1);
}
