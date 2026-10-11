"""Linux-only container smoke test with a deliberately FAKE dstack Unix socket.

Checks router plumbing, not TDX security. Never sends real credentials/data.
"""
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import socketserver
import ssl
import subprocess
import sys
import tempfile
import threading
import time


def docker(*args, **kwargs):
    return subprocess.run(["docker", *args], check=True, capture_output=True, **kwargs).stdout


def main(image):
    reports = []
    healthy = [True]

    class Mock(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            assert self.path == "/GetQuote"
            assert not self.headers.get("authorization")
            body = json.loads(self.rfile.read(int(self.headers["content-length"])))
            reports.append(body["report_data"])
            self.send_response(200 if healthy[0] else 503)
            data = json.dumps({"quote": "00", "event_log": "[]"}).encode()
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    with tempfile.TemporaryDirectory(prefix="teellm-phala-smoke-") as tmp:
        socket_path = str(Path(tmp) / "dstack.sock")
        with socketserver.UnixStreamServer(socket_path, Mock) as server:
            # The non-root lab user owns this fixture socket; container UID 0
            # intentionally has no DAC override capability. Real dstack owns
            # its socket as root. Only this private temporary fixture is 0666.
            os.chmod(socket_path, 0o666)
            threading.Thread(target=server.serve_forever, daemon=True).start()
            env = dict(os.environ, TEELLM_ADMIN_KEY="local-smoke-fixture-not-a-real-credential")
            container = docker("run", "-d", "--read-only", "--cap-drop", "ALL",
                               "--security-opt", "no-new-privileges:true", "--pids-limit", "128",
                               "--ulimit", "core=0", "--log-driver", "none",
                               "--env", "TEELLM_ADMIN_KEY", "-p", "127.0.0.1::8443",
                               "-v", socket_path + ":/var/run/dstack.sock:ro", image,
                               env=env).decode().strip()
            try:
                info = json.loads(docker("inspect", container))[0]
                port = int(info["NetworkSettings"]["Ports"]["8443/tcp"][0]["HostPort"])
                # Only this local fixture bypasses certificate trust so it can inspect
                # the transport. Production uses the Rust attestation verifier.
                context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
                context.check_hostname = False
                context.verify_mode = ssl.CERT_NONE
                for attempt in range(60):
                    try:
                        connection = http.client.HTTPSConnection("127.0.0.1", port, context=context, timeout=3)
                        connection.connect()
                        break
                    except OSError:
                        if attempt == 59:
                            raise
                        time.sleep(0.2)
                cert = connection.sock.getpeercert(binary_form=True)
                nonce = "a1" * 32
                body = json.dumps({"nonce": nonce})
                connection.request("POST", "/.well-known/teellm/attestation", body,
                                   {"Content-Type": "application/json"})
                response = connection.getresponse()
                envelope = json.loads(response.read())
                assert response.status == 200, envelope
                assert envelope["evidence_type"] == "dstack_tdx"
                assert envelope["nonce"] == nonce
                assert envelope["certificate_sha256"] == hashlib.sha256(cert).hexdigest()
                expected = hashlib.sha512(b"teellm:attested-channel:v1\0" + bytes.fromhex(nonce)
                                          + hashlib.sha256(cert).digest()).hexdigest()
                assert reports == [expected], reports
                assert envelope["evidence"]["quote"] == "00"  # invalid hardware evidence by design
                healthy[0] = False
                connection.request("POST", "/.well-known/teellm/attestation", body,
                                   {"Content-Type": "application/json"})
                response = connection.getresponse()
                assert response.status == 503
                assert json.loads(response.read())["error"]["type"] == "attestation_unavailable"
                connection.close()
                shell = subprocess.run(["docker", "exec", container, "/bin/sh", "-c", "true"], capture_output=True)
                assert shell.returncode != 0
                print("PASS: read-only single router, Unix socket quote binding, no fallback, no shell; NOT a TEE proof")
            finally:
                docker("rm", "-f", container)
                server.shutdown()


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "teellm-phala:local")
