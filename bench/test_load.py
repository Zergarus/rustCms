import json
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PAYLOAD = ROOT / "bench" / "payloads" / "schemes.json"


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        body = b'{"status":"ok"}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class LoadTest(unittest.TestCase):
    def test_latency_reports_successful_requests(self):
        server = HTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            proc = subprocess.run(
                [
                    sys.executable, str(ROOT / "bench" / "load.py"),
                    "--url", f"http://127.0.0.1:{server.server_port}",
                    "--payload", str(PAYLOAD),
                    "--mode", "latency",
                    "--requests", "3",
                    "--concurrency", "1",
                    "--warmup", "1",
                ],
                capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            result = json.loads(proc.stdout.strip().splitlines()[-1])
            self.assertEqual(result["total"], 3)
            self.assertEqual(result["errors"], 0)
            self.assertGreater(result["rps"], 0)
            self.assertGreaterEqual(result["p50_ms"], 0)
        finally:
            server.shutdown()


if __name__ == "__main__":
    unittest.main()
