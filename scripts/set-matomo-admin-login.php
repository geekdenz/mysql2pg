<?php

use Piwik\Access;
use Piwik\Application\Environment;
use Piwik\Auth\Password;
use Piwik\Date;
use Piwik\Plugins\LanguagesManager\API as LanguagesManagerAPI;
use Piwik\Plugins\UsersManager\Model;
use Piwik\Plugins\UsersManager\UsersManager;

$login = $argv[1] ?? 'root';
$plainPassword = $argv[2] ?? 'ChangeMe123!';
$email = $argv[3] ?? 'root@example.test';

define('PIWIK_DOCUMENT_ROOT', getcwd());
define('PIWIK_INCLUDE_PATH', PIWIK_DOCUMENT_ROOT);

require_once PIWIK_INCLUDE_PATH . '/core/bootstrap.php';

$environment = new Environment(null);
$environment->init();

Access::getInstance()->setSuperUserAccess(true);

$passwordHelper = new Password();
$hashedPassword = $passwordHelper->hash(UsersManager::getPasswordHash($plainPassword));

$model = new Model();
$user = $model->getUser($login);

if (empty($user)) {
    $model->addUser($login, $hashedPassword, $email, Date::now()->getDatetime());
} else {
    $model->updateUser($login, $hashedPassword, $email);
}

$model->setSuperUserAccess($login, true);
LanguagesManagerAPI::getInstance()->setLanguageForUser($login, 'en');

$user = $model->getUser($login);
$passwordOk = $passwordHelper->verify(UsersManager::getPasswordHash($plainPassword), $user['password']);

if (!$passwordOk || empty($user['superuser_access'])) {
    fwrite(STDERR, "Failed to verify Matomo admin credentials for {$login}\n");
    exit(1);
}

echo "Matomo admin {$login} is ready\n";
