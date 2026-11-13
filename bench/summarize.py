#!/usr/bin/env python3
"""Сводит сырые результаты замера в медианы и min–max."""
import json
import statistics
import sys
from pathlib import Path

LABELS = {
    "product": "Карточка товара",
    "list": "Список товаров",
    "sections": "Разделы каталога",
    "search": "Поиск U660",
    "schemes": "Схемы (72–190 КБ)",
}
SYSTEMS = {"cms": "CMS", "bitrix": "Битрикс"}
MODES = {"latency": "Latency c=1", "throughput": "Throughput c=16"}


def load(results: Path):
    rows = {}
    for path in results.glob("*.json"):
        parts = path.stem.split("-")
        # mode-system-endpoint-rN
        if len(parts) != 4 or parts[3][0] != "r":
            continue
        mode, system, endpoint, _ = parts
        row = json.loads(path.read_text())
        rows.setdefault((mode, system, endpoint), []).append(row)
    return rows


def main():
    results = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("bench/results")
    rows = load(results)
    summary = {}
    for (mode, system, endpoint), items in sorted(rows.items()):
        summary.setdefault((mode, endpoint), {})[system] = {
            "rps_median": round(statistics.median(x["rps"] for x in items), 1),
            "rps_min": min(x["rps"] for x in items),
            "rps_max": max(x["rps"] for x in items),
            "p50_median": round(statistics.median(x["p50_ms"] for x in items), 2),
            "p95_median": round(statistics.median(x["p95_ms"] for x in items), 2),
            "p99_median": round(statistics.median(x["p99_ms"] for x in items), 2),
            "errors": sum(x["errors"] for x in items),
            "runs": len(items),
        }

    out = {
        "endpoints": LABELS,
        "systems": SYSTEMS,
        "modes": MODES,
        "summary": {
            f"{mode}:{endpoint}": data
            for (mode, endpoint), data in summary.items()
        },
    }
    print(json.dumps(out, ensure_ascii=False, indent=2))

    for mode in ("latency", "throughput"):
        print(f"\n## {MODES[mode]}\n")
        print("| Ручка | CMS rps | CMS p50/p95/p99, мс | Битрикс rps | Битрикс p50/p95/p99, мс | rps × | p50 × |")
        print("|---|---:|---:|---:|---:|---:|---:|")
        for endpoint in LABELS:
            data = summary.get((mode, endpoint))
            if not data or "cms" not in data or "bitrix" not in data:
                continue
            c, b = data["cms"], data["bitrix"]
            rps_x = c["rps_median"] / b["rps_median"] if b["rps_median"] else 0
            p50_x = b["p50_median"] / c["p50_median"] if c["p50_median"] else 0
            print(
                f"| {LABELS[endpoint]} | {c['rps_median']} | "
                f"{c['p50_median']} / {c['p95_median']} / {c['p99_median']} | "
                f"{b['rps_median']} | {b['p50_median']} / {b['p95_median']} / {b['p99_median']} | "
                f"{rps_x:.1f}× | {p50_x:.1f}× |"
            )


if __name__ == "__main__":
    main()
