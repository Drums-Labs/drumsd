"""Host-side TCP forwarder into a slot's private network namespace.

    python -m sharedos_rootfs.proxy --marker M --init-pid PID --listen-port HOSTPORT --target-port 5000

The guest server listens inside the slot's net namespace, unreachable from the host. For each client this forwarder
opens the backend connection from a short-lived THREAD that has setns()'d into the slot's net namespace (setns is
per-thread), so the process itself stays in the host namespace. Needs CAP_SYS_ADMIN (root).
"""

from __future__ import annotations

import argparse
import os
import socket
import sys
import threading


def _has_setns() -> bool:
    return hasattr(os, "setns")   # Python 3.12+; tests patch this function


def pump(src, dst) -> None:
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass
    finally:
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def netns_connector(init_pid: int, port: int, proc: str = "/proc", setns=None, timeout: float = 10.0):
    setns = setns or os.setns
    newnet = getattr(os, "CLONE_NEWNET", 0x40000000)

    def connect() -> socket.socket:
        box: dict = {}

        def work():
            fd = None
            try:
                fd = os.open(f"{proc}/{init_pid}/ns/net", os.O_RDONLY)
                setns(fd, newnet)     # affects only this thread
                box["sock"] = socket.create_connection(("127.0.0.1", port), timeout=timeout)
            except BaseException as e:   # reported to the caller, which closes the client
                box["err"] = e
            finally:
                if fd is not None:
                    os.close(fd)
        t = threading.Thread(target=work)
        t.start()
        t.join()
        if "err" in box:
            raise OSError(f"cannot connect into netns of pid {init_pid} port {port}: {box['err']}")
        box["sock"].settimeout(None)
        return box["sock"]
    return connect


class Forwarder:
    def __init__(self, listen_port: int, connector, host: str = "127.0.0.1"):
        self.connector = connector
        self.srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.srv.bind((host, listen_port))
        self.srv.listen(64)
        self.port = self.srv.getsockname()[1]
        self._stop = False

    def _handle(self, client: socket.socket) -> None:
        try:
            backend = self.connector()
        except OSError:
            client.close()
            return
        t = threading.Thread(target=pump, args=(client, backend), daemon=True)
        t.start()
        pump(backend, client)
        t.join(timeout=5)
        client.close()
        backend.close()

    def serve_forever(self) -> None:
        while not self._stop:
            try:
                c, _ = self.srv.accept()
            except OSError:
                return
            threading.Thread(target=self._handle, args=(c,), daemon=True).start()

    def close(self) -> None:
        self._stop = True
        self.srv.close()


def main(argv=None) -> int:
    if not _has_setns():
        print("os.setns needs Python 3.12+: refusing to run (the proxy cannot enter the slot's namespace)", file=sys.stderr)
        return 2
    ap = argparse.ArgumentParser()
    ap.add_argument("--marker", required=True)
    ap.add_argument("--init-pid", type=int, required=True)
    ap.add_argument("--listen-port", type=int, required=True)
    ap.add_argument("--target-port", type=int, required=True)
    a = ap.parse_args(argv)
    Forwarder(a.listen_port, netns_connector(a.init_pid, a.target_port)).serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
