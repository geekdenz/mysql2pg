#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_HOST="${DEPLOY_HOST:-tim@wmsvt.com}"
DEPLOY_DIR="${DEPLOY_DIR:-matomo-mysql2pg}"
assume_yes=false

if [[ "${1:-}" == --help ]]; then
    printf '%s\n' \
        'Usage: ./reset-matomo.sh [--yes]' \
        'Defaults: DEPLOY_HOST=tim@wmsvt.com DEPLOY_DIR=matomo-mysql2pg' \
        'Backs up and deletes all Matomo database data and installer state.' \
        'Use --yes for non-interactive execution.'
    exit 0
fi
if [[ "${1:-}" == --yes ]]; then
    assume_yes=true
    shift
fi
[[ $# == 0 ]] || { echo 'Use --help for usage.' >&2; exit 2; }
[[ "$DEPLOY_DIR" =~ ^[a-zA-Z0-9_-]+(/[a-zA-Z0-9_-]+)*$ ]] || {
    echo 'DEPLOY_DIR must be a simple path relative to the remote home directory.' >&2
    exit 2
}
[[ "$DEPLOY_HOST" =~ ^[a-zA-Z0-9_][a-zA-Z0-9_.@-]*$ ]] || {
    echo 'Invalid DEPLOY_HOST; use an SSH host alias or user@hostname.' >&2
    exit 2
}

if [[ "$assume_yes" != true ]]; then
    printf 'Reset all Matomo data at %s:%s? A backup will be kept remotely. [y/N] ' \
        "$DEPLOY_HOST" "$DEPLOY_DIR"
    read -r answer
    [[ "$answer" =~ ^[Yy]$ ]] || { echo 'Reset cancelled.'; exit 0; }
fi

ssh_args=(-o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=4)
ssh "${ssh_args[@]}" "$DEPLOY_HOST" bash -s -- "$DEPLOY_DIR" \
    <"$ROOT_DIR/deployment/reset-matomo.sh"
