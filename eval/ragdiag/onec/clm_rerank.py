#!/usr/bin/env python3
"""Does a CLM (Contrastive-LM/CLM-v0.1-8B) rerank improve narsil retrieval on 1C code?

Reranks the cached top-20 pools of rerank_eval.py (same candidates, same FRIDA
orders) with CLM and compares search_chunks / hybrid_search, +frida, +clm and
+clm-raw (cosine of the bare Qwen3-8B embeddings, no projection heads).

CLM is a bi-encoder: Qwen3-8B last-token embeddings, then a state head for the
query and an action head for each candidate, ranked by cosine. The reference
serves Qwen3-8B through vLLM; here it is any OpenAI-compatible /v1/embeddings
endpoint, e.g. llama-server --embedding --pooling last on Qwen3-8B-Q8_0.gguf.
--selfcheck reproduces the README examples to confirm the endpoint matches.

Only the head math is reimplemented (clm/heads.py, clm/schema.py at d5f9ef0),
so the contrastive-lm package is not needed: torch, numpy, requests.

    python onec/clm_rerank.py --ckpt CLM_v0.1-8B.pt --emb-url http://127.0.0.1:8091/v1/embeddings --selfcheck
    python onec/clm_rerank.py --ckpt CLM_v0.1-8B.pt --emb-url http://127.0.0.1:8091/v1/embeddings
"""

from __future__ import annotations

import argparse
import base64
import json
import math
import statistics
import sys
import time
from pathlib import Path

import numpy as np
import requests
import torch
import torch.nn as nn

_EVAL = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(_EVAL))
sys.path.insert(0, str(_EVAL / "onec"))

from paired_stats import mcnemar_exact, paired_permutation
from rerank_eval import INSTRUCTIONS, first_hit

BASES = ("search_chunks", "hybrid_search")
RAW_SCALE = 100.0


class Head(nn.Module):
    def __init__(self, hidden: int, width: int, depth: int, proj: int, activation: str,
                 layernorm: bool, residual: bool):
        super().__init__()
        self.inp = nn.Linear(hidden, width)
        self.hidden = nn.ModuleList(nn.Linear(width, width) for _ in range(depth - 2))
        self.norms = nn.ModuleList(
            (nn.LayerNorm(width) if layernorm else nn.Identity()) for _ in range(depth - 2)
        )
        self.out = nn.Linear(width, proj)
        self.act = {"gelu": nn.GELU, "relu": nn.ReLU, "silu": nn.SiLU}[activation]()
        self.residual = residual

    def forward(self, x):
        x = self.act(self.inp(x))
        for lin, nrm in zip(self.hidden, self.norms):
            h = self.act(nrm(lin(x)))
            x = x + h if self.residual else h
        return self.out(x)


class Clm:
    def __init__(self, ckpt: Path, emb_url: str, emb_model: str):
        ck = torch.load(ckpt, map_location="cpu", weights_only=True)
        cfg = dict(ck["cfg"])
        kw = dict(
            hidden=cfg.get("hidden_size", 4096), width=cfg["width"], depth=cfg["depth"],
            proj=ck.get("projection_dim", cfg.get("projection_dim", 512)),
            activation=cfg.get("activation", "gelu"), layernorm=cfg.get("layernorm", False),
            residual=cfg.get("residual", False),
        )
        self.state_head, self.action_head = Head(**kw).eval(), Head(**kw).eval()
        self.state_head.load_state_dict(ck["state_head"])
        self.action_head.load_state_dict(ck["action_head"])
        self.scale = float(torch.as_tensor(ck["logit_scale"]).float().exp().clamp(max=100.0))
        self.emb_url, self.emb_model = emb_url, emb_model
        self.session = requests.Session()
        self.memo: dict[str, np.ndarray] = {}

    def embed(self, texts: list[str]) -> np.ndarray:
        todo = [t for t in dict.fromkeys(texts) if t not in self.memo]
        for i in range(0, len(todo), 8):
            batch = todo[i:i + 8]
            r = self.session.post(
                self.emb_url,
                json={"model": self.emb_model, "input": batch, "encoding_format": "base64"},
                timeout=1800,
            )
            r.raise_for_status()
            for d in r.json()["data"]:
                e = d["embedding"]
                v = (np.frombuffer(base64.b64decode(e), dtype=np.float32) if isinstance(e, str)
                     else np.asarray(e, dtype=np.float32))
                self.memo[batch[d["index"]]] = v / (np.linalg.norm(v) + 1e-12)
        return np.stack([self.memo[t] for t in texts])

    def logits(self, state: str, cands: list[str], raw: bool = False) -> list[float]:
        zs, zc = self.embed([state]), self.embed(cands)
        if raw:
            return (RAW_SCALE * (zc @ zs[0])).tolist()
        with torch.no_grad():
            s = nn.functional.normalize(self.state_head(torch.from_numpy(zs)), dim=-1)
            c = nn.functional.normalize(self.action_head(torch.from_numpy(zc)), dim=-1)
        return (self.scale * (c @ s[0])).tolist()


def state_text(state: str, instructions: str) -> str:
    s, i = state.strip(), instructions.strip()
    return f"{s}\n\n{i}" if s and i else (s or i)


def softmax(xs: list[float]) -> list[float]:
    m = max(xs)
    e = [math.exp(x - m) for x in xs]
    return [x / sum(e) for x in e]


