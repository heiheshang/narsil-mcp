"""Paired significance tests for comparing two retrieval systems on the same queries.

Mean P@k/MRR differences on a few dozen queries are mostly noise; these tests
answer "would this difference survive a different sample of queries".
"""

from __future__ import annotations

import random
from dataclasses import dataclass
from math import comb


@dataclass(frozen=True)
class McNemar:
    """Exact McNemar test on per-query binary outcomes (e.g. hit@k)."""

    only_a: int  # queries A got right and B got wrong
    only_b: int  # queries B got right and A got wrong
    p_value: float


def mcnemar_exact(a: list[bool], b: list[bool]) -> McNemar:
    if len(a) != len(b):
        raise ValueError("paired outcomes must have equal length")
    only_a = sum(1 for x, y in zip(a, b) if x and not y)
    only_b = sum(1 for x, y in zip(a, b) if y and not x)
    n = only_a + only_b
    if n == 0:
        return McNemar(0, 0, 1.0)
    # Two-sided binomial test of the discordant pairs against p = 0.5.
    tail = sum(comb(n, i) for i in range(min(only_a, only_b) + 1)) / 2**n
    return McNemar(only_a, only_b, min(1.0, 2 * tail))


@dataclass(frozen=True)
class Permutation:
    """Paired sign-flip permutation test on per-query scores (e.g. reciprocal rank)."""

    mean_diff: float  # mean(b - a)
    p_value: float


def paired_permutation(
    a: list[float], b: list[float], iters: int = 20000, seed: int = 0
) -> Permutation:
    if len(a) != len(b):
        raise ValueError("paired scores must have equal length")
    diffs = [y - x for x, y in zip(a, b)]
    n = len(diffs)
    if n == 0:
        return Permutation(0.0, 1.0)
    observed = abs(sum(diffs))
    rng = random.Random(seed)
    extreme = 0
    for _ in range(iters):
        s = sum(d if rng.random() < 0.5 else -d for d in diffs)
        if abs(s) >= observed - 1e-12:
            extreme += 1
    # +1 smoothing: the observed assignment is one of the permutations.
    return Permutation(sum(diffs) / n, (extreme + 1) / (iters + 1))
