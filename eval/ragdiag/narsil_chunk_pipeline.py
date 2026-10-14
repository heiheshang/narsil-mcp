"""RAGDiag adapter: narsil-mcp search_chunks retrieval."""

from __future__ import annotations

import sys
from pathlib import Path

_EVAL_DIR = Path(__file__).resolve().parent
if str(_EVAL_DIR) not in sys.path:
    sys.path.insert(0, str(_EVAL_DIR))

from ragdiag import Pipeline, RetrievedChunk

from narsil_mcp_client import NarsilMcpClient

_ROOT = Path(__file__).resolve().parents[2]
_BINARY = _ROOT / "target/debug/narsil-mcp"
_REPO = _ROOT / "test-fixtures"

_mcp: NarsilMcpClient | None = None


def _get_mcp() -> NarsilMcpClient:
    global _mcp
    if _mcp is None:
        c = NarsilMcpClient(_BINARY, [_REPO])
        c.start()
        _mcp = c
    return _mcp


class NarsilChunkPipeline(Pipeline):
    name = "narsil_search_chunks"

    def retrieve(self, query: str) -> list[RetrievedChunk]:
        hits = _get_mcp().search_chunks(query, max_results=5)
        return [
            RetrievedChunk(id=h.id, text=h.text, score=h.score)
            for h in hits
        ]

    def generate(self, query: str, chunks: list[RetrievedChunk]) -> str:
        if not chunks:
            return "No context retrieved."
        top = chunks[0].text.split("\n")[0][:200]
        return f"Based on indexed code: {top}"


pipeline = NarsilChunkPipeline()
