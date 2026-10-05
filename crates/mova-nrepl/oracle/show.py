#!/usr/bin/env python3
"""Pretty-print a golden: show.py NAME [maxchars]"""
import json, sys, glob, os
n = sys.argv[1]; mx = int(sys.argv[2]) if len(sys.argv) > 2 else 300
p = glob.glob(os.path.join(os.path.dirname(os.path.abspath(__file__)), "goldens", n + "*.json"))[0]
for e in json.load(open(p))["messages"]:
    s = json.dumps(e["msg"], ensure_ascii=False)
    print((e["conn"] + " " if e["conn"] != "main" else "") + (s if len(s) <= mx else s[:mx] + "...[%d]" % len(s)))
