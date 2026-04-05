<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

$queries = [
    'SELECT option_value FROM `matomo_option` WHERE option_name = ?',
    'UPDATE `matomo_option` SET option_value = ?, autoload = ? WHERE option_name = ?',
    'SELECT option_name, option_value FROM `matomo_option` WHERE option_name LIKE ?',
];

foreach ($queries as $sql) {
    echo "SQL={$sql}\n";
    $stmt = $db->prepare($sql);
    if (!$stmt) {
        echo "PREPARE_FAIL errno={$db->errno} err={$db->error}\n";
        continue;
    }

    echo "PARAM_COUNT={$stmt->param_count}\n";
}
