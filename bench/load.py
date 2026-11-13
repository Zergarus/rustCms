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


def request_once(conn, path, body, headers, timeout=30):
    conn.request("POST", path, body=body, headers=headers)
    resp = conn.getresponse()
    resp.read()
    return resp.status


def worker(url, payload, ctx, total_requests, deadline, warmup_done, out):
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
    try:
        while True:
            if deadline is not None and time.monotonic() >= deadline:
                break
            if deadline is None and done >= total_requests:
                break
            start = time.perf_counter()
            try:
                status = request_once(conn, path, body, headers)
                if status != 200:
                    errors += 1
            except Exception:
                errors += 1
                try:
                    conn.close()
                except Exception:
                    pass
                conn = http.client.HTTPConnection(
                    parts.hostname, parts.port or 80, timeout=30
                )
            latencies.append((time.perf_counter() - start) * 1000.0)
            done += 1
    finally:
        try:
            conn.close()
        except Exception:
            pass
    out.put((done, errors, latencies))


def percentile(values, p):
    if not values:
        return 0.0
    values = sorted(values)
    idx = min(len(values) - 1, int(len(values) * p))
    return values[idx]


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
    args = parser.parse_args()

    headers = []
    for raw in args.header:
        key, _, value = raw.partition(":")
        headers.append((key.strip(), value.strip()))
    ctx = {"host": args.host, "headers": headers}

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

    workers = args.concurrency
    queue = mp.Queue()
    start_event = mp.Event()
    deadline = None
    total = None
    if args.mode == "latency":
        workers = 1
        total = args.requests
        per_worker = args.requests
    else:
        per_worker = 0

    procs = []
    started = time.perf_counter()
    for _ in range(workers):
        if args.mode == "throughput":
            dl = started + args.duration
        else:
            dl = None
        p = mp.Process(target=worker, args=(args.url, args.payload, ctx, per_worker, dl, start_event, queue))
        p.start()
        procs.append(p)
    for p in procs:
        p.join()
    elapsed = time.perf_counter() - started

    counts = []
    errors = 0
    latencies = []
    while not queue.empty():
        done, err, lat = queue.get()
        counts.append(done)
        errors += err
        latencies.extend(lat)
    total_done = sum(counts)
    rps = total_done / elapsed if elapsed else 0.0
    result = {
        "mode": args.mode,
        "url": args.url,
        "total": total_done,
        "concurrency": workers,
        "elapsed_s": round(elapsed, 3),
        "rps": round(rps, 1),
        "errors": errors,
        "p50_ms": round(percentile(latencies, 0.50), 2),
        "p95_ms": round(percentile(latencies, 0.95), 2),
        "p99_ms": round(percentile(latencies, 0.99), 2),
        "mean_ms": round(statistics.mean(latencies), 2) if latencies else 0.0,
    }
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    main()
