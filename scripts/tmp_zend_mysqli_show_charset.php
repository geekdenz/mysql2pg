<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('matomo-tests-middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

$stmt = $db->prepare("SHOW CHARACTER SET LIKE 'utf8mb4'");
if (!$stmt) {
    fwrite(STDERR, "PREPARE_FAIL={$db->errno}:{$db->error}\n");
    exit(10);
}
echo "PREPARE_OK\n";

if (!$stmt->execute()) {
    fwrite(STDERR, "EXECUTE_FAIL={$stmt->errno}:{$stmt->error}\n");
    exit(11);
}
echo "EXECUTE_OK\n";

$meta = $stmt->result_metadata();
if ($meta === false) {
    fwrite(STDERR, "META_FAIL={$stmt->errno}:{$stmt->error}\n");
    exit(12);
}
echo "META_OK fields={$meta->field_count}\n";

$fields = $meta->fetch_fields();
$names = [];
foreach ($fields as $field) {
    $names[] = $field->name;
    echo "FIELD name={$field->name} type={$field->type}\n";
}

$stmt->store_result();
echo "STORE_RESULT_OK\n";

$values = array_fill(0, count($names), null);
$refs = [];
foreach ($values as $i => &$value) {
    $refs[$i] = &$value;
}
unset($value);

if (!call_user_func_array([$stmt, 'bind_result'], $refs)) {
    fwrite(STDERR, "BIND_RESULT_FAIL={$stmt->errno}:{$stmt->error}\n");
    exit(13);
}
echo "BIND_RESULT_OK\n";

$rowCount = 0;
while (true) {
    $status = $stmt->fetch();
    if ($status === null || $status === false) {
        break;
    }
    $row = [];
    foreach ($names as $i => $name) {
        $row[$name] = $values[$i];
    }
    echo "ROW=" . json_encode($row, JSON_UNESCAPED_SLASHES) . "\n";
    $rowCount++;
}
echo "FETCH_DONE rows={$rowCount}\n";

$stmt->free_result();
echo "FREE_RESULT_OK\n";

if (!$stmt->reset()) {
    fwrite(STDERR, "RESET_FAIL={$stmt->errno}:{$stmt->error}\n");
    exit(15);
}
echo "RESET_OK\n";

if (!$stmt->close()) {
    fwrite(STDERR, "CLOSE_FAIL={$stmt->errno}:{$stmt->error}\n");
    exit(16);
}
echo "CLOSE_OK\n";
