<?php
/**
 * Raw mysqli protocol checks against the middleware.
 *
 * SilverStripe's own suite exercises mysqli through an ORM, which hides which
 * protocol features are actually in use. This talks to the middleware directly
 * so a regression in, say, prepared-statement metadata is attributed clearly.
 *
 * Exits non-zero if any check fails.
 */
$host   = getenv('MW_HOST') ?: 'middleware';
$schema = getenv('MW_DB') ?: 'mysqli_probe';
$user   = getenv('MW_USER') ?: 'anyuser';
$pass   = getenv('MW_PASS') ?: 'matomo';

mysqli_report(MYSQLI_REPORT_OFF);
$failures = 0;

function check(string $name, callable $fn): void {
    global $failures;
    try {
        $detail = $fn();
        printf("  ok   %-42s %s\n", $name, $detail ?? '');
    } catch (\Throwable $e) {
        printf("  FAIL %-42s %s\n", $name, $e->getMessage());
        $failures++;
    }
}

$m = @new mysqli($host, $user, $pass, $schema, 3306);
if ($m->connect_error) {
    fwrite(STDERR, "cannot reach the middleware: {$m->connect_error}\n");
    exit(1);
}
printf("  connected: server=%s protocol=%d\n", $m->server_info, $m->protocol_version);

// The middleware maps a MySQL database onto a PostgreSQL schema.
$m->query(sprintf('CREATE DATABASE IF NOT EXISTS `%s`', $schema));
$m->query(sprintf('USE `%s`', $schema));

check('set_charset(utf8mb4)', fn() => $m->set_charset('utf8mb4') ? 'yes' : throw new Exception($m->error));

check('text protocol: SELECT', function () use ($m) {
    $r = $m->query('SELECT 1 AS one') ?: throw new Exception($m->error);
    return json_encode($r->fetch_assoc());
});

check('text protocol: DDL', function () use ($m) {
    $m->query('DROP TABLE IF EXISTS mysqli_probe') ?: throw new Exception($m->error);
    $m->query('CREATE TABLE mysqli_probe (
        id INT NOT NULL AUTO_INCREMENT,
        name VARCHAR(64),
        qty INT,
        price DECIMAL(10,2),
        PRIMARY KEY (id)
    )') ?: throw new Exception($m->error);
    return 'created';
});

check('insert_id and affected_rows', function () use ($m) {
    $m->query("INSERT INTO mysqli_probe (name, qty, price) VALUES ('widget', 5, 9.99)")
        ?: throw new Exception($m->error);
    return "insert_id={$m->insert_id} affected={$m->affected_rows}";
});

check('binary protocol: bind_param', function () use ($m) {
    $s = $m->prepare('INSERT INTO mysqli_probe (name, qty, price) VALUES (?, ?, ?)')
        ?: throw new Exception($m->error);
    $name = 'gadget'; $qty = 7; $price = '12.50';
    $s->bind_param('sis', $name, $qty, $price) ?: throw new Exception($s->error);
    $s->execute() ?: throw new Exception($s->error);
    $id = $s->insert_id;
    $s->close();
    return "insert_id=$id";
});

check('binary protocol: result_metadata + bind_result', function () use ($m) {
    $s = $m->prepare('SELECT id, name, qty, price FROM mysqli_probe WHERE qty > ? ORDER BY id')
        ?: throw new Exception($m->error);
    $min = 0;
    $s->bind_param('i', $min) ?: throw new Exception($s->error);
    $s->execute() ?: throw new Exception($s->error);
    $meta = $s->result_metadata() ?: throw new Exception('no result metadata: ' . $s->error);
    $cols = array_map(fn ($f) => $f->name, $meta->fetch_fields());
    $s->bind_result($id, $name, $qty, $price) ?: throw new Exception($s->error);
    $rows = [];
    while ($s->fetch()) {
        $rows[] = "$id:$name:$qty:$price";
    }
    $s->close();
    return 'cols=[' . implode(',', $cols) . '] rows=[' . implode(' ', $rows) . ']';
});

check('binary protocol: get_result', function () use ($m) {
    $s = $m->prepare('SELECT name, qty FROM mysqli_probe ORDER BY id') ?: throw new Exception($m->error);
    $s->execute() ?: throw new Exception($s->error);
    $r = $s->get_result() ?: throw new Exception('get_result failed: ' . $s->error);
    $out = [];
    while ($row = $r->fetch_assoc()) {
        $out[] = implode('/', $row);
    }
    $s->close();
    return implode(' | ', $out);
});

check('store_result and num_rows', function () use ($m) {
    $s = $m->prepare('SELECT id, name FROM mysqli_probe ORDER BY id') ?: throw new Exception($m->error);
    $s->execute() ?: throw new Exception($s->error);
    $s->store_result();
    $n = $s->num_rows;
    $s->close();
    return "num_rows=$n";
});

check('transaction commit', function () use ($m) {
    $m->begin_transaction();
    $m->query("UPDATE mysqli_probe SET qty = 42 WHERE name = 'widget'") ?: throw new Exception($m->error);
    $m->commit();
    $r = $m->query("SELECT qty FROM mysqli_probe WHERE name = 'widget'") ?: throw new Exception($m->error);
    return json_encode($r->fetch_row());
});

// Prepared SHOW statements: the shape Zend's mysqli adapter (and so Matomo) uses.
foreach ([
    'SHOW VARIABLES LIKE ?'     => 'max_allowed_packet',
    'SHOW TABLES LIKE ?'        => 'mysqli_probe',
    'SHOW CHARACTER SET LIKE ?' => 'utf8mb4',
    'SHOW TABLE STATUS LIKE ?'  => 'mysqli_probe',
] as $sql => $param) {
    check("prepared: $sql", function () use ($m, $sql, $param) {
        $s = $m->prepare($sql) ?: throw new Exception($m->error);
        $s->bind_param('s', $param) ?: throw new Exception($s->error);
        $s->execute() ?: throw new Exception($s->error);
        $r = $s->get_result() ?: throw new Exception('no result set: ' . $s->error);
        $rows = $r->num_rows;
        $first = $r->fetch_row();
        $s->close();
        if ($rows < 1) {
            throw new Exception('expected at least one row');
        }
        return "rows=$rows first=" . implode('|', array_slice($first, 0, 2));
    });
}

foreach (['SHOW FULL FIELDS IN `mysqli_probe`', 'SHOW INDEXES IN `mysqli_probe`'] as $sql) {
    check("prepared: $sql", function () use ($m, $sql) {
        $s = $m->prepare($sql) ?: throw new Exception($m->error);
        $s->execute() ?: throw new Exception($s->error);
        $r = $s->get_result() ?: throw new Exception('no result set: ' . $s->error);
        $n = $r->num_rows;
        $s->close();
        if ($n < 1) {
            throw new Exception('expected at least one row');
        }
        return "rows=$n";
    });
}

$m->close();
exit($failures === 0 ? 0 : 1);
