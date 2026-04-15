"""
fake_bridge.py — Bridge sintético para smoke testing del bot.

Escucha en 127.0.0.1:9000 (como lo haría el bridge real en el PC gaming) y
responde los comandos ASCII del protocolo Pico:
  PING             → PONG\n
  MOUSE_MOVE x y   → OK\n
  MOUSE_CLICK b    → OK\n
  KEY_TAP 0xNN     → OK\n
  RESET            → OK\n
  (cualquier otro) → ERR unknown\n

Registra cada comando recibido en un log JSON-lines para verificación posterior.
"""

import json
import socket
import sys
import threading
import time
from pathlib import Path

HOST = "127.0.0.1"
PORT = 9000
LOG_PATH = Path(__file__).parent / "bridge.log"


def handle_client(conn: socket.socket, addr, log_file) -> None:
    conn.settimeout(5.0)
    buf = b""
    try:
        while True:
            data = conn.recv(4096)
            if not data:
                break
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                cmd = line.decode("ascii", errors="replace").strip()
                if not cmd:
                    continue
                reply = handle_command(cmd)
                ts = time.time()
                entry = {"ts": ts, "cmd": cmd, "reply": reply.strip()}
                log_file.write(json.dumps(entry) + "\n")
                log_file.flush()
                print(f"[{addr[0]}:{addr[1]}] {cmd!r} -> {reply.strip()!r}",
                      flush=True)
                conn.sendall(reply.encode("ascii"))
    except (ConnectionResetError, TimeoutError, BrokenPipeError):
        pass
    finally:
        conn.close()


def handle_command(cmd: str) -> str:
    head = cmd.split(" ", 1)[0].upper()
    if head == "PING":
        return "PONG\n"
    if head in ("MOUSE_MOVE", "MOUSE_CLICK", "KEY_TAP", "RESET"):
        return "OK\n"
    return f"ERR unknown: {cmd}\n"


def main() -> int:
    log_file = LOG_PATH.open("w", buffering=1)
    print(f"fake_bridge: logging to {LOG_PATH}", flush=True)

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((HOST, PORT))
    srv.listen(4)
    print(f"fake_bridge: escuchando en {HOST}:{PORT}", flush=True)

    try:
        while True:
            conn, addr = srv.accept()
            print(f"fake_bridge: cliente conectado {addr}", flush=True)
            t = threading.Thread(
                target=handle_client,
                args=(conn, addr, log_file),
                daemon=True,
            )
            t.start()
    except KeyboardInterrupt:
        print("fake_bridge: cerrando", flush=True)
    finally:
        srv.close()
        log_file.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
