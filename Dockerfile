FROM rust:1.94.1-bookworm AS builder
WORKDIR /app

COPY Cargo.toml ./
COPY Cargo.lock ./
COPY src ./src
COPY config ./config

RUN cargo build --release --locked

FROM debian:bookworm-slim
WORKDIR /app

RUN apt-get update     && apt-get install -y --no-install-recommends ca-certificates     && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/mysql2pg-middleware /usr/local/bin/mysql2pg-middleware
COPY config ./config

EXPOSE 8080
EXPOSE 3306

CMD ["mysql2pg-middleware", "--config", "/app/config/example.toml", "serve"]
