<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

$steps = [
    ['sql' => 'SELECT DATABASE()', 'types' => '', 'params' => []],
    ['sql' => 'SELECT option_value, option_name FROM `matomo_option` WHERE autoload = 1', 'types' => '', 'params' => []],
    ['sql' => 'SELECT option_value FROM `matomo_option` WHERE option_name = ?', 'types' => 's', 'params' => ['TestingIfDatabaseConnectionWorked']],
    ['sql' => 'SELECT option_value FROM `matomo_option` WHERE option_name = ?', 'types' => 's', 'params' => ['version_core']],
    ['sql' => 'UPDATE `matomo_option` SET option_value = ?, autoload = ? WHERE option_name = ?', 'types' => 'sss', 'params' => ['5.8.0', '1', 'version_core']],
    ['sql' => 'SELECT option_value FROM `matomo_option` WHERE option_name = ?', 'types' => 's', 'params' => ['enableBrowserTriggerArchiving']],
    ['sql' => 'SELECT option_value FROM `matomo_option` WHERE option_name = ?', 'types' => 's', 'params' => ['lastTrackerCronRun']],
    ['sql' => 'SELECT option_name, option_value FROM `matomo_option` WHERE option_name LIKE ?', 'types' => 's', 'params' => ['Tracker%']],
    ['sql' => 'SHOW COLUMNS FROM `matomo_log_visit`', 'types' => '', 'params' => []],
];

foreach ($steps as $idx => $step) {
    $num = $idx + 1;
    echo "STEP={$num} SQL={$step['sql']}\n";
    $stmt = $db->prepare($step['sql']);
    if (!$stmt) {
        echo "PREPARE_FAIL errno={$db->errno} err={$db->error}\n";
        exit(10 + $idx);
    }

    if ($step['types'] !== '') {
        $bindArgs = [$step['types']];
        foreach ($step['params'] as $i => $value) {
            $bindArgs[] = &$step['params'][$i];
        }
        if (!call_user_func_array([$stmt, 'bind_param'], $bindArgs)) {
            echo "BIND_FAIL stmt_err={$stmt->error}\n";
            exit(20 + $idx);
        }
    }

    if (!$stmt->execute()) {
        echo "EXECUTE_FAIL stmt_err={$stmt->error}\n";
        exit(30 + $idx);
    }

    $result = $stmt->get_result();
    if ($result !== false) {
        $rows = 0;
        while ($result->fetch_assoc()) {
            $rows++;
        }
        echo "ROWS={$rows}\n";
    } else {
        echo "NO_RESULT stmt_err={$stmt->error}\n";
    }
}
