#!/usr/bin/env bash
# Поднимает отдельный bench-стенд, не трогая рабочие transopt-local-* контейнеры.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SITE_DIR="${SITE_DIR:-$ROOT/../transopt.local}"
NETWORK="${BITRIX_NETWORK:-bitrix-common_bitrix}"
[[ -d "$SITE_DIR" ]] || { echo "нет SITE_DIR=$SITE_DIR" >&2; exit 1; }
[[ -d "$ROOT/../config/php-fpm" ]] || { echo "нет $ROOT/../config/php-fpm" >&2; exit 1; }
CMS_IMAGE="${CMS_IMAGE:-cms-bench}"
BITRIX_IMAGE="${BITRIX_IMAGE:-registry.gitlab.com/bitrix-docker/images/php-fpm-8.3:latest}"

if ! docker image inspect "$CMS_IMAGE" >/dev/null 2>&1; then
  docker build -t "$CMS_IMAGE" "$ROOT"
fi

docker rm -f cms-bench bench-nginx bench-bitrix-nginx bench-php-fpm >/dev/null 2>&1 || true

docker run -d --name cms-bench --network "$NETWORK" \
  --add-host=host.docker.internal:host-gateway \
  -p 127.0.0.1:3011:3000 \
  --cpus=4 --memory=2g \
  -e DATABASE_URL="${DATABASE_URL:-postgres://cms:cms@host.docker.internal:5433/cms}" \
  -e CORS_ORIGINS='*' -e CSRF_ENFORCE=false -e RATE_LIMIT_PER_MIN=0 \
  -e COOKIE_SECURE=false -e RUST_LOG=cms=warn \
  -v "$ROOT/upload:/app/upload:ro" \
  "$CMS_IMAGE" >/dev/null

docker run -d --name bench-php-fpm --network "$NETWORK" \
  --cpus=4 --memory=2g \
  --env-file "$SITE_DIR/.env" \
  -e PHP_INI_SCAN_DIR=/usr/local/etc/php/conf.d/common/:/usr/local/etc/php/conf.d/:/usr/local/etc/php/conf.d/site/ \
  -v "$SITE_DIR:/var/www" \
  -v "$ROOT/bench/bitrix/php.ini:/usr/local/etc/php/conf.d/zz-bench.ini:ro" \
  -v "$ROOT/bench/bitrix/xdebug-off.ini:/usr/local/etc/php/conf.d/docker-php-ext-xdebug.ini:ro" \
  -v "$ROOT/bench/bitrix/www.conf:/usr/local/etc/php-fpm.d/www.conf:ro" \
  -v "$ROOT/../config/php-fpm:/usr/local/etc/php/conf.d/common:ro" \
  -v "$SITE_DIR/config/php-fpm/dev:/usr/local/etc/php/conf.d/site:ro" \
  "$BITRIX_IMAGE" >/dev/null

docker run -d --name bench-bitrix-nginx --network "$NETWORK" \
  -p 127.0.0.1:8081:80 \
  -v "$ROOT/bench/bitrix/nginx.conf:/etc/nginx/conf.d/default.conf:ro" \
  -v "$SITE_DIR:/var/www:ro" \
  nginx:latest >/dev/null

docker run -d --name bench-nginx --network "$NETWORK" \
  -p 127.0.0.1:8082:80 \
  -v "$ROOT/bench/nginx-cms.conf:/etc/nginx/conf.d/default.conf:ro" \
  nginx:alpine >/dev/null

sleep 4

if [[ "${1:-}" == "--check" ]]; then
  set -x
  curl -fsS -o /dev/null -w 'cms health %{http_code}\n' http://127.0.0.1:3011/api/health
  curl -fsS -o /dev/null -w 'cms nginx %{http_code}\n' -H 'Host: transopt.local' http://127.0.0.1:8082/api/v1/iblock/list
  curl -fsS -o /dev/null -w 'bitrix nginx %{http_code}\n' -H 'Host: transopt.local' -H 'X-Forwarded-Proto: https' http://127.0.0.1:8081/api/v1/iblock/list
fi