def selfcheck(clm: Clm) -> None:
    state = "Customer: my invoice was charged twice and nobody answers the phone!"
    urgent = "Is this urgent?"
    p = softmax(clm.logits(state_text(state, urgent), [
        f"false: No. This is false: {urgent}", f"true: Yes. This is true: {urgent}",
    ]))
    print(f"urgency noul          {p[1]:.5f}   README 0.41022")
    p = softmax(clm.logits(state_text(state, "Which team should handle this?"),
                           ["Charges, invoices, refunds", "Bugs and outages"]))
    print(f"department billing    {p[0]:.5f}   README 0.93878")
    p = softmax(clm.logits(state_text(state, "How frustrated is the customer?"),
                           ["Calm", "Frustrated", "Very angry"]))
    print(f"frustration score     {sum(i * x for i, x in enumerate(p)):.5f}   README 1.98386")
    p = softmax(clm.logits("What causes tides on Earth?", [
        "The Moon's gravitational pull.", "Photosynthesis in plants.", "Because the Earth is round.",
    ]))
    print(f"tides moon            {p[0]:.3f}     README 0.997")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ckpt", type=Path, required=True, help="CLM_v0.1-8B.pt")
    ap.add_argument("--emb-url", default="http://127.0.0.1:8091/v1/embeddings")
    ap.add_argument("--emb-model", default="qwen3-8b")
    ap.add_argument("--dataset", type=Path, default=_EVAL / "onec/dataset.json")
    ap.add_argument("--cache", type=Path, default=_EVAL / "onec/.cache.after.json",
                    help="rerank_eval.py cache: pools and FRIDA orders")
    ap.add_argument("--clm-cache", type=Path, default=_EVAL / "onec/.cache.clm.json")
    ap.add_argument("--k", type=int, default=5)
    ap.add_argument("--selfcheck", action="store_true")
    args = ap.parse_args()

    clm = Clm(args.ckpt, args.emb_url, args.emb_model)
    if args.selfcheck:
        selfcheck(clm)
        return 0

    samples = json.loads(args.dataset.read_text())["samples"]
    cache = json.loads(args.cache.read_text())
    pool = cache["pool"]
    out = json.loads(args.clm_cache.read_text()) if args.clm_cache.exists() else {}
    for s in samples:
        for base in BASES:
            key = f"{s['id']}|{base}"
            if key in out:
                continue
            cands = [c["text"] or "(пусто)" for c in cache["retrieval"][s["id"]][base]]
            state = state_text(s["query"], INSTRUCTIONS)
            t = time.perf_counter()
            head = clm.logits(state, cands)
            secs = time.perf_counter() - t
            raw = clm.logits(state, cands, raw=True)
            out[key] = {
                "clm": sorted(range(len(cands)), key=lambda i: -head[i]),
                "clm-raw": sorted(range(len(cands)), key=lambda i: -raw[i]),
                "seconds": secs,
            }
            args.clm_cache.write_text(json.dumps(out, ensure_ascii=False))
            print(f"  {key}: {secs:.1f}s", file=sys.stderr)

    rerankers = ("frida", "clm", "clm-raw")
    ranks: dict[str, list[int | None]] = {}
    for s in samples:
        for base in BASES:
            cands = cache["retrieval"][s["id"]][base]
            key = f"{s['id']}|{base}"
            orders = {
                "frida": cache["rerank"][key]["order"],
                "clm": out[key]["clm"],
                "clm-raw": out[key]["clm-raw"],
            }
            ranks.setdefault(base, []).append(first_hit(cands, s["relevant"]))
            for r in rerankers:
                ranks.setdefault(f"{base}+{r}", []).append(
                    first_hit([cands[i] for i in orders[r]], s["relevant"])
                )

    n, k = len(samples), args.k

    def hits(name: str, at: int) -> list[bool]:
        return [r is not None and r <= at for r in ranks[name]]

    def rr(name: str, at: int = 10) -> list[float]:
        return [1.0 / r if r is not None and r <= at else 0.0 for r in ranks[name]]

    print(f"\n{args.dataset.name}: {n} queries, pool={pool}, k={k}\n")
    print(f"{'system':<24} {'hit@1':>7} {f'hit@{k}':>7} {'MRR@10':>7} {f'recall@{pool}':>10}")
    for name in ranks:
        print(
            f"{name:<24} {sum(hits(name, 1)) / n:7.3f} {sum(hits(name, k)) / n:7.3f} "
            f"{statistics.mean(rr(name)):7.3f} {sum(hits(name, pool)) / n:10.3f}"
        )

    print("\nPaired tests (B vs A; only_A/only_B = discordant queries):")
    pairs = [(base, f"{base}+clm") for base in BASES]
    pairs += [(f"{base}+frida", f"{base}+clm") for base in BASES]
    pairs += [(f"{base}+clm-raw", f"{base}+clm") for base in BASES]
    for a, b in pairs:
        print(f"  {b}  vs  {a}")
        for at in (1, k):
            m = mcnemar_exact(hits(a, at), hits(b, at))
            print(f"    hit@{at:<2} only_A={m.only_a:<3} only_B={m.only_b:<3} McNemar p={m.p_value:.4f}")
        p = paired_permutation(rr(a), rr(b))
        print(f"    MRR   diff={p.mean_diff:+.3f}  permutation p={p.p_value:.4f}")

    secs = [v["seconds"] for v in out.values()]
    print(f"\nCLM rerank latency ({pool} candidates, encoder memo shared across bases): "
          f"median {statistics.median(secs):.1f}s, max {max(secs):.1f}s")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
