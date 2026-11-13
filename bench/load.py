#!/usr/bin/env python3
"""Нагрузочный генератор для bxapi: latency (c=1) и throughput (c=N)."""
import argparse
import http.client
import json
import multiprocessing as mp
import statistics
import sys
import time
from urllib.parse import urlsplit


def request_once(conn, path, body, headers):
    conn.request("POST", path, body=body, headers=headers)
    resp = conn.getresponse()
    data = resp.read()
    return resp.status, data


def is_success(body, validate):
    if not validate:
        return True
    try:
        return json.loads(body).get("status") == "success"
    except Exception:
        return False


def worker(url, payload, ctx, total_requests, duration, out):
    parts = urlsplit(url)
    path = parts.path or "/"
    body = open(payload, "rb").read()
    headers = {
        "Host": ctx["host"],
        "Content-Type": "application/json",
        "Connection": "keep-alive",
    }
    for key, value in ctx["headers"]:
        headers[key] = value
    conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=30)
    latencies = []
    errors = 0
    done = 0
    first_start = time.monotonic()
    deadline = first_start + duration if duration is not None else None
    try:
        while True:
            if deadline is not None and time.monotonic() >= deadline:
                break
            if deadline is None and done >= total_requests:
                break
            start = time.perf_counter()
            try:
                status, data = request_once(conn, path, body, headers)
                done += 1
                if status != 200 or not is_success(data, ctx["validate"]):
                    errors += 1
                    continue
                latencies.append((time.perf_counter() - start) * 1000.0)
            except Exception:
                done += 1
                errors += 1
                try:
                    conn.close()
                except Exception:
                    pass
                conn = http.client.HTTPConnection(
                    parts.hostname, parts.port or 80, timeout=30
                )
    finally:
        try:
            conn.close()
        except Exception:
            pass
    out.put((done, errors, latencies, first_start, time.monotonic()))


def percentile(values, p):
    if not values:
        return 0.0
    values = sorted(values)
    import math

    idx = max(0, math.ceil(p * len(values)) - 1)
    return values[min(idx, len(values) - 1)]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--payload", required=True)
    parser.add_argument("--mode", choices=["latency", "throughput"], required=True)
    parser.add_argument("--requests", type=int, default=200)
    parser.add_argument("--concurrency", type=int, default=1)
    parser.add_argument("--duration", type=float, default=10.0)
    parser.add_argument("--warmup", type=int, default=30)
    parser.add_argument("--host", default="transopt.local")
    parser.add_argument("--header", action="append", default=[])
    parser.add_argument("--validate", action="store_true", help="проверять status=success в JSON")
    args = parser.parse_args()

    headers = []
    for raw in args.header:
        key, _, value = raw.partition(":")
        headers.append((key.strip(), value.strip()))
    ctx = {"host": args.host, "headers": headers, "validate": args.validate}

    # Прогрев: первый запрос после простоя не должен попасть в замер.
    parts = urlsplit(args.url)
    warm = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=30)
    body = open(args.payload, "rb").read()
    for _ in range(args.warmup):
        try:
            request_once(
                warm,
                parts.path or "/",
                body,
                {"Host": args.host, "Content-Type": "application/json", **dict(headers)},
            )
        except Exception:
            pass
    warm.close()

    workers = 1 if args.mode == "latency" else args.concurrency
    total_requests = args.requests if args.mode == "latency" else None
    duration = args.duration if args.mode == "throughput" else None
    queue = mp.Queue()
    procs = []
    for _ in range(workers):
        p = mp.Process(
            target=worker,
            args=(args.url, args.payload, ctx, total_requests, duration, queue),
        )
        p.start()
        procs.append(p)

    counts = []
    errors = 0
    latencies = []
    starts = []
    ends = []
    received = 0
    while received < workers:
        try:
            done, err, lat, first, last = queue.get(timeout=1.0)
            received += 1
            counts.append(done)
            errors += err
            latencies.extend(lat)
            starts.append(first)
            ends.append(last)
        except Exception:
            if all(not p.is_alive() for p in procs):
                break
    for p in procs:
        p.join()
    while True:
        try:
            done, err, lat, first, last = queue.get_nowait()
        except Exception:
            break
        counts.append(done)
        errors += err
        latencies.extend(lat)
        starts.append(first)
        ends.append(last)

    if len(counts) != workers:
        print(
            f"worker died: got {len(counts)} results, expected {workers}",
            file=sys.stderr,
        )
        return 2

    started = min(starts) if starts else time.monotonic()
    finished = max(ends) if ends else started
    elapsed = max(finished - started, 1e-9)
    attempts = sum(counts)
    total_done = max(attempts - errors, 0)
    result = {
        "mode": args.mode,
        "url": args.url,
        "total": total_done,
        "attempts": attempts,
        "concurrency": workers,
        "elapsed_s": round(elapsed, 3),
        "rps": round(total_done / elapsed, 1),
        "errors": errors,
        "p50_ms": round(percentile(latencies, 0.50), 2),
        "p95_ms": round(percentile(latencies, 0.95), 2),
        "p99_ms": round(percentile(latencies, 0.99), 2),
        "mean_ms": round(statistics.mean(latencies), 2) if latencies else 0.0,
    }
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    sys.exit(main())
