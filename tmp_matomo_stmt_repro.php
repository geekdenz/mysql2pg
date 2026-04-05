<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

$steps = [
    ['sql' => 'SELECT DATABASE()', 'execute' => true, 'fetch_all' => true],
    ['sql' => 'SELECT option_value, option_name FROM `matomo_option` WHERE autoload = 1', 'execute' => true, 'fetch_all' => true],
    ['sql' => 'SELECT option_value, option_name FROM `matomo_option` WHERE autoload = 1', 'execute' => true, 'fetch_all' => true],
    ['sql' => "SHOW TABLES LIKE 'matomo\\_%'", 'execute' => true, 'fetch_all' => true],
    ['sql' => "SHOW TABLES LIKE 'matomo\\_archive_numeric%'", 'execute' => false, 'fetch_all' => false],
    ['sql' => 'SELECT option_value, option_name FROM `matomo_option` WHERE autoload = 1', 'execute' => false, 'fetch_all' => false],
];

foreach ($steps as $idx => $step) {
    $num = $idx + 1;
    echo "STEP={$num} PREPARE {$step['sql']}\n";
    $stmt = $db->prepare($step['sql']);
    if (!$stmt) {
        echo "PREPARE_FAIL errno={$db->errno} err={$db->error}\n";
        exit(10 + $idx);
    }

    if (!$step['execute']) {
        echo "PREPARE_OK\n";
        continue;
    }

    $ok = $stmt->execute();
    echo "EXECUTE=" . ($ok ? 'OK' : 'FAIL') . " stmt_err={$stmt->error}\n";
    if (!$ok) {
        continue;
    }

    $result = $stmt->get_result();
    if ($result === false) {
        echo "NO_RESULT stmt_err={$stmt->error}\n";
        continue;
    }

    if ($step['fetch_all']) {
        while ($row = $result->fetch_assoc()) {
            echo json_encode($row, JSON_UNESCAPED_SLASHES) . "\n";
        }
    }
}
