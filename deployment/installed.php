<?php
$file = '/var/www/html/config/config.ini.php';
$config = is_file($file) ? parse_ini_file($file, true) : [];
exit(
    !empty($config['General']['salt'])
    && empty($config['General']['installation_in_progress'])
    && empty($config['General']['installation_first_accessed'])
    ? 0 : 1
);
