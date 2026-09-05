#!/bin/sh
set -eu

while true; do
    if php /opt/deployment/installed.php; then
        if ! php /var/www/html/console core:archive --url="$MATOMO_ARCHIVE_URL"; then
            echo 'Matomo archive failed; see the error above. Retrying in five minutes.' >&2
        fi
    else
        echo 'Waiting for the Matomo installation wizard to finish.'
    fi
    sleep 300 &
    wait "$!"
done
