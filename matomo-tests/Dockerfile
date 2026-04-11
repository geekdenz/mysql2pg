ARG MATOMO_TESTS_IMAGE_TAG=5.8.0-apache
FROM composer:2 AS composer_bin

FROM matomo:${MATOMO_TESTS_IMAGE_TAG}

COPY --from=composer_bin /usr/bin/composer /usr/local/bin/composer

RUN apt-get update \
    && apt-get install -y --no-install-recommends git unzip mariadb-client \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /var/www/html
