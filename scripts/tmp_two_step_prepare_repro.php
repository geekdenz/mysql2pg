<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('matomo-tests-middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

$mode = $argv[1] ?? 'full';

echo "MODE={$mode}\n";

function dump_stats(string $label): void
{
    if (!function_exists('mysqli_get_client_stats')) {
        return;
    }
    $stats = mysqli_get_client_stats();
    $keys = [
        'bytes_received_ok_packet',
        'bytes_received_eof_packet',
        'bytes_received_prepare_response_packet',
        'packets_received_ok',
        'packets_received_eof',
        'packets_received_prepare_response',
        'packets_received_rset_header',
        'packets_received_rset_field_meta',
        'packets_received_rset_row',
        'proto_binary_fetched_string',
        'ps_buffered_sets',
        'rows_fetched_from_server_ps',
        'explicit_stmt_close',
        'com_stmt_prepare',
        'com_stmt_execute',
        'com_stmt_close',
    ];
    $filtered = [];
    foreach ($keys as $key) {
        $filtered[$key] = $stats[$key] ?? null;
    }
    echo "STATS_{$label}=" . json_encode($filtered, JSON_UNESCAPED_SLASHES) . "\n";
}

$stmt1 = $db->prepare("SHOW CHARACTER SET LIKE 'utf8mb4'");
if (!$stmt1) {
    fwrite(STDERR, "PREPARE1_FAIL={$db->errno}:{$db->error}\n");
    exit(3);
}
echo "PREPARE1_OK\n";
dump_stats('AFTER_PREPARE1');

if ($mode !== 'prepare_only') {
    if (!$stmt1->execute()) {
        fwrite(STDERR, "EXECUTE1_FAIL={$stmt1->errno}:{$stmt1->error}\n");
        exit(4);
    }
    echo "EXECUTE1_OK\n";
    dump_stats('AFTER_EXECUTE1');
}

if ($mode === 'meta' || $mode === 'full') {
    $meta = $stmt1->result_metadata();
    if ($meta === false) {
        fwrite(STDERR, "META1_FAIL={$stmt1->errno}:{$stmt1->error}\n");
        exit(5);
    }
    echo "META1_OK fields={$meta->field_count}\n";
    dump_stats('AFTER_META1');
}

if ($mode === 'full') {
    $result = $stmt1->get_result();
    if ($result === false) {
        fwrite(STDERR, "GET_RESULT1_FAIL={$stmt1->errno}:{$stmt1->error}\n");
        exit(6);
    }
    echo "GET_RESULT1_OK rows={$result->num_rows}\n";
    while ($row = $result->fetch_assoc()) {
        echo "ROW1=" . json_encode($row, JSON_UNESCAPED_SLASHES) . "\n";
    }
    dump_stats('AFTER_GET_RESULT1');
}

$stmt1->close();
echo "CLOSE1_OK\n";
dump_stats('AFTER_CLOSE1');

$stmt2 = $db->prepare("SHOW CHARACTER SET WHERE `Charset` = ?");
if (!$stmt2) {
    dump_stats('PREPARE2_FAIL');
    fwrite(STDERR, "PREPARE2_FAIL={$db->errno}:{$db->error}\n");
    exit(7);
}
echo "PREPARE2_OK\n";
dump_stats('AFTER_PREPARE2');

$value = 'utf8mb4';
if (!$stmt2->bind_param('s', $value)) {
    fwrite(STDERR, "BIND2_FAIL={$stmt2->errno}:{$stmt2->error}\n");
    exit(8);
}
echo "BIND2_OK\n";

if (!$stmt2->execute()) {
    dump_stats('EXECUTE2_FAIL');
    fwrite(STDERR, "EXECUTE2_FAIL={$stmt2->errno}:{$stmt2->error}\n");
    exit(9);
}
echo "EXECUTE2_OK\n";
dump_stats('AFTER_EXECUTE2');
