<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('matomo-tests-middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

$stmt = $db->prepare("SHOW TABLES LIKE '%'");
if (!$stmt) {
    fwrite(STDERR, "PREPARE_FAIL={$db->errno}:{$db->error}\n");
    exit(3);
}

echo "PREPARE_OK\n";
if (!$stmt->execute()) {
    fwrite(STDERR, "EXECUTE_FAIL={$stmt->errno}:{$stmt->error}\n");
    exit(4);
}

echo "EXECUTE_OK\n";
$result = $stmt->get_result();
if ($result === false) {
    fwrite(STDERR, "GET_RESULT_FAIL={$stmt->errno}:{$stmt->error}\n");
    exit(5);
}

echo "ROWS={$result->num_rows}\n";
while ($row = $result->fetch_assoc()) {
    echo json_encode($row, JSON_UNESCAPED_SLASHES) . "\n";
}
