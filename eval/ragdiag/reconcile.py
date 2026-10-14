#!/usr/bin/env python3
"""Verify RAGDiag metrics match direct narsil-mcp MCP retrieval on the same queries."""

from __future__ import annotations

import json
import sys
from pathlib import Path

_EVAL = Path(__file__).resolve().parent
_ROOT = _EVAL.parents[1]
sys.path.insert(0, str(_EVAL))

from ragdiag.dataset.loader import load_dataset
from ragdiag.metrics.retrieval import compute_retrieval_metrics
from ragdiag.models.chunk import RetrievedChunk
from ragdiag.runner.evaluator import Evaluator

from chunk_ids import (
    abs_path,
    build_catalog_from_chunk_markdown,
    match_chunk_id,
    resolve_golden_ids,
)
from narsil_mcp_client import NarsilMcpClient, NarsilChunk
from paired_stats import mcnemar_exact, paired_permutation

K = 5
REPO = _ROOT / "test-fixtures"
BINARY = _ROOT / "target/debug/narsil-mcp"
DATASET = _EVAL / "narsil_fixtures_dataset.json"


def to_retrieved(hits: list[NarsilChunk]) -> list[RetrievedChunk]:
    return [RetrievedChunk(id=h.id, text=h.text, score=h.score) for h in hits]


def metrics_for(relevant: list[str], hits: list[NarsilChunk]) -> tuple[float, float, float]:
    m = compute_retrieval_metrics(relevant, to_retrieved(hits), k=K)
    return m.precision_at_k, m.recall_at_k, m.reciprocal_rank


def normalize_hits(
    hits: list[NarsilChunk], catalog, repo_root: str
) -> list[NarsilChunk]:
    out: list[NarsilChunk] = []
    for h in hits:
        cid = h.id
        file_path = h.file_path or h.id
        if not file_path.startswith("/"):
            file_path = abs_path(repo_root, file_path.strip("`"))
        if h.start_line and h.end_line:
            mapped = match_chunk_id(catalog, file_path, h.start_line, h.end_line)
            if mapped:
                cid = mapped
        elif cid.count(":") >= 2 and cid.startswith("/"):
            pass
        out.append(
            NarsilChunk(
                id=cid,
                text=h.text,
                score=h.score,
                file_path=file_path,
                start_line=h.start_line,
                end_line=h.end_line,
            )
        )
    return out


