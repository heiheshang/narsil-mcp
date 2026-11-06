#!/usr/bin/env python3
"""Paired before/after comparison of two narsil binaries on onec/dataset.json.

Runs search_chunks and hybrid_search of both binaries over the same module
subset as rerank_eval.py and compares them per tool: exact McNemar on
hit@1 / hit@k, sign-flip permutation on MRR@10. No reranking.

    .venv/bin/python -I onec/compare_binaries.py --repo ~/Downloads/ConfigFiles/CommonModules \
        --old ~/.local/bin/narsil-mcp --new ../../target/release/narsil-mcp
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from paired_stats import mcnemar_exact, paired_permutation
from rerank_eval import first_hit, retrieve

TOOLS = ("search_chunks", "hybrid_search")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", type=Path, required=True)
    ap.add_argument("--old", type=Path, required=True)
    ap.add_argument("--new", type=Path, required=True)
    ap.add_argument("--dataset", type=Path, default=Path(__file__).parent / "dataset.json")
    ap.add_argument("--pool", type=int, default=20)
    ap.add_argument("--k", type=int, default=5)
    ap.add_argument("--cache", type=Path, default=Path(__file__).parent / ".cache.compare.json")
    args = ap.parse_args()

    samples = json.loads(args.dataset.read_text())["samples"]
    cache = json.loads(args.cache.read_text()) if args.cache.exists() else {}
    for side in ("old", "new"):
        if side not in cache:
            print(f"Retrieving with {side} binary...", file=sys.stderr)
            ns = argparse.Namespace(repo=args.repo, binary=getattr(args, side), pool=args.pool)
            cache[side] = retrieve(ns, samples)
            args.cache.write_text(json.dumps(cache, ensure_ascii=False))

    n, k = len(samples), args.k
    for tool in TOOLS:
        ranks = {
            side: [first_hit(cache[side][s["id"]][tool], s["relevant"]) for s in samples]
            for side in ("old", "new")
        }

        def hits(side: str, at: int) -> list[bool]:
            return [r is not None and r <= at for r in ranks[side]]

        def rr(side: str) -> list[float]:
            return [1.0 / r if r is not None and r <= 10 else 0.0 for r in ranks[side]]

        print(f"\n{tool}: {n} queries")
        for side in ("old", "new"):
            secs = [cache[side][s["id"]][f"{tool}_seconds"] for s in samples]
            print(
                f"  {side}: hit@1 {sum(hits(side, 1)) / n:.3f}  hit@{k} {sum(hits(side, k)) / n:.3f}  "
                f"MRR@10 {statistics.mean(rr(side)):.3f}  recall@{args.pool} "
                f"{sum(hits(side, args.pool)) / n:.3f}  median {statistics.median(secs):.2f}s"
            )
        for at in (1, k, args.pool):
            m = mcnemar_exact(hits("old", at), hits("new", at))
            print(f"  hit@{at:<2} only_old={m.only_a:<3} only_new={m.only_b:<3} McNemar p={m.p_value:.4f}")
        p = paired_permutation(rr("old"), rr("new"))
        print(f"  MRR   diff={p.mean_diff:+.3f}  permutation p={p.p_value:.4f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
