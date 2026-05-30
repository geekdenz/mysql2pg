<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('matomo-tests-middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

function run_stmt(mysqli $db, string $label, string $sql, ?string $param = null): void
{
    echo "SQL_{$label}={$sql}\n";
    $stmt = $db->prepare($sql);
    if (!$stmt) {
        fwrite(STDERR, "PREPARE_{$label}_FAIL={$db->errno}:{$db->error}\n");
        exit(10);
    }
    echo "PREPARE_{$label}_OK\n";

    if ($param !== null) {
        if (!$stmt->bind_param('s', $param)) {
            fwrite(STDERR, "BIND_{$label}_FAIL={$stmt->errno}:{$stmt->error}\n");
            exit(11);
        }
        echo "BIND_{$label}_OK\n";
    }

    if (!$stmt->execute()) {
        fwrite(STDERR, "EXECUTE_{$label}_FAIL={$stmt->errno}:{$stmt->error}\n");
        exit(12);
    }
    echo "EXECUTE_{$label}_OK\n";

    $meta = $stmt->result_metadata();
    if ($meta === false) {
        fwrite(STDERR, "META_{$label}_FAIL={$stmt->errno}:{$stmt->error}\n");
        exit(13);
    }
    echo "META_{$label}_OK fields={$meta->field_count}\n";

    $result = $stmt->get_result();
    if ($result === false) {
        fwrite(STDERR, "GET_RESULT_{$label}_FAIL={$stmt->errno}:{$stmt->error}\n");
        exit(14);
    }
    echo "GET_RESULT_{$label}_OK rows={$result->num_rows}\n";
    while ($row = $result->fetch_assoc()) {
        echo "ROW_{$label}=" . json_encode($row, JSON_UNESCAPED_SLASHES) . "\n";
    }

    $stmt->close();
    echo "CLOSE_{$label}_OK\n";
}

run_stmt($db, 'ONE', "SHOW CHARACTER SET LIKE 'utf8mb4'");
run_stmt($db, 'TWO', "SHOW CHARACTER SET WHERE `Charset` = ?", 'utf8mb4');
run_stmt($db, 'THREE', "SHOW TABLES LIKE '%'");
