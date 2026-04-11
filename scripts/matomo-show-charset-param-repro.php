<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('matomo-tests-middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

$stmt = $db->prepare('SHOW CHARACTER SET WHERE `Charset` = ?');
if (!$stmt) {
    fwrite(STDERR, "PREPARE_FAIL={$db->errno}:{$db->error}\n");
    exit(3);
}

$charset = 'utf8mb4';
if (!$stmt->bind_param('s', $charset)) {
    fwrite(STDERR, "BIND_FAIL={$stmt->error}\n");
    exit(4);
}

echo "PREPARE_OK\n";
if (!$stmt->execute()) {
    fwrite(STDERR, "EXECUTE_FAIL={$stmt->error}\n");
    exit(5);
}

echo "EXECUTE_OK\n";
$result = $stmt->get_result();
if ($result === false) {
    fwrite(STDERR, "GET_RESULT_FAIL={$stmt->error}\n");
    exit(6);
}

while ($row = $result->fetch_assoc()) {
    echo json_encode($row, JSON_UNESCAPED_SLASHES) . "\n";
}
