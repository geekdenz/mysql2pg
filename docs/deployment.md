# Deploy Matomo with mysql2pg

Run from this checkout:

```bash
./deploy.sh
```

The defaults deploy to `tim@wmsvt.com:matomo-mysql2pg`. You need SSH access,
tar and Git locally, and Docker Compose, Bash, flock, OpenSSL and curl remotely.
The script uploads only the middleware build inputs and deployment files from
the working tree. It builds the middleware on the server, waits for PostgreSQL
and Matomo health checks, and checks PHP database access through the middleware.
Local `.env`, application data and test artifacts are never uploaded.

Override the destination if needed:

```bash
DEPLOY_HOST=user@server DEPLOY_DIR=matomo-mysql2pg ./deploy.sh
```

## Access and first installation

Only Matomo's HTTP port is published, on the remote loopback address. PostgreSQL
and both middleware interfaces are confined to the deployment's private Docker
network because middleware authentication is currently permissive.

```bash
ssh -N -L 18084:127.0.0.1:18084 tim@wmsvt.com
```

Open `http://localhost:18084` and complete the Matomo wizard. The database fields
are supplied by the container environment: host `middleware`, adapter `PDO\MYSQL`,
database and user `matomo`, prefix `matomo_`, and the generated database password.
Select `MariaDB` as the database type. The `MYSQLI` adapter and `MySQL` database
type do not follow the compatibility path used and tested by this deployment.
Choose your administrator credentials and the website to track in the wizard.
If the password field needs entering manually, retrieve `POSTGRES_PASSWORD` from
the remote `shared/.env.remote`; do not commit or share this file.

The deployment uses the [official Matomo Docker image](https://github.com/matomo-org/docker),
pinned to `5.10.1-apache`. A healthy container proves HTTP and database connectivity;
it does not mean the installation wizard has been completed.

For public HTTPS access, configure a chosen hostname on the existing reverse
proxy. Matomo joins the existing external Docker network `web` (override with
`PROXY_NETWORK` in the remote environment file). The proxy must also be on that
network and route to `matomo-mysql2pg-matomo-1:80`; its own loopback address cannot
reach the host port. Only Matomo joins the proxy network; the middleware and
PostgreSQL remain private. The script does not change shared proxy configuration.
After HTTPS works, configure trusted hosts and proxy settings according to
[Matomo's proxy documentation](https://matomo.org/faq/how-to-install/faq_98/).

## Persistent state and repeat deployments

Remote layout:

```text
~/matomo-mysql2pg/
  current -> releases/<timestamp>-<commit>
  releases/<timestamp>-<commit>/
  shared/.env.remote
  shared/deployed-revision
  backups/<timestamp>-<commit>/
```

`shared/.env.remote` is generated once with a random database password and mode
`0600`. Edit the remote file to change the loopback HTTP port or archive URL.
Changing the stored database password alone does not change PostgreSQL's password.
Compose uses a stable project name so volumes survive new releases and reboots.
Before replacing an existing deployment, the script backs up the PostgreSQL database,
Matomo configuration and deployment settings. Backups contain secrets and stay
in restricted directories on the server. Take off-host backups separately.

The `archive` service starts after Matomo becomes healthy and runs every five
minutes once installation finishes. Disable browser-triggered archiving in
Matomo after checking that `core:archive` succeeds. A fresh installation logs
that it is waiting for setup, without trying to archive a missing schema.

Inspect the deployment:

```bash
ssh tim@wmsvt.com
cd ~/matomo-mysql2pg/current
export DEPLOY_RELEASE="$(basename "$(pwd -P)")"
docker compose --env-file "$HOME/matomo-mysql2pg/shared/.env.remote" -f compose.remote.yml ps
docker compose --env-file "$HOME/matomo-mysql2pg/shared/.env.remote" -f compose.remote.yml logs --tail=100
```

The script retains old release images and directories for inspection or rollback;
it does not prune volumes. Matomo application code also persists in its volume.
Changing `MATOMO_IMAGE_TAG` alone does not upgrade that code: perform Matomo's
application/database update deliberately after a backup. Re-deployment does not
reset the installer, run schema migrations, or replace administrator credentials.

## MySQL/MariaDB builtin functions with no PostgreSQL equivalent

Some MySQL/MariaDB builtin functions have no PostgreSQL equivalent (for example
`CRC32()`, which Matomo calls on every tracking request to look up
`matomo_log_action`). The middleware maintains a registry of these in
`MYSQL_COMPAT_FUNCTIONS` (`src/executor.rs`) and creates matching PostgreSQL
functions automatically: once eagerly on every startup (`CREATE OR REPLACE`, safe
to repeat), and again just-in-time the first time any query hits PostgreSQL's
"function does not exist" error for one of them. No manual migration step is
needed, including after a fresh database or `reset-matomo.sh`.

Currently covered: `CRC32`, `SHA2`, `WEEKDAY`, `DAYNAME`, `MONTHNAME`, `LOCATE`.
(`MD5` needs nothing extra — PostgreSQL's native `md5()` already matches MySQL's
signature and output. Functions whose MySQL syntax uses a bare keyword argument,
such as `TIMESTAMPDIFF(unit, ...)` and `TIMESTAMPADD(unit, ...)`, can't be handled
this way — PostgreSQL would see `unit` as an undefined column, not an undefined
function, so those need a translator-level rewrite instead; see
`rewrite_mysql_functions` in `src/translator.rs`.) Add another entry to
`MYSQL_COMPAT_FUNCTIONS` for any other missing function as it's discovered — no
other code changes are needed for it to install both eagerly and just-in-time.

## Reset Matomo

If an installation attempt leaves a partial schema and the data can be discarded,
run:

```bash
./reset-matomo.sh
```

The script asks for confirmation, saves a PostgreSQL dump and Matomo configuration
under the remote `backups/` directory, empties the Matomo schema, clears installer
sessions and configuration, restarts Matomo, and waits for a healthy installation
page. Use `./reset-matomo.sh --yes` for non-interactive operation. `DEPLOY_HOST` and
`DEPLOY_DIR` use the same overrides as `deploy.sh`.
