"""Container-local TCP listeners backed by private AGS Unix stream relays.

No network discovery, credentials, subprocess shell parsing, or protocol logging.
Listeners are bound before starting the user's command; conflicts fail closed.
"""
import json
import os
import selectors
import signal
import socket
import subprocess
import sys
import threading
import time


LIMIT = 32
BUFFER = 32768


def relay(client, path):
    with client, socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as upstream:
        upstream.settimeout(1)
        upstream.connect(path)
        sockets = (client, upstream)
        pending = [bytearray(), bytearray()]
        eof = [False, False]
        for sock in sockets:
            sock.setblocking(False)
        with selectors.DefaultSelector() as selector:
            while not all(eof) or any(pending):
                for index, sock in enumerate(sockets):
                    events = 0
                    if not eof[index] and len(pending[1 - index]) < BUFFER:
                        events |= selectors.EVENT_READ
                    if pending[index]:
                        events |= selectors.EVENT_WRITE
                    try:
                        selector.unregister(sock)
                    except KeyError:
                        pass
                    if events:
                        selector.register(sock, events, index)
                for key, mask in selector.select():
                    index = key.data
                    sock = sockets[index]
                    if mask & selectors.EVENT_WRITE:
                        try:
                            count = sock.send(pending[index])
                            if count == 0:
                                raise ConnectionError("closed stream")
                            del pending[index][:count]
                            if not pending[index] and eof[1 - index]:
                                sock.shutdown(socket.SHUT_WR)
                        except BlockingIOError:
                            pass
                    if mask & selectors.EVENT_READ:
                        try:
                            data = sock.recv(BUFFER - len(pending[1 - index]))
                        except BlockingIOError:
                            continue
                        if data:
                            pending[1 - index].extend(data)
                        else:
                            eof[index] = True
                            if not pending[1 - index]:
                                sockets[1 - index].shutdown(socket.SHUT_WR)


def serve(listener, path, failed):
    capacity = threading.BoundedSemaphore(LIMIT)

    def worker(client):
        try:
            relay(client, path)
        except OSError:
            pass  # A failed upstream closes this connection; never dump payloads.
        finally:
            client.close()
            capacity.release()

    try:
        while True:
            client, _ = listener.accept()
            if not capacity.acquire(blocking=False):
                client.close()
                continue
            try:
                threading.Thread(target=worker, args=(client,), daemon=True).start()
            except RuntimeError:
                client.close()
                capacity.release()
                raise
    except (OSError, RuntimeError):
        failed.set()


def main():
    with open(sys.argv[1]) as file:
        entries = json.load(file)
    listeners = []
    failed = threading.Event()
    child = None
    terminate_at = None
    payload_fds = tuple(range(3, 3 + int(sys.argv[2])))

    def stop(signum, _frame):
        nonlocal terminate_at
        if signum in (signal.SIGTERM, signal.SIGHUP):
            terminate_at = time.monotonic() + 2
        if child is not None:
            try:
                child.send_signal(signum)
            except ProcessLookupError:
                pass
        else:
            raise SystemExit(128 + signum)

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, stop)
    try:
        for entry in entries:
            family = socket.AF_INET6 if ":" in entry["host"] else socket.AF_INET
            listener = socket.socket(family, socket.SOCK_STREAM)
            listeners.append(listener)
            if family == socket.AF_INET6:
                listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            listener.bind((entry["host"], entry["port"]))
            listener.listen(LIMIT)
        for listener, entry in zip(listeners, entries):
            threading.Thread(target=serve, args=(listener, entry["socket"], failed), daemon=True).start()
        child = subprocess.Popen(sys.argv[3:], pass_fds=payload_fds)
        # Anonymous secret descriptors belong to the final-process bootstrap,
        # not to the long-lived TCP supervisor or any connection worker.
        for fd in payload_fds:
            os.close(fd)
        while True:
            try:
                result = child.wait(timeout=0.1)
                return result if result >= 0 else 128 - result
            except subprocess.TimeoutExpired:
                if failed.is_set() or (terminate_at is not None and time.monotonic() >= terminate_at):
                    child.terminate()
                    try:
                        child.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
                    return 1
    except (OSError, RuntimeError):
        print("[ags] host connection listener/command startup failed", file=sys.stderr)
        return 1
    finally:
        for listener in listeners:
            listener.close()


if __name__ == "__main__":
    sys.exit(main())
