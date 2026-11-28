#!/usr/bin/env python3
"""Summarise jumpkey telemetry: where the time goes inside each press.

pick-bench measures hotkey -> overlay visible from the outside; the
telemetry build records milestones inside jumpkey (docs/TELEMETRY.md).
This script turns those records into p50 / p95 per interval, per build
and (for pick-bench runs) per scope / scenario / pattern, and compares
two builds with bootstrap confidence intervals.

Usage:
    python scripts/telemetry_summary.py <run-dir-or-jsonl> [...]
        [--kind element|window] [--outcome picked|cancelled|...]
        [--compare a b] [--iters 2000]

A pick-bench run folder (bench/results/<run>) is read whole: telemetry
from every sandbox, joined to the harness's presses in iterations.jsonl
by QPC time (both use the system-wide QueryPerformanceCounter), so each
press gets its scenario. Plain telemetry .jsonl files are summarised
without that join, grouped by file.

Schemas 1-3 are understood. The milestone names never change; schema 2
moves the search to a worker thread (root cpu is main-thread only) and
schema 3 redefines `shown` as "shown without taking focus". Standard
library only (Python 3.8+). Prints numbers only: no titles or names
exist in the records to print.
"""

import argparse
import json
import random
import sys
from collections import defaultdict
from pathlib import Path

INTERVALS = [
    ("hotkey", "enum_start"),
    ("enum_start", "elements_ready"),
    ("elements_ready", "labels_ready"),
    ("labels_ready", "first_ulw"),
    ("first_ulw", "shown"),
    ("hotkey", "shown"),
    ("key_complete", "action_done"),
]
SPANS = ["enumerate", "sort", "labels", "overlay_state", "render", "show_focus", "show", "perform"]
# Root attributes worth a distribution when present (newer schemas add some).
NUMERIC_ATTRS = ["queue_ms", "search_queue_us", "reply_us", "hook_install_us", "count", "researches"]
KNOWN_SCHEMAS = {1, 2, 3}


def pct(values, q):
    v = sorted(values)
    if not v:
        return float("nan")
    k = (len(v) - 1) * q
    lo, hi = int(k), min(int(k) + 1, len(v) - 1)
    return v[lo] + (v[hi] - v[lo]) * (k - lo)


def read_jsonl(path):
    out = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line:
                try:
                    out.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
    return out


def load_telemetry(path):
    """(qpc_hz, [pick records]) for one telemetry file."""
    qpc_hz = None
    picks = []
    warned = False
    for rec in read_jsonl(path):
        schema = rec.get("schema")
        if schema not in KNOWN_SCHEMAS and not warned:
            print(f"note: {path.name}: unknown schema {schema}; reading anyway", file=sys.stderr)
            warned = True
        if rec.get("record") == "session":
            qpc_hz = rec.get("qpc_hz")
        elif rec.get("record") == "pick":
            picks.append(rec)
    return qpc_hz, picks


def milestones(rec):
    return {e["name"]: e["t_us"] for e in rec.get("events", [])}


def hotkey_qpc(rec, qpc_hz):
    """Absolute QPC of the `hotkey` milestone (or the handler start)."""
    t = milestones(rec).get("hotkey", 0)
    return rec["start_qpc"] + int(t * qpc_hz / 1e6)


def features(rec):
    """Every number this script summarises for one pick, in ms."""
    m = milestones(rec)
    f = {}
    for a, b in INTERVALS:
        if a in m and b in m:
            f[f"{a} -> {b}"] = (m[b] - m[a]) / 1000.0
    for s in rec.get("spans", []):
        if s.get("parent") == 0 and s.get("name") in SPANS:
            key = f"span {s['name']}"
            f[key] = f.get(key, 0.0) + s["dur_us"] / 1000.0
    attrs = rec.get("attributes", {})
    for a in NUMERIC_ATTRS:
        if isinstance(attrs.get(a), (int, float)):
            f[f"attr {a}"] = attrs[a] / 1000.0 if a.endswith("_us") else float(attrs[a])
    if isinstance(rec.get("cpu_us"), (int, float)):
        f["root cpu (main thread from schema 2)"] = rec["cpu_us"] / 1000.0
    return f


def wanted(rec, args):
    a = rec.get("attributes", {})
    if args.kind and a.get("kind") != args.kind:
        return False
    if args.outcome and a.get("outcome") != args.outcome:
        return False
    return not rec.get("incomplete", False)


