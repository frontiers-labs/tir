#!/usr/bin/env python3
"""Check Jevlint parsing and diagnostic transport with a local test provider."""

import http.server
import json
import os
import subprocess
import tempfile
import threading
from pathlib import Path


class Provider(http.server.BaseHTTPRequestHandler):
    fail = False
    requests = 0

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        type(self).requests += 1
        answer = {
            "type": "choice",
            "choice": "fail" if self.fail else "pass",
            "confidence": 1.0,
        }
        body = json.dumps(
            {"answers": {key: answer for key in payload["questions"]}}
        ).encode()
        self.send_response(200)
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def main():
    root = Path(__file__).resolve().parents[2]
    binary = root / "target/debug/xtask"
    with tempfile.TemporaryDirectory(prefix="jevlint-check-") as directory:
        temporary = Path(directory)
        config = temporary / ".jevlint.json"
        config.write_text(
            json.dumps(
                {
                    "languages": {"rust": {}},
                    "rules": [
                        {
                            "id": "transport",
                            "description": "Check diagnostic transport.",
                            "severity": "error",
                            "kinds": ["function"],
                        }
                    ],
                }
            )
        )
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("TYPESAFE_", "JEVLINT_"))
        }
        env.update(
            TYPESAFE_API_KEY="local-test",
            JEVLINT_PROVIDER="typesafe",
            TYPESAFE_ENDPOINT=f"http://127.0.0.1:{server.server_port}/v1/systemone",
            XDG_CACHE_HOME=str(temporary / "cache"),
        )

        def check(source, expected):
            env["XDG_CACHE_HOME"] = str(
                temporary / f"cache-{Provider.requests}-{source.name}"
            )
            before = Provider.requests
            result = subprocess.run(
                [
                    "jevlint",
                    "check",
                    "--format",
                    "json",
                    "--config",
                    str(config),
                    str(source),
                ],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            assert result.returncode == expected, result.stdout + result.stderr
            if expected == 2:
                assert "source contains syntax errors" in result.stderr
                assert Provider.requests == before
            else:
                assert Provider.requests > before
            print(f"PASS {source.name}: exit={result.returncode}")

        try:
            for name, source, expected in [
                (
                    "macro.rs",
                    "macro_rules! m { ($($t:tt)*) => {} }\nm! { a ~ b }\npub fn f() {}",
                    0,
                ),
                ("expression.rs", "[{ fn f() {} f() }]", 0),
                ("malformed.rs", "pub fn f( {", 2),
                ("malformed-expression.rs", "[{ fn f() {} f() }", 2),
            ]:
                path = temporary / name
                path.write_text(source)
                check(path, expected)
            Provider.fail = True
            check(temporary / "macro.rs", 1)
            for path in temporary.glob("malformed*.rs"):
                path.unlink()
            result = subprocess.run(
                [
                    str(binary),
                    "lint",
                    "--root",
                    str(temporary),
                    "--only",
                    "jevlint",
                    "--deny-warnings",
                ],
                env={**env, "XDG_CACHE_HOME": str(temporary / "transport-cache")},
                text=True,
                capture_output=True,
                check=False,
            )
            assert result.returncode == 1 and "jevlint transport" in result.stdout, (
                result.stdout + result.stderr
            )
            print("PASS xtask diagnostic transport: exit=1")
            Provider.fail = False
            # This checks real repository parsing, not semantic rule accuracy.
            before = Provider.requests
            result = subprocess.run(
                [
                    str(binary),
                    "lint",
                    "--root",
                    str(root),
                    "--only",
                    "jevlint",
                    "--deny-warnings",
                ],
                env={**env, "XDG_CACHE_HOME": str(temporary / "repository-cache")},
                check=False,
            )
            assert result.returncode == 0
            assert Provider.requests > before
            print(
                f"PASS full repository parse and transport: {Provider.requests} local provider requests"
            )
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
