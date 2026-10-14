#!/usr/bin/env python3
"""Emit golden dataset chunk IDs exactly as narsil search_chunks returns them."""

from __future__ import annotations

import json
import sys
from pathlib import Path

_EVAL = Path(__file__).resolve().parent
_ROOT = _EVAL.parents[1]
sys.path.insert(0, str(_EVAL))

from narsil_mcp_client import NarsilMcpClient

SAMPLES = [
    {
        "id": "q_calculate_total",
        "query": "calculate total price of items in a list",
        "expected_answer": "calculate_total iterates items and sums price times quantity.",
        "symbol": "calculate_total",
        "query_type": "factual",
    },
    {
        "id": "q_process_user",
        "query": "process user id and name into a result dict",
        "expected_answer": "process_user builds a dict with id, name, name_length, and valid flag.",
        "symbol": "process_user",
        "query_type": "factual",
    },
    {
        "id": "q_fetch_data",
        "query": "fetch data from URL using urllib",
        "expected_answer": "fetch_data opens the URL with urllib.request.urlopen and returns decoded text.",
        "symbol": "fetch_data",
        "query_type": "factual",
    },
]

OUT = _EVAL / "narsil_fixtures_dataset.json"
REPO = _ROOT / "test-fixtures"
BINARY = _ROOT / "target/debug/narsil-mcp"


def main() -> None:
    client = NarsilMcpClient(BINARY, [REPO])
    client.start()
    catalog = client.call_tool_text(
        "search_chunks",
        {"query": "process_user calculate_total fetch_data", "repo": client._repo_name, "max_results": 30},
    )
    from narsil_mcp_client import _parse_chunk_markdown

    by_symbol = {}
    for h in _parse_chunk_markdown(catalog):
        sym = h.id.rsplit(":", 1)[-1]
        by_symbol[sym] = h.id

    samples = []
    for s in SAMPLES:
        sym = s["symbol"]
        if sym not in by_symbol:
            raise SystemExit(f"Missing chunk id for symbol {sym}")
        samples.append(
            {
                "id": s["id"],
                "query": s["query"],
                "expected_answer": s["expected_answer"],
                "relevant_chunk_ids": [by_symbol[sym]],
                "query_type": s["query_type"],
            }
        )

    doc = {"name": "narsil_test_fixtures", "version": "1.0", "samples": samples}
    OUT.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"Wrote {OUT}")
    client.close()


if __name__ == "__main__":
    main()
