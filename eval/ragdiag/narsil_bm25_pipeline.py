"""RAGDiag adapter: narsil-mcp search_code (BM25) retrieval."""

from __future__ import annotations

import sys
from pathlib import Path

_EVAL_DIR = Path(__file__).resolve().parent
if str(_EVAL_DIR) not in sys.path:
    sys.path.insert(0, str(_EVAL_DIR))

from ragdiag import Pipeline, RetrievedChunk

from chunk_ids import build_catalog_from_chunk_markdown, match_chunk_id, abs_path
from narsil_mcp_client import NarsilMcpClient

_ROOT = Path(__file__).resolve().parents[2]
_BINARY = _ROOT / "target/debug/narsil-mcp"
_REPO = _ROOT / "test-fixtures"

_mcp: NarsilMcpClient | None = None
_catalog = None


def _get_mcp() -> NarsilMcpClient:
    global _mcp, _catalog
    if _mcp is None:
        c = NarsilMcpClient(_BINARY, [_REPO])
        c.start()
        md = c.call_tool_text(
            "search_chunks",
            {"query": "process_user calculate_total fetch_data", "repo": c._repo_name, "max_results": 30},
        )
        _catalog = build_catalog_from_chunk_markdown(md)
        _mcp = c
    return _mcp


def _normalize(hits):
    c = _get_mcp()
    out = []
    for h in hits:
        fp = h.file_path or h.id
        if not fp.startswith("/"):
            fp = abs_path(str(_REPO.resolve()), fp.strip("`"))
        cid = h.id
        if h.start_line and h.end_line:
            mapped = match_chunk_id(_catalog, fp, h.start_line, h.end_line)
            if mapped:
                cid = mapped
        out.append(RetrievedChunk(id=cid, text=h.text, score=h.score))
    return out


class NarsilBm25Pipeline(Pipeline):
    name = "narsil_search_code"

    def retrieve(self, query: str) -> list[RetrievedChunk]:
        hits = _get_mcp().search_code(query, max_results=5)
        return _normalize(hits)

    def generate(self, query: str, chunks: list[RetrievedChunk]) -> str:
        if not chunks:
            return "No context retrieved."
        top = chunks[0].text.split("\n")[0][:200]
        return f"Based on indexed code: {top}"


pipeline = NarsilBm25Pipeline()
