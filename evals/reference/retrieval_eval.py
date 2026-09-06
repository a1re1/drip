"""Retrieval-benchmark helpers: compare raw oasis with drip's REFERENCE tool.

Pure helper layer — parsing, scoring, aggregation and table rendering only.
Importing this module performs no subprocess calls, no file access and no
execution; the CLI orchestration lives behind a ``__main__`` guard.

The two sides compared per gold query are:

  raw  — ``oasis --root <corpus> search "<q>" --json -k <k>``: a JSON array
         of hit objects whose order is the ranking.
  tool — ``cargo run --quiet --example reference_probe -- ...``: one JSON
         line ``{"query":…,"failed":bool,"paths":[…],"text":…}``.

A side failure (malformed JSON, wrong types, the probe reporting
``failed:true``) is recorded on that side's record; it is never folded into
a successful empty ranking. Ranked paths keep their order and duplicates
exactly — parity is judged on the full lists.
"""

from __future__ import annotations

import json
from typing import Any, Dict, Iterable, Optional, Sequence

import grade

#: Per-side metric keys, in table order.
METRIC_KEYS = ("recall@1", "recall@3", "recall@5", "recall@k", "mrr@k")


class SideError(ValueError):
    """A side produced unusable output — recorded as that side's failure."""


# --------------------------------------------------------------- parsing


def parse_raw_hits(stdout: str) -> list[str]:
    """Strictly parse raw ``oasis search --json`` stdout into ranked paths.

    The payload must be a JSON array of objects each carrying a non-empty
    string ``path``; hit order is the ranking. Anything else raises
    :class:`SideError`.
    """
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as error:
        raise SideError(f"oasis stdout is not valid JSON: {error}") from error
    if not isinstance(payload, list):
        raise SideError(
            f"oasis stdout must be a JSON array, got {type(payload).__name__}"
        )
    paths: list[str] = []
    for index, hit in enumerate(payload):
        if not isinstance(hit, dict):
            raise SideError(f"oasis hit #{index} is not a JSON object")
        path = hit.get("path")
        if not isinstance(path, str) or not path:
            raise SideError(f'oasis hit #{index} lacks a non-empty string "path"')
        paths.append(path)
    return paths


