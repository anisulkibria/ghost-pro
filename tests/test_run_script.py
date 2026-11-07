"""Regression checks for launcher dependency preparation and failure reporting.

Run with: python3 -m unittest discover -s tests -p 'test_run_script.py'
The server smoke checks use temporary deployments and require a release binary.
"""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "run.sh"
BINARY = ROOT / "target/release/ghost-pro"


class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="vuuut-launch-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.deployment = self.base / "deployment"
        self.deployment.mkdir()
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("PRO_") and key != "NODE_ENV"}
        self.env.update(PRO_DIR=str(self.deployment), RUST_LOG="info")
        self.config = {
            "url": "http://pro.localhost:8080", "subdomainBase": "localhost",
            "paths": {"ghostBinary": "/bin/sh", "dataDir": "data"},
            "billing": {"provider": "none"}, "mail": {"transport": "log"},
        }

    def write_config(self):
        (self.deployment / "pro.json").write_text(json.dumps(self.config))

    def preflight(self):
        self.write_config()
        harness = self.base / "preflight.sh"
        harness.write_text(SCRIPT.read_text().split('case "${1:-start}" in', 1)[0]
                           + '\nrequire_tenant_bin\n')
        return subprocess.run(["bash", str(harness)], env=self.env,
                              capture_output=True, text=True, errors="backslashreplace", timeout=15)

    def test_missing_local_cms_is_built_at_the_configured_target(self):
        checkout = self.base / "Vuuut checkout"
        (checkout / "crates/ghost").mkdir(parents=True)
        (checkout / "Cargo.toml").write_text("[workspace]\n")
        (checkout / "crates/ghost/Cargo.toml").write_text('[package]\nname="ghost"\n')
        binary = checkout / "target/release/ghost"
        self.config["paths"]["ghostBinary"] = str(binary)
        commands = self.base / "commands"
        commands.mkdir()
        cargo = commands / "cargo"
        cargo.write_text('''#!/usr/bin/env python3
import pathlib, sys
args = sys.argv[1:]
assert args[:2] == ["build", "--release"]
target = pathlib.Path(args[args.index("--target-dir") + 1]) / "release/ghost"
target.parent.mkdir(parents=True)
target.write_text("#!/bin/sh\\nexit 0\\n")
target.chmod(0o755)
''')
        cargo.chmod(0o755)
        self.env["PATH"] = str(commands) + os.pathsep + self.env["PATH"]
        result = self.preflight()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("building Vuuut", result.stdout)
        self.assertTrue(os.access(binary, os.X_OK))

    def test_missing_custom_binary_fails_before_launch(self):
        self.config["paths"]["ghostBinary"] = str(self.base / "missing/ghost")
        result = self.preflight()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tenant CMS binary is missing", result.stderr)
        self.assertFalse((self.deployment / ".ghost-pro.pid").exists())

    def test_environment_and_environment_file_override_base_path(self):
        self.config["paths"]["ghostBinary"] = "/missing/base/ghost"
        (self.deployment / "pro.development.json").write_text(
            json.dumps({"paths": {"ghostBinary": "/bin/sh"}}))
        result = self.preflight()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.env["PRO_PATHS__GHOST_BINARY"] = "/missing/environment/ghost"
        result = self.preflight()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("/missing/environment/ghost", result.stderr)

    def prepare_server(self):
        if not BINARY.exists():
            self.skipTest("build ghost-pro --release for launcher smoke checks")
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.config["url"] = f"http://pro.localhost:{port}"
        self.config["server"] = {"host": "127.0.0.1", "port": port}
        return port

    def test_failed_launch_does_not_repeat_historical_database_errors(self):
        self.prepare_server()
        self.config["database"] = {"url": "postgres://invalid"}
        self.write_config()
        (self.deployment / "server.log").write_text("HISTORICAL DATABASE ERROR\n")
        result = subprocess.run([str(SCRIPT), "start"], env=self.env,
                                capture_output=True, text=True, timeout=20)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("output from this launch", result.stdout)
        self.assertNotIn("HISTORICAL DATABASE ERROR", result.stdout)
        self.assertFalse((self.deployment / ".ghost-pro.pid").exists())

    def test_server_survives_launcher_exit_and_opens_its_database(self):
        port = self.prepare_server()
        self.write_config()
        result = subprocess.run([str(SCRIPT), "start"], env=self.env,
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        pid = int((self.deployment / ".ghost-pro.pid").read_text())
        def stop():
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            for _ in range(100):
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.02)
        self.addCleanup(stop)
        request = urllib.request.Request(f"http://127.0.0.1:{port}/health",
                                         headers={"Host": f"pro.localhost:{port}"})
        with urllib.request.urlopen(request, timeout=5) as response:
            health = json.load(response)
        self.assertEqual(health["status"], "ok")
        self.assertTrue(health["database"])


if __name__ == "__main__":
    unittest.main()
