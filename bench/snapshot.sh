#!/usr/bin/env bash
# Фиксирует снимок данных и проверяет, что CMS и Битрикс смотрят на одинаковый набор.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CMS_URL="${CMS_URL:-http://127.0.0.1:8082}"
BITRIX_URL="${BITRIX_URL:-http://127.0.0.1:8081}"
HOST="${BENCH_HOST:-transopt.local}"
PG_CONTAINER="${PG_CONTAINER:-transoptrusttest-postgres-1}"
MYSQL_CONTAINER="${MYSQL_CONTAINER:-bitrix-common-db}"
OUT="$ROOT/bench/snapshot.json"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

post() {
  local url="$1" file="$2" out="$3"
  shift 3
  curl -fsS -H "Host: $HOST" -H 'Content-Type: application/json' "$@" \
    --data-binary "@$file" "$url" -o "$out"
}

post "$CMS_URL/api/v1/iblock/catalog/element/38382" "$ROOT/bench/payloads/product.json" "$TMP/cms-product.json"
post "$BITRIX_URL/api/v1/iblock/catalog/element/38382" "$ROOT/bench/payloads/product.json" "$TMP/bitrix-product.json" -H 'X-Forwarded-Proto: https'
post "$CMS_URL/api/v1/iblock/catalog/search" "$ROOT/bench/payloads/search.json" "$TMP/cms-search.json"
post "$BITRIX_URL/api/v1/iblock/catalog/search" "$ROOT/bench/payloads/search.json" "$TMP/bitrix-search.json" -H 'X-Forwarded-Proto: https'

docker exec "$PG_CONTAINER" psql -U cms -d cms -Atc \
  "SELECT c.code || '|' || count(i.id) FROM collections c LEFT JOIN collection_items i ON i.collection_id = c.id GROUP BY c.code" \
  > "$TMP/cms-counts.txt"
docker exec "$MYSQL_CONTAINER" mysql -uroot -ppassword transopt -N -e \
  "SELECT b.CODE, count(e.ID) FROM b_iblock b LEFT JOIN b_iblock_element e ON e.IBLOCK_ID = b.ID GROUP BY b.CODE" \
  2>/dev/null | awk -F'\t' 'NF==2 {print $1 "|" $2}' > "$TMP/bitrix-counts.txt"

python3 - "$TMP" "$OUT" <<'PY'
import json, sys, subprocess
from datetime import datetime, timezone
from pathlib import Path

tmp, out = Path(sys.argv[1]), Path(sys.argv[2])

def product(path):
    data = json.loads(path.read_text())["data"]["item"]
    return {"id": data["id"], "name": data["name"]}

def search(path):
    data = json.loads(path.read_text())["data"]
    return {"items": len(data.get("items", [])), "marks": len(data.get("marks", []))}

def counts(path):
    result = {}
    for line in path.read_text().splitlines():
        code, _, value = line.strip().partition("|")
        if code:
            result[code] = int(value)
    return result

cms_product = product(tmp / "cms-product.json")
bitrix_product = product(tmp / "bitrix-product.json")
cms_search = search(tmp / "cms-search.json")
bitrix_search = search(tmp / "bitrix-search.json")
cms_counts = counts(tmp / "cms-counts.txt")
bitrix_counts = counts(tmp / "bitrix-counts.txt")

# Разные коды коллекций в CMS и Битриксе — сравниваем по парам.
pairs = {
    "catalog": "task",
    "auto": "auto",
    "marks": "marks",
    "models": "auto_variants",
    "locations": "locations",
    "schemes": "schemes",
    "news": "news",
    "info": "info",
}
mismatches = {
    f"{cms} / {bitrix}": [cms_counts.get(cms), bitrix_counts.get(bitrix)]
    for cms, bitrix in pairs.items()
    if cms_counts.get(cms) != bitrix_counts.get(bitrix)
}
if cms_product["name"] != bitrix_product["name"]:
    raise SystemExit(f"Товар 38382 отличается: {cms_product['name']!r} != {bitrix_product['name']!r}")
if not cms_search["items"] or not bitrix_search["items"]:
    raise SystemExit(f"Поиск U660 пуст: CMS={cms_search}, Bitrix={bitrix_search}")

commit = subprocess.check_output(["git", "rev-parse", "--short", "HEAD"], text=True).strip()
data = {
    "date": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    "commit": commit,
    "product": {"cms": cms_product, "bitrix": bitrix_product},
    "search_u660": {"cms": cms_search, "bitrix": bitrix_search},
    "counts": {"cms": cms_counts, "bitrix": bitrix_counts, "pairs": pairs, "mismatches": mismatches},
}
out.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
if mismatches:
    raise SystemExit(f"Расхождение counts: {mismatches}")
print(f"snapshot ok: {out} ({len(cms_counts)} коллекций CMS, {len(bitrix_counts)} Битрикса)")
PY
