# Matomo Test Setup

This repository includes an optional Docker Compose service for testing `mysql2pg-middleware` with a real MySQL-native application: Matomo.

## Version

The Compose service uses the official Matomo Docker image and defaults to `matomo:5.10.1-apache`.

You can override the tag without editing Compose:

```bash
MATOMO_IMAGE_TAG=5.10.1-apache docker compose --profile matomo up --build -d
```

## Start the stack

```bash
docker compose --profile matomo up --build -d
```

Matomo will be available on:

```text
http://127.0.0.1:8081
```

If `MATOMO_PORT` is set in `.env`, use that port instead.

The Matomo app/config volume defaults to a clean named volume:

```env
MATOMO_DATA_VOLUME=mysql2pg-middleware_matomo_manual_install_data
```

This avoids stale generated Matomo config from older Matomo app/config volumes. A stale `config/config.ini.php` can make Matomo treat itself as installed while the configured database is missing tables, which produces errors such as `relation "matomo_changes" does not exist`.

## Database wiring

The Matomo container does not set `MATOMO_DATABASE_*` environment variables, so database setup happens manually in the installer.

To test the middleware's MySQL-compatible frontend, enter:

- host: `middleware`
- adapter: `MYSQLI`
- database: `app`
- username: `anyuser`
- password: `matomo`

PostgreSQL is still the actual storage engine behind the middleware.

## What this verifies

This setup is useful for checking:

- client handshake compatibility
- metadata statement coverage such as `SHOW VARIABLES` and `SHOW TABLES`
- whether a real PHP application can reach the translated backend
- which remaining MySQL features block full installation

## Current expectation

The Matomo service should be able to start and reach the initial web installer. Full application installation may still hit unsupported SQL, especially around broader MySQL DDL such as `ALTER TABLE`, index management, and other schema-management statements not fully translated yet.

## Archive cron

For a local or single-container setup, start the opt-in archive worker:

```bash
docker compose --profile matomo --profile matomo-archive up -d matomo matomo-archive
```

For production, run `php /var/www/html/console core:archive --url=https://your-matomo-host.example` from the host scheduler and disable browser-triggered archiving in Matomo. The worker must use the same database configuration and a URL reachable from its container.

Server-side `LOAD DATA INFILE` is translated to PostgreSQL `COPY`; the referenced file must be readable by PostgreSQL, so production deployments should mount the same import directory into both containers. `LOAD DATA LOCAL INFILE` remains explicitly rejected until the MySQL wire library exposes the client-local-infile data phase.

## Configuration check

Validate the Matomo service without starting containers:

```bash
docker compose --profile matomo config
```

That confirms the Compose wiring and rendered image tag. It does not start Matomo or modify Matomo configuration.

The Matomo System Check URL probes must be reachable from inside the Matomo container. The test stack therefore makes Apache listen internally on `8084`, matching its published test URL. In another deployment, do not configure Matomo with a host-only `localhost:<published-port>` URL; use the Docker service name or an address resolvable from the Matomo container.

When HTTPS is terminated by a reverse proxy, set `force_ssl = 1` in the `[General]` section of `config/config.ini.php` only after the HTTPS URL is reachable from both clients and the Matomo container:

```ini
[General]
force_ssl = 1
```

## Stale app volume

If Matomo reports missing tables such as `matomo_changes`, recreate the container using the clean default app/config volume:

```bash
docker compose --profile matomo up -d --force-recreate matomo
```

To intentionally use the old volume for inspection or rollback, set:

```env
MATOMO_DATA_VOLUME=mysql2pg-middleware_matomo_data
```
