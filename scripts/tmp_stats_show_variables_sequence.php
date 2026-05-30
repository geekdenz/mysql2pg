<?php

mysqli_report(MYSQLI_REPORT_OFF);

$db = new mysqli('matomo-tests-middleware', 'anyuser', 'matomo', 'app', 3306);
if ($db->connect_errno) {
    fwrite(STDERR, "CONNECT_FAIL={$db->connect_errno}:{$db->connect_error}\n");
    exit(2);
}

function dump_stats(string $label): void
{
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
echo "PREPARE1=" . (bool)$stmt1 . "\n";
dump_stats('AFTER_PREPARE1');
$stmt1->execute();
$stmt1->result_metadata();
$res1 = $stmt1->get_result();
while ($res1->fetch_assoc()) {
}
dump_stats('AFTER_FETCH1');
$stmt1->close();
dump_stats('AFTER_CLOSE1');

$stmt2 = $db->prepare("SHOW VARIABLES LIKE 'character_set_database'");
echo "PREPARE2=" . (bool)$stmt2 . " errno={$db->errno} err={$db->error}\n";
dump_stats('AFTER_PREPARE2');
