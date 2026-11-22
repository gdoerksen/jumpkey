#!/usr/bin/env python3
"""Compare the elements two jumpkey builds found on the same window.

Input: the `walks.jsonl` files written by a telemetry build run with
`JUMPKEY_DUMP_WALK` set (see bench/README.md). Each line is one walk:
exe basename, counts, the window rect, and each element's role + rect.
No names, titles or URLs are in the file.

    python scripts/walk_diff.py A/walks.jsonl B/walks.jsonl [--exe vivaldi.exe]

For each file the walks of the chosen exe are grouped by the set of
elements they found (rects relative to the window's top-left corner, so
a moved window still compares equal). The most common set stands for
that build; the script prints what B is missing and what it added
compared with A, plus visited / collected counts.

Exit status: 0 if B is missing nothing, 1 if it is, 2 on bad input.
Python 3.8+, standard library only.
"""

import argparse
import json
import statistics
import sys
from collections import Counter


def load(path):
    walks = []
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                walks.append(json.loads(line))
            except json.JSONDecodeError as e:
                sys.exit(f"{path}:{n}: not JSON ({e})")
    return walks


def pick_exe(walks):
    """The most common exe among browser walks, else among all walks."""
    browser = Counter(w.get("exe") for w in walks if w.get("is_browser"))
    counts = browser or Counter(w.get("exe") for w in walks)
    return counts.most_common(1)[0][0] if counts else None


def signature(walk):
    """Multiset of (role, x, y, w, h) relative to the window origin."""
    ox, oy = (walk.get("window") or [0, 0, 0, 0])[:2]
    return frozenset(
        Counter(
            (e["role"], e["rect"][0] - ox, e["rect"][1] - oy, e["rect"][2], e["rect"][3])
            for e in walk.get("elements", [])
        ).items()
    )


def summarize(label, walks):
    sigs = Counter(signature(w) for w in walks)
    modal, modal_n = sigs.most_common(1)[0]
    visited = [w.get("visited", 0) for w in walks]
    collected = [w.get("collected", 0) for w in walks]
    capped = sum(1 for w in walks if w.get("hit_element_cap"))
    print(
        f"{label}: {len(walks)} walks, {len(sigs)} distinct element sets "
        f"(most common seen {modal_n}x); visited median {statistics.median(visited):.0f}, "
        f"collected median {statistics.median(collected):.0f}"
        + (f"; element cap hit {capped}x" if capped else "")
    )
    return Counter(dict(modal))


def fmt(item):
    (role, x, y, w, h), n = item
    return f"  {role:<10} at ({x}, {y}) size {w}x{h}" + (f"  x{n}" if n > 1 else "")


def main():
    ap = argparse.ArgumentParser(description="Compare the elements two jumpkey builds found.")
    ap.add_argument("a", help="walks.jsonl from the reference build (e.g. main)")
    ap.add_argument("b", help="walks.jsonl from the build under test")
    ap.add_argument("--exe", help="only walks of this exe (default: most common browser exe in A)")
    args = ap.parse_args()

    a_all, b_all = load(args.a), load(args.b)
    exe = args.exe or pick_exe(a_all)
    a = [w for w in a_all if w.get("exe") == exe]
    b = [w for w in b_all if w.get("exe") == exe]
    if not a or not b:
        print(f"no walks of {exe!r} in {'A' if not a else 'B'}", file=sys.stderr)
        return 2
    print(f"exe: {exe}")
    sa = summarize("A", a)
    sb = summarize("B", b)

    missing = sorted((sa - sb).items())
    added = sorted((sb - sa).items())
    print(f"\nmissing in B: {sum(n for _, n in missing)}")
    for item in missing:
        print(fmt(item))
    print(f"added in B: {sum(n for _, n in added)}")
    for item in added:
        print(fmt(item))
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
