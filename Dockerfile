# syntax=docker/dockerfile:1
# Сборка CMS: статический бинарник + шаблоны, статика и миграции.
FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
# Модуль проекта может отсутствовать в чистом клоне — build.rs это допускает.
COPY project ./project
COPY migrations ./migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /src/target/release/cms /usr/local/bin/cms
COPY templates ./templates
COPY static ./static
COPY migrations ./migrations
RUN useradd --system --uid 10001 --home /app cms \
    && mkdir -p /app/upload /app/mail \
    && chown -R cms:cms /app
USER cms
ENV BIND_ADDR=0.0.0.0:3000 \
    TEMPLATES_DIR=/app/templates \
    STATIC_DIR=/app/static \
    UPLOAD_DIR=/app/upload \
    MAIL_DIR=/app/mail
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3000/api/health || exit 1
CMD ["cms"]