def main() -> int:
    dataset = load_dataset(DATASET)
    repo_root = str(REPO.resolve())
    failures = 0

    client = NarsilMcpClient(BINARY, [REPO])
    client.start()

    catalog_md = client.call_tool_text(
        "search_chunks",
        {
            "query": "process_user calculate_total fetch_data def",
            "repo": client._repo_name,
            "max_results": 30,
        },
    )
    catalog = build_catalog_from_chunk_markdown(catalog_md)

    tools = ("search_chunks", "hybrid_search", "search_code")
    agg: dict[str, list[tuple[float, float, float]]] = {t: [] for t in tools}

    print(f"Dataset: {dataset.name} ({len(dataset.samples)} samples), K={K}\n")
    print(
        f"{'query_id':<20} {'tool':<16} {'P@5':>6} {'R@5':>6} {'MRR':>6}  top chunk ids"
    )
    print("-" * 100)

    for sample in dataset.samples:
        relevant = resolve_golden_ids(repo_root, list(sample.relevant_chunk_ids))
        for tool in tools:
            if tool == "search_chunks":
                hits = client.search_chunks(sample.query, K)
            elif tool == "hybrid_search":
                hits = normalize_hits(
                    client.hybrid_search(sample.query, K, mode="hybrid"), catalog, repo_root
                )
            else:
                hits = normalize_hits(client.search_code(sample.query, K), catalog, repo_root)

            p, r, rr = metrics_for(relevant, hits)
            agg[tool].append((p, r, rr))
            ids = [h.id.rsplit("/", 1)[-1] for h in hits[:2]]
            print(f"{sample.id:<20} {tool:<16} {p:6.2f} {r:6.2f} {rr:6.2f}  {ids}")

        direct = client.search_chunks(sample.query, K)
        dp, dr, _ = metrics_for(relevant, direct)

        from narsil_chunk_pipeline import NarsilChunkPipeline

        ev_result = Evaluator(k=K).execute_sample(sample, NarsilChunkPipeline())
        ev_p = float(ev_result.metrics.get(f"precision_at_{K}", 0))
        ev_r = float(ev_result.metrics.get(f"recall_at_{K}", 0))
        ev_rr = float(ev_result.metrics.get("reciprocal_rank", 0))
        _, _, drr = metrics_for(relevant, direct)
        if abs(ev_p - dp) > 1e-9 or abs(ev_r - dr) > 1e-9 or abs(ev_rr - drr) > 1e-9:
            print(
                f"  FAIL RAGDiag Evaluator vs same-query MCP {sample.id}: "
                f"P/R/MRR {ev_p}/{ev_r}/{ev_rr} vs {dp}/{dr}/{drr}"
            )
            failures += 1
        else:
            print(f"  OK: RAGDiag pipeline metrics match MCP for {sample.id}")
        print()

    def mean(xs: list[tuple[float, float, float]]) -> tuple[float, float, float]:
        n = len(xs)
        return (sum(x[0] for x in xs) / n, sum(x[1] for x in xs) / n, sum(x[2] for x in xs) / n)

    print("=" * 100)
    for tool in tools:
        p, r, rr = mean(agg[tool])
        print(f"{tool:<16} mean direct MCP (NL queries): P@5={p:.4f} R@5={r:.4f} MRR={rr:.4f}")

    # Means above hide whether a gap would survive other queries; test it per query.
    print("\nPaired tests vs search_chunks (hit@K: exact McNemar, MRR: permutation):")
    base = agg["search_chunks"]
    for tool in tools[1:]:
        other = agg[tool]
        m = mcnemar_exact([x[0] > 0 for x in base], [x[0] > 0 for x in other])
        perm = paired_permutation([x[2] for x in base], [x[2] for x in other])
        print(
            f"{tool:<16} hit@{K}: only_base={m.only_a} only_{tool}={m.only_b} p={m.p_value:.3f}; "
            f"MRR diff={perm.mean_diff:+.3f} p={perm.p_value:.3f}"
        )
    if len(dataset.samples) < 20:
        print(f"  (n={len(dataset.samples)}: too few queries for any difference to be significant)")

    # search_code is symbol-oriented BM25; compare on identifier-style queries.
    sym_queries = {
        "q_calculate_total": "calculate_total",
        "q_process_user": "process_user",
        "q_fetch_data": "fetch_data",
    }
    sym_scores: list[tuple[float, float, float]] = []
    for sample in dataset.samples:
        relevant = list(sample.relevant_chunk_ids)
        hits = normalize_hits(
            client.search_code(sym_queries[sample.id], K), catalog, repo_root
        )
        sym_scores.append(metrics_for(relevant, hits))
    client.close()
    sp, sr, sm = mean(sym_scores)
    print(
        f"search_code      mean direct MCP (symbol queries + line→chunk map): "
        f"P@5={sp:.4f} R@5={sr:.4f} MRR={sm:.4f}"
    )

    hist = _EVAL / ".ragdiag/evaluations/latest.json"
    if hist.exists():
        data = json.loads(hist.read_text())
        if data.get("pipeline_name") == "narsil_search_chunks":
            rep = data["retrieval"]
            mp, mr, mm = mean(agg["search_chunks"])
            rp, rr, rm = rep["mean_precision_at_k"], rep["mean_recall_at_k"], rep["mrr"]
            print(
                f"RAGDiag CLI (search_chunks pipeline): P@5={rp:.4f} R@5={rr:.4f} MRR={rm:.4f}"
            )
            if abs(rp - mp) > 1e-6 or abs(rr - mr) > 1e-6 or abs(rm - mm) > 1e-6:
                print("  FAIL: stale CLI report — run: ragdiag run --pipeline narsil_chunk_pipeline.py ...")
                failures += 1
            else:
                print("  OK: RAGDiag CLI matches live MCP metrics.")

    print(f"\n{'PASSED' if failures == 0 else f'FAILED ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
