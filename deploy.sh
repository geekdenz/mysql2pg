#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# Read a single key from .env without sourcing it; the file holds values with
# spaces (connection strings) that a shell would try to execute.
read_env_key() {
    local key="$1" file="$ROOT_DIR/.env" line
    [[ -f "$file" ]] || return 0
    line="$(grep -m1 -E "^[[:space:]]*${key}=" "$file")" || return 0
    line="${line#*=}"
    line="${line%$'\r'}"
    line="${line%\"}"; line="${line#\"}"
    line="${line%\'}"; line="${line#\'}"
    printf '%s' "$line"
}
DEPLOY_HOST="${DEPLOY_HOST:-$(read_env_key DEPLOY_HOST)}"
DEPLOY_DIR="${DEPLOY_DIR:-$(read_env_key DEPLOY_DIR)}"
DEPLOY_DIR="${DEPLOY_DIR:-matomo-mysql2pg}"
DEPLOY_RELEASE="$(date -u +%Y%m%dT%H%M%SZ)-$(git -C "$ROOT_DIR" rev-parse --short HEAD)"
DEPLOY_REVISION="$(git -C "$ROOT_DIR" rev-parse HEAD)"

if [[ "${1:-}" == --help ]]; then
    printf '%s\n' \
        'Usage: ./deploy.sh' \
        'Settings: DEPLOY_HOST (required; set it in .env) DEPLOY_DIR=matomo-mysql2pg' \
        'Deploys the working source tree; secrets and volumes stay on the server.' \
        'Remote settings: ~/matomo-mysql2pg/shared/.env.remote' \
        'Access: ssh -L 18084:127.0.0.1:18084 "$DEPLOY_HOST"' \
        'Then open http://localhost:18084 and complete the Matomo installer.'
    exit 0
fi
[[ $# == 0 ]] || { echo 'Use --help for usage.' >&2; exit 2; }
# The path is embedded in remote shell commands; accept simple relative paths only.
[[ "$DEPLOY_DIR" =~ ^[a-zA-Z0-9_-]+(/[a-zA-Z0-9_-]+)*$ ]] || {
    echo 'DEPLOY_DIR must be a simple path relative to the remote home directory.' >&2
    exit 2
}
[[ -n "$DEPLOY_HOST" ]] || {
    echo 'DEPLOY_HOST is not set. Add it to .env (see .env.example) or export it.' >&2
    exit 2
}
[[ "$DEPLOY_HOST" =~ ^[a-zA-Z0-9_][a-zA-Z0-9_.@-]*$ ]] || {
    echo 'Invalid DEPLOY_HOST; use an SSH host alias or user@hostname.' >&2
    exit 2
}
for tool in ssh tar git; do command -v "$tool" >/dev/null; done

ssh_args=(-o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=4)
ssh "${ssh_args[@]}" "$DEPLOY_HOST" 'command -v docker >/dev/null && docker compose version && command -v openssl >/dev/null && command -v curl >/dev/null'

remote_release="$DEPLOY_DIR/releases/$DEPLOY_RELEASE"
printf 'Uploading working source %s to %s:%s\n' "$DEPLOY_RELEASE" "$DEPLOY_HOST" "$remote_release"
tar -C "$ROOT_DIR" -czf - \
    Cargo.toml Cargo.lock Dockerfile src vendor config compose.remote.yml deployment \
    | ssh "${ssh_args[@]}" "$DEPLOY_HOST" \
        "umask 022; mkdir -p '$remote_release'; tar -xzf - -C '$remote_release'"

ssh "${ssh_args[@]}" "$DEPLOY_HOST" \
    "bash '$remote_release/deployment/remote.sh' '$DEPLOY_DIR' '$DEPLOY_RELEASE' '$DEPLOY_REVISION'"
printf '\nAccess through SSH: ssh -N -L 18084:127.0.0.1:18084 %s\n' "$DEPLOY_HOST"
printf 'Then open http://localhost:18084 (adjust the port if changed in shared/.env.remote).\n'
