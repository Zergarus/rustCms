#!/usr/bin/env python3
"""Сводит сырые результаты замера в медианы, min–max и отношения по раундам."""
import argparse
import json
import statistics
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


def load(results):
    rows = {}
    for path in results.glob("*.json"):
        parts = path.stem.split("-")
        if len(parts) != 4 or not parts[3].startswith("r"):
            continue
        mode, system, endpoint, round_no = parts
        rows.setdefault((mode, system, endpoint), []).append(
            (int(round_no[1:]), json.loads(path.read_text()))
        )
    return rows


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("results", nargs="?", default="bench/results")
    parser.add_argument("--json-out")
    args = parser.parse_args()
    rows = load(Path(args.results))

    expected = {(m, s, e) for m in MODES for s in SYSTEMS for e in LABELS}
    missing = expected - set(rows)
    if missing:
        raise SystemExit(f"нет результатов: {sorted(missing)}")
    for key, items in rows.items():
        if len(items) != 2:
            raise SystemExit(f"{key}: ожидалось 2 раунда, получено {len(items)}")
    if any(row.get("errors") for items in rows.values() for _, row in items):
        raise SystemExit("в результатах есть ошибки — сводка остановлена")

    summary = {}
    ratios = {}
    for mode, endpoint in [(m, e) for m in MODES for e in LABELS]:
        data = {}
        for system in SYSTEMS:
            items = [row for _, row in rows[(mode, system, endpoint)]]
            data[system] = {
                "rps_median": round(statistics.median(x["rps"] for x in items), 1),
                "rps_min": min(x["rps"] for x in items),
                "rps_max": max(x["rps"] for x in items),
                "p50_median": round(statistics.median(x["p50_ms"] for x in items), 2),
                "p95_median": round(statistics.median(x["p95_ms"] for x in items), 2),
                "p99_median": round(statistics.median(x["p99_ms"] for x in items), 2),
                "errors": sum(x["errors"] for x in items),
                "runs": len(items),
            }
        summary[f"{mode}:{endpoint}"] = data

        per_round = []
        for round_no in (1, 2):
            c = next(row for r, row in rows[(mode, "cms", endpoint)] if r == round_no)
            b = next(row for r, row in rows[(mode, "bitrix", endpoint)] if r == round_no)
            if mode == "latency":
                per_round.append(b["p50_ms"] / c["p50_ms"])
            else:
                per_round.append(c["rps"] / b["rps"])
        ratios[f"{mode}:{endpoint}"] = {
            "median": round(statistics.median(per_round), 1),
            "min": round(min(per_round), 1),
            "max": round(max(per_round), 1),
        }

    out = {"endpoints": LABELS, "systems": SYSTEMS, "modes": MODES,
           "summary": summary, "ratios": ratios}
    if args.json_out:
        Path(args.json_out).write_text(json.dumps(out, ensure_ascii=False, indent=2) + "\n")
        return
    print(json.dumps(out, ensure_ascii=False, indent=2))

    for mode in MODES:
        print(f"\n## {MODES[mode]}\n")
        print("| Ручка | CMS rps | CMS p50/p95/p99, мс | Битрикс rps | Битрикс p50/p95/p99, мс | × (min–max) |")
        print("|---|---:|---:|---:|---:|---:|")
        for endpoint in LABELS:
            d = summary[f"{mode}:{endpoint}"]
            c, b, r = d["cms"], d["bitrix"], ratios[f"{mode}:{endpoint}"]
            print(
                f"| {LABELS[endpoint]} | {c['rps_median']} | "
                f"{c['p50_median']} / {c['p95_median']} / {c['p99_median']} | "
                f"{b['rps_median']} | {b['p50_median']} / {b['p95_median']} / {b['p99_median']} | "
                f"{r['median']}× ({r['min']}–{r['max']}×) |"
            )


if __name__ == "__main__":
    main()
