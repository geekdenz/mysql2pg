# SilverStripe CMS talking to PostgreSQL through the mysql2pg middleware.
# The point of this image is that nothing in it knows about PostgreSQL: the
# framework is configured with its own stock MySQL/MariaDB driver.
#
# Both the framework and PHP version are build arguments, so the same Dockerfile
# covers more than one SilverStripe line:
#
#   4.13 / PHP 8.1  - ships both the PDO and mysqli connectors
#   5.x  / PHP 8.3  - mysqli only, PDO having been removed in 5.0
#
# Pass them through compose (SILVERSTRIPE_VERSION / SILVERSTRIPE_PHP_VERSION)
# rather than editing this file.
ARG PHP_VERSION=8.1
FROM php:${PHP_VERSION}-apache

ARG SILVERSTRIPE_VERSION=^4.13

RUN apt-get update && apt-get install -y --no-install-recommends \
        git unzip libicu-dev libzip-dev libpng-dev libjpeg62-turbo-dev libfreetype6-dev \
        default-mysql-client \
    && docker-php-ext-configure gd --with-freetype --with-jpeg \
    && docker-php-ext-install -j"$(nproc)" pdo_mysql mysqli intl zip gd \
    && a2enmod rewrite \
    && rm -rf /var/lib/apt/lists/*

COPY --from=composer:2 /usr/bin/composer /usr/bin/composer

ENV COMPOSER_ALLOW_SUPERUSER=1 \
    COMPOSER_MEMORY_LIMIT=-1

WORKDIR /var/www/html

# The full CMS recipe; installing at build time keeps container start fast.
RUN composer create-project --no-interaction --no-progress --prefer-dist \
        "silverstripe/installer:${SILVERSTRIPE_VERSION}" /var/www/html

RUN composer show silverstripe/framework 2>/dev/null \
        | awk '/^versions/ { print $NF }' > /etc/silverstripe-version || true

# SilverStripe serves from the public/ subdirectory.
ENV APACHE_DOCUMENT_ROOT=/var/www/html/public
RUN sed -ri -e 's!/var/www/html!${APACHE_DOCUMENT_ROOT}!g' \
        /etc/apache2/sites-available/*.conf /etc/apache2/apache2.conf

COPY app/ /var/www/html/app/
COPY mysqli-probe.php /usr/local/bin/mysqli-probe.php
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh \
    && chown -R www-data:www-data /var/www/html

ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["apache2-foreground"]
