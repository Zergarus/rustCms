#!/usr/bin/env bash
# Три перемежающихся раунда: latency c=1 и throughput c=16 на пяти ручках.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RES="$ROOT/bench/results"
CMS_URL="${CMS_URL:-http://127.0.0.1:8082}"
BITRIX_URL="${BITRIX_URL:-http://127.0.0.1:8081}"
ROUNDS="${BENCH_ROUNDS:-2}"
DURATION="${BENCH_DURATION:-10}"
LATENCY_REQUESTS="${BENCH_LATENCY_REQUESTS:-200}"
THROUGHPUT_CLIENTS="${BENCH_THROUGHPUT_CLIENTS:-16}"
mkdir -p "$RES"
rm -f "$RES"/*.json "$RES"/*.txt

ENDPOINTS=(
  "product|/api/v1/iblock/catalog/element/38382|product.json"
  "list|/api/v1/iblock/catalog/element/list|list.json"
  "sections|/api/v1/iblock/catalog/section/list|sections.json"
  "search|/api/v1/iblock/catalog/search|search.json"
  "schemes|/api/v1/iblock/schemes/element/list|schemes.json"
)

run_one() {
  local system="$1" path="$2" payload="$3" mode="$4" round="$5"
  local url header=()
  if [[ "$system" == "cms" ]]; then url="$CMS_URL"; else url="$BITRIX_URL"; header=(--header "X-Forwarded-Proto: https"); fi
  local args=(--url "$url$path" --payload "$ROOT/bench/payloads/$payload" --mode "$mode"
    --concurrency 1 --requests "$LATENCY_REQUESTS" --duration "$DURATION" --warmup 30)
  if [[ "$mode" == "throughput" ]]; then
    args=(--url "$url$path" --payload "$ROOT/bench/payloads/$payload" --mode throughput
      --concurrency "$THROUGHPUT_CLIENTS" --duration "$DURATION" --warmup 30)
  fi
  if [[ ${#header[@]} -gt 0 ]]; then args+=(--header "X-Forwarded-Proto: https"); fi
  local out="$RES/${mode}-${system}-${name}-r${round}.json"
  python3 "$ROOT/bench/load.py" "${args[@]}" > "$out"
  python3 - "$out" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
if r["errors"]:
    raise SystemExit(f"errors={r['errors']} in {sys.argv[1]}: {r}")
print(f"{r['mode']:10} {r['url']:60} rps={r['rps']:8.1f} p50={r['p50_ms']:7.2f} p95={r['p95_ms']:8.2f} p99={r['p99_ms']:8.2f}")
PY
}

{
  echo "date=$(date -u +%FT%TZ)"
  echo "commit=$(git -C "$ROOT" rev-parse --short HEAD)"
  echo "host=$(uname -a)"
  echo "cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo n/a)"
  docker version --format 'docker={{.Server.Version}}'
  echo "cms_url=$CMS_URL"
  echo "bitrix_url=$BITRIX_URL"
  echo "rounds=$ROUNDS duration=${DURATION}s latency_requests=$LATENCY_REQUESTS throughput_clients=$THROUGHPUT_CLIENTS"
  docker stats --no-stream --format 'mem {{.Name}} {{.MemUsage}} cpu {{.CPUPerc}}' cms-bench bench-php-fpm bench-bitrix-nginx bench-nginx || true
} > "$RES/environment.txt"

for round in $(seq 1 "$ROUNDS"); do
  for entry in "${ENDPOINTS[@]}"; do
    IFS='|' read -r name path payload <<< "$entry"
    systems=(cms bitrix)
    if (( round % 2 == 0 )); then systems=(bitrix cms); fi
    for system in "${systems[@]}"; do
      run_one "$system" "$path" "$payload" latency "$round"
      run_one "$system" "$path" "$payload" throughput "$round"
    done
  done
done

docker stats --no-stream --format 'mem {{.Name}} {{.MemUsage}} cpu {{.CPUPerc}}' cms-bench bench-php-fpm bench-bitrix-nginx bench-nginx >> "$RES/environment.txt" || true
echo "done: $RES"
