#!/usr/bin/env python3
"""Regenerates src/analyzer/lint/tables.txt from the clj-kondo sources (var_info_gen.clj etc.).
Usage: python3 tools/gen_lint_tables.py <clj_kondo/impl dir>
Sections '@name' followed by one entry per line."""
import sys, os, re
impl = sys.argv[1]
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'src', 'analyzer', 'lint', 'tables.txt')
txt = open(impl + '/var_info_gen.clj').read()
def block(name):
    m = re.search(r"\(def %s '[#{]\{?(.*?)\}\)" % re.escape(name), txt, re.S)
    return m.group(1).split()
with open(out, 'w') as f:
    f.write('@unused-values\n' + '\n'.join(block('unused-values')) + '\n')
