#!/usr/bin/env python3
"""Does a FRIDA-Decisions cross-encoder rerank improve narsil retrieval on 1C code?

Runs every query of onec/dataset.json through search_chunks and hybrid_search
(top --pool candidates), reranks each pool with FRIDA-Decisions (ranking
question, ONNX int8 on CPU), and compares the four orderings pairwise:
exact McNemar on hit@1 / hit@k, sign-flip permutation on MRR.

Retrieval and FRIDA scores are cached in --cache, so re-running only the
statistics is instant; delete the cache after changing narsil or the dataset.

    uv venv .venv --python 3.12
    uv pip install --python .venv/bin/python \
        "frida-decisions[onnx] @ git+https://github.com/ai-forever/FRIDA-Decisions@v0.4.0"
    .venv/bin/python onec/rerank_eval.py --repo ~/Downloads/ConfigFiles/CommonModules
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
import time
from pathlib import Path

_EVAL = Path(__file__).resolve().parents[1]
_ROOT = _EVAL.parents[1]
sys.path.insert(0, str(_EVAL))

from narsil_mcp_client import NarsilChunk, NarsilMcpClient
from paired_stats import mcnemar_exact, paired_permutation

# Module groups the dataset was drawn from. search_chunks and hybrid_search
# re-chunk the whole repo on every call (~40 s and ~80 s per query on the full
# 2279-module CommonModules), so only these groups are indexed: ~200 modules.
PREFIXES = (
    "Пользовател СтроковыеФункции ОбщегоНазначения РаботаСФайлами УправлениеПечатью "
    "Ценообразовани ДатыЗапрета Банк Касс Календар Скидк Партнер Номенклатур"
).split()

INSTRUCTIONS = "Какой фрагмент кода 1С лучше всего соответствует запросу разработчика?"
# FRIDA options are capped at 256 tokens; the head of a chunk (doc comment +
# signature) carries nearly all the signal, so cut well before the cap.
MAX_CANDIDATE_CHARS = 700


def wait_indexed(client: NarsilMcpClient, timeout: float = 1800) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if "In Progress" not in client.call_tool_text("get_index_status", {}):
            return
        time.sleep(2)
    raise TimeoutError("narsil did not finish indexing")


def as_dict(h: NarsilChunk) -> dict:
    return {
        "file": h.file_path,
        "start": h.start_line,
        "end": h.end_line,
        "text": h.text,
    }


def retrieve(args: argparse.Namespace, samples: list[dict]) -> dict:
    modules = sorted(
        d for d in args.repo.iterdir() if d.is_dir() and d.name.startswith(tuple(PREFIXES))
    )
    client = NarsilMcpClient(args.binary, modules)
    client.start(init_timeout=600)
    wait_indexed(client)
    client._repo_name = None  # one repo per module: search across all of them
    out = {}
    for s in samples:
        out[s["id"]] = {}
        for tool in ("search_chunks", "hybrid_search"):
            t = time.perf_counter()
            hits = getattr(client, tool)(s["query"], args.pool)
            out[s["id"]][tool] = [as_dict(h) for h in hits]
            out[s["id"]][f"{tool}_seconds"] = time.perf_counter() - t
    client.close()
    return out


def frida_order(judge, query: str, cands: list[dict]) -> tuple[list[int], float]:
    if len(cands) < 2:
        return list(range(len(cands))), 0.0
    criteria = {f"c{i}": c["text"][:MAX_CANDIDATE_CHARS] or "(пусто)" for i, c in enumerate(cands)}
    req = {
        "state": query,
        "questions": {
            "best": {"type": "ranking", "instructions": INSTRUCTIONS, "criteria": criteria}
        },
    }
    t = time.perf_counter()
    ans = judge(req)["answers"]["best"]
    elapsed = time.perf_counter() - t
    return [int(cid[1:]) for cid in ans["ranking"]], elapsed


def is_relevant(c: dict, relevant: list[dict]) -> bool:
    # Absolute paths (search_chunks) or "<repo>/<relative>" (hybrid_search).
    path = "/" + c["file"].replace("\\", "/").lstrip("/")
    return any(
        f"/{r['module']}/Ext/" in path and c["start"] <= r["line"] <= c["end"] for r in relevant
    )


def first_hit(ordered: list[dict], relevant: list[dict]) -> int | None:
    for i, c in enumerate(ordered):
        if is_relevant(c, relevant):
            return i + 1
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", type=Path, required=True, help="CommonModules dir of a 1C dump")
    ap.add_argument("--dataset", type=Path, default=_EVAL / "onec/dataset.json")
    ap.add_argument("--binary", type=Path, default=_ROOT / "target/release/narsil-mcp")
    ap.add_argument("--pool", type=int, default=20, help="candidates fetched and reranked")
    ap.add_argument("--k", type=int, default=5)
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--cache", type=Path, default=_EVAL / "onec/.cache.json")
    args = ap.parse_args()

    samples = json.loads(args.dataset.read_text())["samples"]
    cache = json.loads(args.cache.read_text()) if args.cache.exists() else {}
    if cache.get("pool") != args.pool or set(cache.get("retrieval", {})) != {s["id"] for s in samples}:
        print(f"Retrieving top-{args.pool} for {len(samples)} queries...", file=sys.stderr)
        cache = {"pool": args.pool, "retrieval": retrieve(args, samples), "rerank": {}}
        args.cache.write_text(json.dumps(cache, ensure_ascii=False))

    judge = None
    for s in samples:
        for base in ("search_chunks", "hybrid_search"):
            key = f"{s['id']}|{base}"
            if key in cache["rerank"]:
                continue
            if judge is None:
                from frida_decisions import OnnxJudge

                judge = OnnxJudge.from_pretrained("ai-forever/FRIDA-Decisions", threads=args.threads)
            order, secs = frida_order(judge, s["query"], cache["retrieval"][s["id"]][base])
            cache["rerank"][key] = {"order": order, "seconds": secs}
            args.cache.write_text(json.dumps(cache, ensure_ascii=False))
            print(f"  {key}: {secs:.2f}s", file=sys.stderr)

    systems = ["search_chunks", "search_chunks+frida", "hybrid_search", "hybrid_search+frida"]
    ranks: dict[str, list[int | None]] = {name: [] for name in systems}
    for s in samples:
        for base in ("search_chunks", "hybrid_search"):
            cands = cache["retrieval"][s["id"]][base]
            order = cache["rerank"][f"{s['id']}|{base}"]["order"]
            ranks[base].append(first_hit(cands, s["relevant"]))
            ranks[f"{base}+frida"].append(first_hit([cands[i] for i in order], s["relevant"]))

    n, k = len(samples), args.k

    def hits(name: str, at: int) -> list[bool]:
        return [r is not None and r <= at for r in ranks[name]]

    def rr(name: str, at: int = 10) -> list[float]:
        return [1.0 / r if r is not None and r <= at else 0.0 for r in ranks[name]]

    print(f"\n{args.dataset.name}: {n} queries, pool={args.pool}, k={k}\n")
    print(f"{'system':<22} {'hit@1':>7} {f'hit@{k}':>7} {'MRR@10':>7} {f'recall@{args.pool}':>10}")
    for name in systems:
        print(
            f"{name:<22} {sum(hits(name, 1)) / n:7.3f} {sum(hits(name, k)) / n:7.3f} "
            f"{statistics.mean(rr(name)):7.3f} {sum(hits(name, args.pool)) / n:10.3f}"
        )

    print("\nPaired tests (B vs A; only_A/only_B = discordant queries):")
    pairs = [
        ("search_chunks", "search_chunks+frida"),
        ("hybrid_search", "hybrid_search+frida"),
        ("search_chunks", "hybrid_search"),
        ("search_chunks", "hybrid_search+frida"),
    ]
    for a, b in pairs:
        print(f"  {b}  vs  {a}")
        for at in (1, k):
            m = mcnemar_exact(hits(a, at), hits(b, at))
            print(f"    hit@{at:<2} only_A={m.only_a:<3} only_B={m.only_b:<3} McNemar p={m.p_value:.4f}")
        p = paired_permutation(rr(a), rr(b))
        print(f"    MRR   diff={p.mean_diff:+.3f}  permutation p={p.p_value:.4f}")

    for base in ("search_chunks", "hybrid_search"):
        t = [r[f"{base}_seconds"] for r in cache["retrieval"].values() if f"{base}_seconds" in r]
        if t:
            print(f"{base} latency: first {t[0]:.2f}s, median {statistics.median(t):.2f}s")

    secs = [v["seconds"] for v in cache["rerank"].values() if v["seconds"] > 0]
    if secs:
        print(
            f"\nFRIDA rerank latency (ONNX int8, CPU, {args.pool} candidates): "
            f"median {statistics.median(secs):.2f}s, max {max(secs):.2f}s"
        )

    misses = [
        (s["id"], ranks["search_chunks"][i], ranks["search_chunks+frida"][i])
        for i, s in enumerate(samples)
        if ranks["search_chunks"][i] != ranks["search_chunks+frida"][i]
    ]
    print("\nRank changes search_chunks -> +frida (None = not in pool):")
    for qid, before, after in misses:
        print(f"  {qid:<60} {before} -> {after}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
