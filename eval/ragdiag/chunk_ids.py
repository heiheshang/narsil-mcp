"""Resolve narsil markdown hits to canonical AST chunk IDs (search_chunks format)."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class ChunkRef:
    id: str
    file_path: str
    start_line: int
    end_line: int


def abs_path(repo_root: str, rel_or_abs: str) -> str:
    p = rel_or_abs.replace("\\", "/")
    if p.startswith("/"):
        return p
    root = Path(repo_root).resolve().as_posix().rstrip("/")
    if p.startswith("test-fixtures/"):
        p = p[len("test-fixtures/") :]
    return f"{root}/{p.lstrip('/')}"


def resolve_golden_ids(repo_root: str, ids: list[str]) -> list[str]:
    return [abs_path(repo_root, i) for i in ids]


def line_overlap(a0: int, a1: int, b0: int, b1: int) -> int:
    lo = max(a0, b0)
    hi = min(a1, b1)
    return max(0, hi - lo + 1)


def match_chunk_id(catalog: list[ChunkRef], file_path: str, start_line: int, end_line: int) -> str | None:
    file_norm = file_path.replace("\\", "/")
    best: tuple[int, str] | None = None
    for c in catalog:
        if c.file_path.replace("\\", "/") != file_norm:
            continue
        ov = line_overlap(c.start_line, c.end_line, start_line, end_line)
        if ov <= 0:
            continue
        if best is None or ov > best[0]:
            best = (ov, c.id)
    return best[1] if best else None


def build_catalog_from_chunk_markdown(text: str) -> list[ChunkRef]:
    catalog: list[ChunkRef] = []
    blocks = re.split(r"\n## \d+\. ", text)
    for block in blocks[1:]:
        lines = block.split("\n")
        cid = lines[0].strip()
        start_line = end_line = 0
        file_path = ""
        for ln in lines[1:15]:
            if ln.startswith("- **File**:"):
                file_path = ln.split(":", 1)[1].strip()
            m = re.match(r"- \*\*Lines\*\*:\s*(\d+)-(\d+)", ln)
            if m:
                start_line, end_line = int(m.group(1)), int(m.group(2))
        if cid and file_path:
            catalog.append(
                ChunkRef(id=cid, file_path=file_path, start_line=start_line, end_line=end_line)
            )
    return catalog
