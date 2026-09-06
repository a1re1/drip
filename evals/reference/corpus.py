"""Where the o-cs gold data lives on this machine.

Both layers read their corpus and gold queries from an o-cs checkout. Rather
than hardcoding one person's path, resolve it the way the tool under test
resolves its roots — an explicit setting first, then the conventional
location — and fail with an instruction instead of a traceback when neither
is there.
"""

import os

#: Where an o-cs checkout is looked for, in order. `.worktrees/*` is included
#: because a linked git worktree is the usual working copy.
GOLD_MARKER = os.path.join("evals", "queries.jsonl")


def candidate_roots(home=None, environ=None):
    """Candidate o-cs roots, best first: $OCS_ROOT, ~/src/o-cs, its worktrees."""
    environ = os.environ if environ is None else environ
    home = home or os.path.expanduser("~")
    roots = []
    if environ.get("OCS_ROOT"):
        roots.append(environ["OCS_ROOT"])
    base = os.path.join(home, "src", "o-cs")
    roots.append(base)
    worktrees = os.path.join(base, ".worktrees")
    if os.path.isdir(worktrees):
        roots.extend(sorted(os.path.join(worktrees, name) for name in os.listdir(worktrees)))
    return roots


def pick_root(roots, has_gold):
    """The first candidate whose gold data is actually present, else None."""
    for root in roots:
        if has_gold(root):
            return root
    return None


def has_gold(root):
    return os.path.exists(os.path.join(root, GOLD_MARKER))


def ocs_root(home=None, environ=None):
    """The resolved o-cs checkout, or None when no candidate holds gold data."""
    return pick_root(candidate_roots(home=home, environ=environ), has_gold)


def default_corpus(root, environ=None):
    """The corpus directory to search: $DRIP_REFERENCE_ROOTS' first root wins.

    Only the first root is used — the benchmark scores one corpus at a time, so
    a multi-root export must be narrowed explicitly with --corpus rather than
    silently searching a different set than the numbers claim.
    """
    environ = os.environ if environ is None else environ
    configured = environ.get("DRIP_REFERENCE_ROOTS", "").split(":")[0].strip()
    if configured:
        return configured
    return os.path.join(root, "wiki") if root else ""


def missing_message(what):
    return (
        f"{what} not found. Pass an explicit path, or set OCS_ROOT to an o-cs checkout "
        f"(the one holding {GOLD_MARKER})."
    )