def parse_probe_line(stdout: str) -> list[str]:
    """Strictly parse reference_probe stdout (one JSON line) into paths.

    Expected shape: ``{"query":…,"failed":bool,"paths":[…],"text":…}``.
    ``failed: true`` is a side failure, as is any malformed structure; an
    empty ``paths`` list is a *successful* empty ranking.
    """
    lines = [line for line in stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        raise SideError(
            f"probe stdout must be exactly one non-blank JSON line, got {len(lines)}"
        )
    try:
        payload = json.loads(lines[0])
    except json.JSONDecodeError as error:
        raise SideError(f"probe stdout is not valid JSON: {error}") from error
    if not isinstance(payload, dict):
        raise SideError(
            f"probe stdout must be a JSON object, got {type(payload).__name__}"
        )
    failed = payload.get("failed")
    if not isinstance(failed, bool):
        raise SideError('probe JSON lacks a boolean "failed"')
    paths = payload.get("paths")
    if not isinstance(paths, list) or any(
        not isinstance(path, str) for path in paths
    ):
        raise SideError('probe JSON "paths" must be a list of strings')
    if failed:
        raise SideError("probe reported failed:true")
    return paths


# --------------------------------------------------------------- scoring


def score_paths(
    ranked: Sequence[str], relevant: Iterable[str], k: int
) -> Dict[str, float]:
    """Per-query metrics for one side: recall@1/3/5/k and MRR@k.

    MRR@k is the reciprocal rank computed over ``ranked[:k]`` only. Ranked
    order and duplicates are preserved — they matter for parity — while the
    grade functions deduplicate inside the recall numerators.
    """
    ranked = list(ranked)
    relevant = list(relevant)
    return {
        "recall@1": grade.recall_at_k(ranked, relevant, 1),
        "recall@3": grade.recall_at_k(ranked, relevant, 3),
        "recall@5": grade.recall_at_k(ranked, relevant, 5),
        "recall@k": grade.recall_at_k(ranked, relevant, k),
        "mrr@k": grade.reciprocal_rank(ranked[: max(k, 0)], relevant),
    }


def zero_scores() -> Dict[str, float]:
    """The all-zero score block a failed side contributes."""
    return {key: 0.0 for key in METRIC_KEYS}


def make_side(
    ok: bool,
    paths: Optional[Sequence[str]] = None,
    seconds: Optional[float] = None,
    error: Optional[str] = None,
) -> Dict[str, Any]:
    """One side's per-query outcome; success/error live apart from paths/seconds."""
    return {
        "ok": bool(ok),
        "paths": list(paths) if paths is not None else [],
        "seconds": seconds,
        "error": None if ok else (error or "unspecified failure"),
    }


def build_record(
    row_id: str,
    query: str,
    relevant: Iterable[str],
    raw: Dict[str, Any],
    tool: Dict[str, Any],
    k: int,
) -> Dict[str, Any]:
    """Pair the two sides for one gold query and score both.

    A failed side keeps its error and scores zero on every metric; a
    successful side scores over its ranked paths (an empty ranking also
    scores zero, but still counts as success).
    """
    record: Dict[str, Any] = {
        "id": row_id,
        "query": query,
        "relevant": list(relevant),
        "raw": dict(raw),
        "tool": dict(tool),
    }
    for side in ("raw", "tool"):
        outcome = record[side]
        outcome["paths"] = list(outcome.get("paths") or [])
        if outcome.get("ok"):
            outcome["error"] = None
            outcome["scores"] = score_paths(outcome["paths"], record["relevant"], k)
        else:
            outcome["error"] = outcome.get("error") or "unspecified failure"
            outcome["scores"] = zero_scores()
    return record


# ------------------------------------------------------------ aggregation


def percentile(values: Iterable[float], fraction: float) -> Optional[float]:
    """Deterministic percentile by linear interpolation over sorted values.

    position = ``fraction * (n - 1)``; the result is interpolated between the
    floor and ceil neighbours, so ``percentile(v, 0.5)`` is the median.
    Empty input yields ``None``.
    """
    items = sorted(values)
    if not items:
        return None
    if len(items) == 1:
        return float(items[0])
    position = fraction * (len(items) - 1)
    lower = int(position)
    upper = min(lower + 1, len(items) - 1)
    weight = position - lower
    return items[lower] * (1.0 - weight) + items[upper] * weight


def median(values: Iterable[float]) -> Optional[float]:
    """Median as the 0.5 percentile; ``None`` for empty input."""
    return percentile(values, 0.5)


def aggregate(records: Sequence[Dict[str, Any]], k: int) -> Dict[str, Any]:
    """Deterministic aggregate math over every sampled record.

    Metric averages run over *all* records, with failed sides contributing
    their zero scores, so a wiring loss lowers a side's numbers instead of
    shrinking the denominator. Latency percentiles use only successful
    sides' timings. Parity compares full ordered path lists (duplicates
    included) across both-successful pairs only — successful empty lists
    count as matching; with no comparable pairs parity is ``None`` and its
    denominator 0. ``delta`` is tool minus raw.
    """
    summary: Dict[str, Any] = {"k": k, "queries": len(records), "delta": {}}
    for side in ("raw", "tool"):
        ok = [record for record in records if record[side]["ok"]]
        latencies = [
            record[side]["seconds"]
            for record in ok
            if record[side]["seconds"] is not None
        ]
        summary[side] = {
            "metrics": {
                key: grade.mean([record[side]["scores"][key] for record in records])
                for key in METRIC_KEYS
            },
            "ok": len(ok),
            "failed": len(records) - len(ok),
            "latency": {
                "median": median(latencies),
                "p95": percentile(latencies, 0.95),
            },
        }
    for key in METRIC_KEYS:
        summary["delta"][key] = (
            summary["tool"]["metrics"][key] - summary["raw"]["metrics"][key]
        )
    comparable = [
        record for record in records if record["raw"]["ok"] and record["tool"]["ok"]
    ]
    matches = sum(
        1 for record in comparable if record["raw"]["paths"] == record["tool"]["paths"]
    )
    summary["parity"] = matches / len(comparable) if comparable else None
    summary["parity_comparable"] = len(comparable)
    summary["diverging_ids"] = [
        record["id"]
        for record in comparable
        if record["raw"]["paths"] != record["tool"]["paths"]
    ]
    return summary


# ------------------------------------------------------------ rendering


def _fmt_metric(value: Optional[float]) -> str:
    return "n/a" if value is None else f"{value:.4f}"


def _fmt_seconds(value: Optional[float]) -> str:
    return "n/a" if value is None else f"{value:.2f}s"


def render_table(summary: Dict[str, Any]) -> str:
    """Aligned comparison table for one run's aggregate summary.

    Metric rows (raw / tool / delta), then the parity line with up to five
    diverging query ids in sample order, then per-side median/p95 latency
    in seconds.
    """
    lines = [
        "REFERENCE retrieval — raw oasis vs REFERENCE tool "
        f"(k={summary['k']}, n={summary['queries']})"
    ]
    lines.append(f"{'metric':<10}{'raw':>10}{'tool':>10}{'delta':>10}")
    for key in METRIC_KEYS:
        lines.append(
            f"{key:<10}"
            f"{_fmt_metric(summary['raw']['metrics'][key]):>10}"
            f"{_fmt_metric(summary['tool']['metrics'][key]):>10}"
            f"{_fmt_metric(summary['delta'][key]):>10}"
        )
    parity = summary["parity"]
    comparable = summary["parity_comparable"]
    if parity is None:
        lines.append("parity: n/a (0 comparable queries)")
    else:
        lines.append(f"parity: {parity:.4f} ({comparable} comparable queries)")
    diverging = summary["diverging_ids"][:5]
    if diverging:
        lines.append("diverging ids: " + ", ".join(str(i) for i in diverging))
    for side in ("raw", "tool"):
        latency = summary[side]["latency"]
        counts = f"{summary[side]['ok']} ok / {summary[side]['failed']} failed"
        lines.append(
            f"{side} latency: median {_fmt_seconds(latency['median'])}  "
            f"p95 {_fmt_seconds(latency['p95'])}  ({counts})"
        )
    return "\n".join(lines)

# ----------------------------------------------------------- orchestration
#
# Everything below runs only as a script: importing this module must stay
# free of subprocess calls and file access (test_grade.py imports it).


def _load_queries(path, limit, seed):
    """A deterministic sample of the gold query rows."""
    import json as _json
    import random

    rows = []
    with open(path) as handle:
        for line in handle:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            rows.append(_json.loads(line))
    if limit and limit < len(rows):
        rows = random.Random(seed).sample(rows, limit)
    return rows


def _run(command, timeout):
    """Run one retrieval command; return (stdout, seconds, error_or_None)."""
    import subprocess
    import time

    started = time.monotonic()
    try:
        completed = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return "", time.monotonic() - started, f"timed out after {timeout}s"
    seconds = time.monotonic() - started
    if completed.returncode != 0:
        return completed.stdout, seconds, f"exit {completed.returncode}: {completed.stderr.strip()[:200]}"
    return completed.stdout, seconds, None


def _side(command, parse, timeout):
    """One side's outcome for one query, via its own strict parser."""
    stdout, seconds, error = _run(command, timeout)
    if error:
        return make_side(False, seconds=seconds, error=error)
    try:
        return make_side(True, paths=parse(stdout), seconds=seconds)
    except (SideError, ValueError) as failure:
        return make_side(False, seconds=seconds, error=str(failure))


def _query_record(row, options, repo):
    """Retrieve one gold query both ways and score the pair."""
    raw_command = ["oasis", "--root", options.corpus, "search", row["query"], "--json", "-k", str(options.k)]
    if options.mode == "lexical":
        raw_command.append("--lexical-only")
    tool_command = [
        "cargo", "run", "--quiet", "--example", "reference_probe", "--",
        "--root", options.corpus, "--k", str(options.k), "--mode", options.mode, row["query"],
    ]

    return build_record(
        row.get("id", row["query"]),
        row["query"],
        row.get("relevant") or [],
        _side(raw_command, parse_raw_hits, options.timeout),
        _side(_with_cwd(tool_command, repo), parse_probe_line, options.timeout),
        options.k,
    )


def _with_cwd(command, cwd):
    """cargo must run inside the repo whatever directory the operator is in."""
    return ["env", "-C", cwd] + command if cwd else command


def _sha1(path):
    import hashlib

    with open(path, "rb") as handle:
        return hashlib.sha1(handle.read()).hexdigest()


def _command_output(command, cwd):
    import subprocess

    try:
        return subprocess.run(command, cwd=cwd, capture_output=True, text=True, timeout=30).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def _main(argv=None):
    import argparse
    import concurrent.futures
    import datetime as dt
    import json as _json
    import os
    import shutil
    import subprocess
    import sys

    import corpus

    here = os.path.dirname(os.path.abspath(__file__))
    repo = os.path.dirname(os.path.dirname(here))
    ocs = corpus.ocs_root()

    parser = argparse.ArgumentParser(description="Compare raw oasis retrieval with drip's REFERENCE tool.")
    parser.add_argument("--corpus", default=corpus.default_corpus(ocs))
    parser.add_argument("--queries", default=os.path.join(ocs, "evals", "queries.jsonl") if ocs else "")
    parser.add_argument("--limit", type=int, default=100, help="sampled queries (0 = all)")
    parser.add_argument("--seed", type=int, default=0, help="sampling seed, for reproducible runs")
    parser.add_argument("--k", type=int, default=10)
    parser.add_argument("--mode", choices=("hybrid", "lexical"), default="lexical")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--timeout", type=int, default=180, help="seconds per retrieval call")
    parser.add_argument("--json", action="store_true", dest="as_json")
    options = parser.parse_args(argv)

    if not options.corpus or not os.path.isdir(options.corpus):
        print(corpus.missing_message(f"corpus ({options.corpus or 'unset'})"), file=sys.stderr)
        return 2
    if not options.queries or not os.path.exists(options.queries):
        print(corpus.missing_message(f"queries file ({options.queries or 'unset'})"), file=sys.stderr)
        return 2
    for binary in ("oasis", "cargo"):
        if shutil.which(binary) is None:
            print(f"the `{binary}` binary is not on PATH", file=sys.stderr)
            return 2

    # Build once: otherwise the first query pays for the whole example build.
    build = subprocess.run(
        ["cargo", "build", "--quiet", "--example", "reference_probe"],
        cwd=repo, capture_output=True, text=True,
    )
    if build.returncode != 0:
        print(f"cargo build --example reference_probe failed:\n{build.stderr.strip()[:800]}", file=sys.stderr)
        return 2

    rows = _load_queries(options.queries, options.limit, options.seed)
    if not rows:
        print("no queries to run", file=sys.stderr)
        return 2

    records = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, options.jobs)) as pool:
        futures = [pool.submit(_query_record, row, options, repo) for row in rows]
        for future in concurrent.futures.as_completed(futures):
            records.append(future.result())
    records.sort(key=lambda record: str(record["id"]))

    summary = aggregate(records, options.k)
    run = {
        "at": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
        "layer": "retrieval",
        "git_sha": _command_output(["git", "rev-parse", "HEAD"], repo),
        "drip_version": _command_output(["drip", "--version"], repo),
        "corpus": options.corpus,
        "queries_file": options.queries,
        "queries_file_sha": _sha1(options.queries),
        "limit": options.limit,
        "seed": options.seed,
        "k": options.k,
        "mode": options.mode,
        "summary": summary,
    }

    results = os.path.join(here, "results")
    os.makedirs(results, exist_ok=True)
    with open(os.path.join(results, "latest-retrieval.json"), "w") as handle:
        _json.dump({**run, "records": records}, handle, indent=2, sort_keys=True)
    with open(os.path.join(results, "history.jsonl"), "a") as handle:
        handle.write(_json.dumps(run, sort_keys=True) + "\n")

    if options.as_json:
        print(_json.dumps(run, indent=2, sort_keys=True))
    else:
        print(render_table(summary))
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
