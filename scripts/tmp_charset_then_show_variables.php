<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('matomo-tests-middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

function run_query(mysqli $db, string $sql): void
{
    echo "SQL={$sql}\n";
    $stmt = $db->prepare($sql);
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

    $meta = $stmt->result_metadata();
    if ($meta === false) {
        fwrite(STDERR, "META_FAIL={$stmt->errno}:{$stmt->error}\n");
        exit(5);
    }
    echo "META_OK fields={$meta->field_count}\n";
    while ($field = $meta->fetch_field()) {
        echo "FIELD name={$field->name} type={$field->type}\n";
    }

    $result = $stmt->get_result();
    if ($result === false) {
        fwrite(STDERR, "GET_RESULT_FAIL={$stmt->errno}:{$stmt->error}\n");
        exit(6);
    }
    echo "GET_RESULT_OK rows={$result->num_rows}\n";
    while ($row = $result->fetch_assoc()) {
        echo "ROW=" . json_encode($row, JSON_UNESCAPED_SLASHES) . "\n";
    }

    $stmt->close();
    echo "CLOSE_OK\n";
}

run_query($db, "SHOW CHARACTER SET LIKE 'utf8mb4'");
run_query($db, "SHOW VARIABLES LIKE 'character_set_database'");