def from_run_dir(run, args):
    """{group key: [feature dicts]} for a pick-bench run folder."""
    iters = [r for r in read_jsonl(run / "iterations.jsonl") if r.get("kind") == "iteration"]
    picks = []  # (hotkey_qpc, build, rec)
    for tel in sorted(run.glob("sandbox/*/localappdata/jumpkey/telemetry/telemetry*.jsonl")):
        build = tel.parts[-5].split("-")[0]
        qpc_hz, recs = load_telemetry(tel)
        if not qpc_hz:
            continue
        for rec in recs:
            if wanted(rec, args):
                picks.append((hotkey_qpc(rec, qpc_hz), build, rec, qpc_hz))
    picks.sort(key=lambda p: p[0])
    groups = defaultdict(list)
    used = set()
    matched = 0
    for it in iters:
        q0 = it["qpc_hotkey"]
        best = None
        for i, (q, build, rec, hz) in enumerate(picks):
            if i in used or build != it["build"]:
                continue
            # The hotkey message arrives within ~1 s of the send.
            if q0 <= q <= q0 + hz:
                best = i
                break
        if best is None:
            continue
        used.add(best)
        matched += 1
        key = (it["scope"], it["scenario"], it["pattern"], it["build"])
        f = features(picks[best][2])
        if it.get("hotkey_to_visible_us") is not None:
            f["harness hotkey -> visible"] = it["hotkey_to_visible_us"] / 1000.0
        if it.get("main_block_us_max") is not None:
            f["harness main-thread stall max"] = it["main_block_us_max"] / 1000.0
        groups[key].append(f)
    print(f"{run.name}: {matched} of {len(iters)} presses matched to telemetry")
    return groups


def from_file(path, args):
    _, recs = load_telemetry(path)
    return {(path.name, "", "", ""): [features(r) for r in recs if wanted(r, args)]}


def print_groups(groups):
    for key in sorted(groups):
        rows = groups[key]
        if not rows:
            continue
        print("\n== " + " / ".join(k for k in key if k) + f"  (n={len(rows)})")
        names = sorted({n for r in rows for n in r}, key=sort_key)
        print(f"   {'interval (ms)':<42} {'n':>4} {'p50':>9} {'p95':>9} {'max':>9}")
        for n in names:
            v = [r[n] for r in rows if n in r]
            print(f"   {n:<42} {len(v):>4} {pct(v, .5):>9.2f} {pct(v, .95):>9.2f} {max(v):>9.2f}")


def sort_key(name):
    order = [f"{a} -> {b}" for a, b in INTERVALS]
    if name in order:
        return (0, order.index(name))
    return (1, name)


def bootstrap_diff(a, b, iters, rng):
    """p50(b) - p50(a) with a 95% bootstrap interval."""
    d = []
    for _ in range(iters):
        ra = [rng.choice(a) for _ in a]
        rb = [rng.choice(b) for _ in b]
        d.append(pct(rb, .5) - pct(ra, .5))
    return pct(b, .5) - pct(a, .5), pct(d, .025), pct(d, .975)


def compare(groups, a, b, iters):
    rng = random.Random(1)
    by_rest = defaultdict(dict)
    for key, rows in groups.items():
        by_rest[key[:3]][key[3]] = rows
    print(f"\nA/B: p50({b}) - p50({a}), ms, 95% bootstrap interval; * = interval excludes 0")
    for rest in sorted(by_rest):
        builds = by_rest[rest]
        if a not in builds or b not in builds:
            continue
        print("\n== " + " / ".join(k for k in rest if k))
        names = sorted({n for r in builds[a] + builds[b] for n in r}, key=sort_key)
        for n in names:
            va = [r[n] for r in builds[a] if n in r]
            vb = [r[n] for r in builds[b] if n in r]
            if len(va) < 3 or len(vb) < 3:
                continue
            d, lo, hi = bootstrap_diff(va, vb, iters, rng)
            star = "*" if lo > 0 or hi < 0 else " "
            print(f"   {n:<42} {d:>+9.2f}  [{lo:>+8.2f}, {hi:>+8.2f}] {star}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("paths", nargs="+", type=Path)
    p.add_argument("--kind", choices=["element", "window"])
    p.add_argument("--outcome")
    p.add_argument("--compare", nargs=2, metavar=("A", "B"), help="build labels, e.g. a b")
    p.add_argument("--iters", type=int, default=2000, help="bootstrap resamples")
    args = p.parse_args()
    groups = defaultdict(list)
    for path in args.paths:
        if path.is_dir() and (path / "iterations.jsonl").is_file():
            parts = from_run_dir(path, args)
        elif path.is_file():
            parts = from_file(path, args)
        else:
            print(f"error: {path} is neither a pick-bench run folder nor a .jsonl file", file=sys.stderr)
            return 2
        for k, v in parts.items():
            groups[k].extend(v)
    print_groups(groups)
    if args.compare:
        compare(groups, args.compare[0], args.compare[1], args.iters)
    return 0


if __name__ == "__main__":
    sys.exit(main())
