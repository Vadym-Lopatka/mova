"""Minimal bencode for nREPL. Stdlib only.

Decoding keeps dict key order as seen on the wire (so key order can be checked).
Strings decode to str (UTF-8); undecodable byte strings become {"$bytes": hex}.
"""


class Incomplete(Exception):
    pass


def encode(obj) -> bytes:
    if isinstance(obj, bool):
        return b"i%de" % int(obj)
    if isinstance(obj, int):
        return b"i%de" % obj
    if isinstance(obj, str):
        b = obj.encode("utf-8")
        return b"%d:%s" % (len(b), b)
    if isinstance(obj, (bytes, bytearray)):
        return b"%d:%s" % (len(obj), bytes(obj))
    if isinstance(obj, (list, tuple)):
        return b"l" + b"".join(encode(x) for x in obj) + b"e"
    if isinstance(obj, dict):
        # bencode spec: keys sorted
        items = sorted(obj.items(), key=lambda kv: kv[0].encode("utf-8"))
        return b"d" + b"".join(encode(k) + encode(v) for k, v in items) + b"e"
    raise TypeError("cannot bencode %r" % (type(obj),))


def _decode(buf, i):
    n = len(buf)
    if i >= n:
        raise Incomplete
    c = buf[i]
    if c == 0x69:  # i
        j = buf.find(b"e", i)
        if j < 0:
            raise Incomplete
        return int(buf[i + 1:j]), j + 1
    if c == 0x6C:  # l
        i += 1
        out = []
        while True:
            if i >= n:
                raise Incomplete
            if buf[i] == 0x65:
                return out, i + 1
            v, i = _decode(buf, i)
            out.append(v)
    if c == 0x64:  # d
        i += 1
        out = {}
        while True:
            if i >= n:
                raise Incomplete
            if buf[i] == 0x65:
                return out, i + 1
            k, i = _decode(buf, i)
            v, i = _decode(buf, i)
            out[k if isinstance(k, str) else str(k)] = v
    if 0x30 <= c <= 0x39:
        j = buf.find(b":", i)
        if j < 0:
            raise Incomplete
        ln = int(buf[i:j])
        if j + 1 + ln > n:
            raise Incomplete
        raw = bytes(buf[j + 1:j + 1 + ln])
        try:
            return raw.decode("utf-8"), j + 1 + ln
        except UnicodeDecodeError:
            return {"$bytes": raw.hex()}, j + 1 + ln
    raise ValueError("bad bencode byte %r at %d" % (chr(c), i))


def decode_one(buf, i=0):
    """Return (obj, next_index). Raises Incomplete if buf has no full value."""
    return _decode(buf, i)


def decode_all(buf):
    out, i = [], 0
    while i < len(buf):
        v, i = _decode(buf, i)
        out.append(v)
    return out
