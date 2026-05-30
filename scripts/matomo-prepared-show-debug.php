<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('matomo-tests-middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

$queries = [
    "SHOW CHARACTER SET LIKE 'utf8mb4'",
    "SHOW CHARACTER SET WHERE `Charset` = ?",
    "SHOW TABLES LIKE '%'",
];

foreach ($queries as $sql) {
    echo "SQL={$sql}\n";
    $stmt = $db->prepare($sql);
    if (!$stmt) {
        echo "PREPARE_FAIL errno={$db->errno} err={$db->error}\n";
        continue;
    }

    if (strpos($sql, '?') !== false) {
        $value = 'utf8mb4';
        $stmt->bind_param('s', $value);
    }

    if (!$stmt->execute()) {
        echo "EXECUTE_FAIL errno={$stmt->errno} err={$stmt->error}\n";
        continue;
    }

    $meta = $stmt->result_metadata();
    if ($meta === false) {
        echo "META=false field_count={$stmt->field_count}\n";
    } else {
        echo "META field_count={$meta->field_count}\n";
        while ($field = $meta->fetch_field()) {
            echo "FIELD name={$field->name} type={$field->type}\n";
        }
    }

    $result = $stmt->get_result();
    if ($result === false) {
        echo "GET_RESULT=false errno={$stmt->errno} err={$stmt->error}\n";
    } else {
        echo "GET_RESULT rows={$result->num_rows} fields={$result->field_count}\n";
        while ($row = $result->fetch_assoc()) {
            $json = json_encode($row, JSON_UNESCAPED_SLASHES);
            echo "ROW_JSON=" . var_export($json, true) . "\n";
            echo "ROW_DUMP=" . var_export($row, true) . "\n";
        }
        if ($result->field_count > 0) {
            echo "FETCH_DONE errno={$stmt->errno} err={$stmt->error}\n";
        }
    }

    $stmt->close();
}
