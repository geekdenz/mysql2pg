# BookStack (Laravel/Eloquent) talking to PostgreSQL through the mysql2pg
# middleware. BookStack supports MySQL/MariaDB only, so nothing in this image
# has a PostgreSQL code path to fall back on.
#
# Versions are build arguments, not pins.
ARG PHP_VERSION=8.3
FROM php:${PHP_VERSION}-apache

ARG BOOKSTACK_VERSION=release

RUN apt-get update && apt-get install -y --no-install-recommends \
        git unzip libicu-dev libzip-dev libpng-dev libjpeg62-turbo-dev libfreetype6-dev \
        libxml2-dev libonig-dev default-mysql-client \
    && docker-php-ext-configure gd --with-freetype --with-jpeg \
    && docker-php-ext-install -j"$(nproc)" pdo_mysql mysqli intl zip gd bcmath \
    && a2enmod rewrite \
    && rm -rf /var/lib/apt/lists/*

COPY --from=composer:2 /usr/bin/composer /usr/bin/composer

ENV COMPOSER_ALLOW_SUPERUSER=1 \
    COMPOSER_MEMORY_LIMIT=-1

WORKDIR /var/www/html

# /var/www/html already exists in the php:apache image, so clone elsewhere and
# move the tree in.
RUN git clone --depth 1 --branch "${BOOKSTACK_VERSION}" \
        https://github.com/BookStackApp/BookStack.git /tmp/bookstack \
    && git -C /tmp/bookstack describe --tags --always > /etc/bookstack-version \
    && cp -a /tmp/bookstack/. /var/www/html/ \
    && rm -rf /tmp/bookstack \
    && composer install --no-interaction --no-progress --no-dev --prefer-dist

# BookStack serves from public/.
ENV APACHE_DOCUMENT_ROOT=/var/www/html/public
RUN sed -ri -e 's!/var/www/html!${APACHE_DOCUMENT_ROOT}!g' \
        /etc/apache2/sites-available/*.conf /etc/apache2/apache2.conf

COPY app/ /var/www/html/app/
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh \
    && chown -R www-data:www-data /var/www/html

ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["apache2-foreground"]
