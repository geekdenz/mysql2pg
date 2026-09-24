# SilverStripe CMS talking to PostgreSQL through the mysql2pg middleware.
# The point of this image is that nothing in it knows about PostgreSQL: the
# framework is configured with its stock MySQL/MariaDB PDO driver.
# SilverStripe 4.13 is the last line that ships the PDO connector
# (SilverStripe 5 removed PDO in favour of mysqli only), and it targets PHP 8.1.
FROM php:8.1-apache

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
        silverstripe/installer:^4.13 /var/www/html

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
