#!/usr/bin/env python3
"""Deterministic messy-corpus generator: messy.py <in-root> <out-root> [seed].
Mangles whitespace outside strings/comments/char literals: randomizes or strips indentation, doubles spaces,
turns spaces into tabs/newlines/commas-free gaps, adds blank lines and trailing spaces, removes the space in `) (`,
and converts ~10% of files to CRLF."""
import os, random, re, sys, zlib

def mangle(text, rnd):
    out = []
    i, n = 0, len(text)
    in_str = False      # inside "..."
    at_bol = True       # at beginning of line (outside string)
    mode = rnd.choice(["strip", "random", "keep", "strip"])
    while i < n:
        c = text[i]
        if in_str:
            out.append(c)
            if c == "\\" and i + 1 < n:
                out.append(text[i + 1]); i += 2; continue
            if c == '"': in_str = False
            i += 1; continue
        if c == ";":                      # line comment: copy verbatim to EOL
            j = text.find("\n", i)
            j = n if j < 0 else j
            seg = text[i:j]
            if rnd.random() < 0.15: seg += "   "
            out.append(seg); i = j; at_bol = False; continue
        if c == "\\" and i + 1 < n:       # char literal
            out.append(text[i:i + 2]); i += 2; at_bol = False
            while i < n and re.match(r"[A-Za-z0-9]", text[i]) and text[i-1].isalnum():
                out.append(text[i]); i += 1
            continue
        if c == '"':
            in_str = True; out.append(c); i += 1; at_bol = False; continue
        if c == "\n":
            out.append(c)
            r = rnd.random()
            if r < 0.04: out.append("\n" * rnd.randint(1, 3))
            elif r < 0.08 and out and out[-1] == "\n": pass
            i += 1; at_bol = True; continue
        if c in " \t":
            j = i
            while j < n and text[j] in " \t": j += 1
            if at_bol:
                if mode == "strip": pass
                elif mode == "random": out.append(" " * rnd.randint(0, 14))
                else: out.append(text[i:j])
                i = j; continue
            nxt = text[j] if j < n else "\n"
            if nxt == "\n":
                if rnd.random() < 0.2: out.append(text[i:j] + "  ")
                else: out.append(text[i:j])
            else:
                r = rnd.random()
                if r < 0.10: out.append(text[i:j] + " " * rnd.randint(1, 3))
                elif r < 0.14: out.append("\t")
                elif r < 0.17 and len(text[i:j]) == 1 and out and out[-1][-1:] not in ";":
                    out.append("\n" + " " * rnd.randint(0, 6))
                elif r < 0.20 and out and out[-1][-1:] in ")]}" and nxt in "([{":
                    pass
                else: out.append(text[i:j])
            i = j; at_bol = False; continue
        out.append(c); i += 1; at_bol = False
    s = "".join(out)
    return s

def main():
    src, dst = sys.argv[1], sys.argv[2]
    seed = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    for root, _, files in os.walk(src):
        for f in sorted(files):
            if not re.search(r"\.(clj[cs]?|bb|edn)$", f): continue
            p = os.path.join(root, f)
            rel = os.path.relpath(p, src)
            rnd = random.Random(zlib.crc32(rel.encode()) ^ seed)
            with open(p, encoding="utf-8", errors="surrogateescape", newline="") as fh:
                t = fh.read()
            t = mangle(t.replace("\r\n", "\n"), rnd)
            if rnd.random() < 0.10: t = t.replace("\n", "\r\n")
            o = os.path.join(dst, rel)
            os.makedirs(os.path.dirname(o), exist_ok=True)
            with open(o, "w", encoding="utf-8", errors="surrogateescape", newline="") as fh:
                fh.write(t)
main()
