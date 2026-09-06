"""Pure metric functions for the REFERENCE retrieval benchmark.

Standard library only, no I/O, no subprocess. Every function returns a sane
value (0.0 unless noted) for empty inputs instead of raising, so scoring a
failed side can never crash the harness.
"""

from __future__ import annotations

from typing import Iterable, Sequence


def recall_at_k(ranked: Sequence[str], relevant: Iterable[str], k: int) -> float:
    """Fraction of unique relevant pages present in the top-k ranked paths.

    Relevant identities and retrieved hits are deduplicated in the numerator
    (a duplicate hit never counts twice), but ranked positions are not
    compacted: only ``ranked[:k]`` is considered. ``k <= 0`` yields 0.0.
    """
    unique_relevant = set(relevant)
    if not unique_relevant or k <= 0:
        return 0.0
    top = ranked[:k]
    hits = {path for path in top if path in unique_relevant}
    return len(hits) / len(unique_relevant)


def reciprocal_rank(ranked: Sequence[str], relevant: Iterable[str]) -> float:
    """Reciprocal of the 1-based rank of the first relevant page, else 0.0."""
    unique_relevant = set(relevant)
    for index, path in enumerate(ranked, start=1):
        if path in unique_relevant:
            return 1.0 / index
    return 0.0


def _slug(path: str) -> str:
    """Page slug: basename without extension (``a/b/name.md`` -> ``name``)."""
    base = path.rstrip("/").rsplit("/", 1)[-1]
    stem, dot, _extension = base.partition(".")
    return stem if dot else base


def citation_hit(cited: object, expected: object) -> float:
    """1.0 when the citation and an expected page share a slug, else 0.0.

    Either argument may be a single path string or a collection of paths.
    Strings are treated as one path each (never iterated as characters), and
    matching ignores directories entirely.
    """
    cited_items = [cited] if isinstance(cited, str) else list(cited or [])
    expected_items = [expected] if isinstance(expected, str) else list(expected or [])
    expected_slugs = {_slug(str(item)) for item in expected_items}
    return 1.0 if any(_slug(str(item)) in expected_slugs for item in cited_items) else 0.0


def point_recall(answer_text: str, expect_points: Iterable[str]) -> float:
    """Fraction of expected points appearing as case-insensitive substrings.

    Empty expectations yield 0.0.
    """
    points = list(expect_points)
    if not points:
        return 0.0
    haystack = (answer_text or "").lower()
    matched = sum(1 for point in points if str(point).lower() in haystack)
    return matched / len(points)


def mean(values: Iterable[float]) -> float:
    """Arithmetic mean; 0.0 for an empty iterable."""
    items = list(values)
    if not items:
        return 0.0
    return sum(items) / len(items)
