# Benchmarks

Recorded results from `tools/pick-bench` (see `bench/README.md` for how
to run it), kept so changes can be compared against them.

- `log.md`: one entry per measurement session, newest first: what was
  run, on which commit and machine, the numbers, and what they mean.
- `baseline-<git-sha>/`: full-matrix baselines (`manifest.json` +
  `iterations.jsonl` copied from `bench/results/<run-id>/`, without the
  `sandbox/` folder). Records contain timings, counts and machine
  details only: no window titles or page content.

Raw results in `bench/results/` are git-ignored; copy anything worth
keeping here.

When adding a log entry, include: date, app commit, harness command,
machine/power state, window size/DPI, then a table of p50 / p95 / CPU,
and a short "what this shows". Note anything that makes the run not
comparable with earlier ones (different pattern, profile, window size).
