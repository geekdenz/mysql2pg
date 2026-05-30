<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('matomo-tests-middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

$steps = [
    ['type' => 'query', 'sql' => "SET NAMES 'utf8mb4' COLLATE 'utf8mb4_general_ci'"],
    ['type' => 'query', 'sql' => 'SET sql_mode = "NO_AUTO_VALUE_ON_ZERO"'],
    ['type' => 'query', 'sql' => 'DROP DATABASE IF EXISTS `latest_stable`'],
    ['type' => 'prepare', 'sql' => "SHOW CHARACTER SET LIKE 'utf8mb4'"],
    ['type' => 'prepare_bind', 'sql' => 'SHOW CHARACTER SET WHERE `Charset` = ?', 'types' => 's', 'params' => ['utf8mb4']],
    ['type' => 'query', 'sql' => 'CREATE DATABASE IF NOT EXISTS `latest_stable` DEFAULT CHARACTER SET utf8'],
    ['type' => 'query', 'sql' => "SET NAMES 'utf8mb4' COLLATE 'utf8mb4_general_ci'"],
    ['type' => 'query', 'sql' => 'SET sql_mode = "NO_AUTO_VALUE_ON_ZERO"'],
    ['type' => 'prepare', 'sql' => "SHOW TABLES LIKE '%'"],
];

foreach ($steps as $index => $step) {
    echo "STEP=" . ($index + 1) . " TYPE={$step['type']} SQL={$step['sql']}\n";

    if ($step['type'] === 'query') {
        $ok = $db->query($step['sql']);
        if ($ok === false) {
            echo "QUERY_FAIL errno={$db->errno} err={$db->error}\n";
            exit(10 + $index);
        }
        echo "QUERY_OK\n";
        continue;
    }

    $stmt = $db->prepare($step['sql']);
    if (!$stmt) {
        echo "PREPARE_FAIL errno={$db->errno} err={$db->error}\n";
        exit(30 + $index);
    }

    if ($step['type'] === 'prepare_bind') {
        $bindArgs = [$step['types']];
        foreach ($step['params'] as $i => $value) {
            $bindArgs[] = &$step['params'][$i];
        }
        if (!call_user_func_array([$stmt, 'bind_param'], $bindArgs)) {
            echo "BIND_FAIL err={$stmt->error}\n";
            exit(40 + $index);
        }
    }

    if (!$stmt->execute()) {
        echo "EXECUTE_FAIL err={$stmt->error}\n";
        exit(50 + $index);
    }

    echo "EXECUTE_OK\n";
    $result = $stmt->get_result();
    if ($result !== false) {
        while ($row = $result->fetch_assoc()) {
            echo json_encode($row, JSON_UNESCAPED_SLASHES) . "\n";
        }
    }
}
