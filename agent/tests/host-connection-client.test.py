"""Real socket/process tests; no Podman or host credentials needed."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import unittest

CLIENT = Path(__file__).resolve().parents[1] / "host-connection-client.py"


class ClientTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def config(self, port, host="127.0.0.1"):
        path = self.root / "config.json"
        path.write_text(json.dumps([{"host": host, "port": port, "socket": str(self.root / "upstream")}]))
        return path

    def free_port(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            return listener.getsockname()[1]

    def command(self, port, code):
        return [sys.executable, "-I", str(CLIENT), str(self.config(port)), "0", sys.executable, "-c", code]

    def test_binary_stream_backpressure_half_close_and_exit_status(self):
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(self.root / "upstream"))
            listener.listen(1)
            listener.settimeout(5)
            errors = []

            def upstream():
                try:
                    stream, _ = listener.accept()
                    with stream:
                        stream.settimeout(5)
                        received = bytearray()
                        while True:
                            chunk = stream.recv(8192)
                            if not chunk:
                                break
                            received.extend(chunk)
                        self.assertEqual(received, b"\x00\xff" * 500000)
                        stream.sendall(b"\xff\x00" * 500000)
                except Exception as error:
                    errors.append(error)

            worker = threading.Thread(target=upstream)
            worker.start()
            port = self.free_port()
            code = f"""
import socket
with socket.create_connection(('127.0.0.1', {port}), timeout=5) as s:
    s.sendall(b'\\x00\\xff' * 500000)
    s.shutdown(socket.SHUT_WR)
    data = bytearray()
    while chunk := s.recv(8192): data.extend(chunk)
    assert data == b'\\xff\\x00' * 500000
raise SystemExit(17)
"""
            result = subprocess.run(self.command(port, code), capture_output=True, timeout=10)
            worker.join(timeout=6)
            self.assertFalse(worker.is_alive())
            self.assertEqual(errors, [])
            self.assertEqual(result.returncode, 17, result.stderr)

    def test_explicit_payload_descriptor_reaches_child(self):
        config = self.root / "config.json"
        config.write_text("[]")
        with tempfile.TemporaryFile() as payload:
            payload.write(b"descriptor-fixture")
            payload.seek(0)
            bootstrap = "import os,sys; os.dup2(int(sys.argv[1]),3); os.set_inheritable(3, True); os.execv(sys.executable, [sys.executable, *sys.argv[2:]])"
            command = [sys.executable, "-c", bootstrap, str(payload.fileno()), "-I", str(CLIENT), str(config), "1", sys.executable, "-c", "import os; assert os.read(3, 100)==b'descriptor-fixture'"]
            result = subprocess.run(command, pass_fds=(payload.fileno(),), capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_listener_conflict_does_not_start_command(self):
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", 0))
            occupied.listen()
            port = occupied.getsockname()[1]
            result = subprocess.run(self.command(port, "print('MUST NOT EXECUTE')"), capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 1)
            self.assertNotIn(b"MUST NOT EXECUTE", result.stdout)

    def test_termination_reaps_command_and_releases_listener(self):
        port = self.free_port()
        command = self.command(port, "import time; print('ready', flush=True); time.sleep(60)")
        with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
            try:
                self.assertEqual(process.stdout.readline(), b"ready\n")
                process.terminate()
                process.communicate(timeout=5)
                self.assertEqual(process.returncode, 143)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", port))


if __name__ == "__main__":
    unittest.main()
