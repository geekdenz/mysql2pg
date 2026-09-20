<?php

namespace App\Task;

use SilverStripe\Dev\BuildTask;
use SilverStripe\ORM\DB;

/**
 * Reports which driver SilverStripe is actually using.
 *
 * This exists so the proof cannot be faked by configuration drift: it asks the
 * live connection what it is talking to, rather than trusting the .env.
 */
class DriverReportTask extends BuildTask
{
    private static $segment = 'mysql2pg-driver-report';

    protected $title = 'mysql2pg driver report';

    protected $description = 'Prints the live database class, connector and server version.';

    public function run($request)
    {
        $conn = DB::get_conn();

        $connector = 'unknown';
        if (method_exists($conn, 'getConnector')) {
            $connector = (new \ReflectionClass($conn->getConnector()))->getShortName();
        }

        $version = DB::query('SELECT VERSION()')->value();
        $mode = DB::query('SELECT @@sql_mode')->value();
        $ansi = (stripos((string) $mode, 'ANSI') !== false) ? 'yes' : 'no';

        printf(
            "DRIVER database=%s connector=%s version=%s sql_mode=%s ansi=%s\n",
            (new \ReflectionClass($conn))->getShortName(),
            $connector,
            $version,
            $mode,
            $ansi
        );
    }
}
