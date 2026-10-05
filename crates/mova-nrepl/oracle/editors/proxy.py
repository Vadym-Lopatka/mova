#!/usr/bin/env python3
"""Recording TCP proxy for nREPL. usage: proxy.py LISTEN_PORT TARGET_PORT LOGFILE
Writes one JSON line per decoded message: {"dir":"c2s"|"s2c","msg":{...}}. Stops on SIGTERM."""
import socket, sys, threading, json, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import bencode

lp, tp, logf = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
lock = threading.Lock()
log = open(logf, "a", buffering=1)

def pump(src, dst, d):
    buf = b""
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
            buf += data
            while buf:
                try:
                    msg, n = bencode._decode(buf, 0)
                except bencode.Incomplete:
                    break
                buf = buf[n:]
                with lock:
                    log.write(json.dumps({"dir": d, "msg": msg}) + "\n")
    except OSError:
        pass
    finally:
        for s in (src, dst):
            try: s.shutdown(socket.SHUT_RDWR)
            except OSError: pass

srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", lp)); srv.listen(16)
while True:
    c, _ = srv.accept()
    s = socket.create_connection(("127.0.0.1", tp))
    threading.Thread(target=pump, args=(c, s, "c2s"), daemon=True).start()
    threading.Thread(target=pump, args=(s, c, "s2c"), daemon=True).start()
