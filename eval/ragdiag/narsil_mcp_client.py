"""Minimal MCP stdio client for narsil-mcp search tools."""
from __future__ import annotations

import json
import re
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass
class NarsilChunk:
    id: str
    text: str
    score: float
    file_path: str = ""
    start_line: int = 0
    end_line: int = 0


class NarsilMcpClient:
    def __init__(self, binary: Path, repos: list[Path], extra_args: list[str] | None = None):
        self.binary = binary
        self.repos = repos
        self.extra_args = extra_args or []
        self._proc: subprocess.Popen[str] | None = None
        self._lock = threading.Lock()
        self._id = 0
        self._repo_name: str | None = None

    def start(self, init_timeout: float = 120.0) -> None:
        cmd = [str(self.binary)]
        for r in self.repos:
            cmd.extend(["--repos", str(r)])
        cmd.extend(self.extra_args)
        self._proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            # Nobody reads stderr; a PIPE would fill up and block the server mid-request.
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        assert self._proc.stdin and self._proc.stdout
        self._request("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "ragdiag-narsil", "version": "0.1"},
        })
        self._notify("notifications/initialized", {})
        deadline = time.time() + init_timeout
        while time.time() < deadline:
            repos = self._request("tools/call", {"name": "list_repos", "arguments": {}})
            text = _tool_text(repos)
            m = re.search(r"^##\s+(\S+)\s*$", text, re.M)
            if m:
                self._repo_name = m.group(1).strip()
                break
            time.sleep(0.5)
        if not self._repo_name:
            self._repo_name = self.repos[0].name

    def close(self) -> None:
        if self._proc and self._proc.stdin:
            self._proc.stdin.close()
        if self._proc:
            self._proc.wait(timeout=30)

    def _notify(self, method: str, params: dict[str, Any]) -> None:
        assert self._proc and self._proc.stdin
        msg = {"jsonrpc": "2.0", "method": method, "params": params}
        with self._lock:
            self._proc.stdin.write(json.dumps(msg) + "\n")
            self._proc.stdin.flush()

    def _request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        assert self._proc and self._proc.stdin and self._proc.stdout
        self._id += 1
        rid = self._id
        msg = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        with self._lock:
            self._proc.stdin.write(json.dumps(msg) + "\n")
            self._proc.stdin.flush()
            while True:
                line = self._proc.stdout.readline()
                if not line:
                    raise RuntimeError("narsil-mcp stdout closed")
                data = json.loads(line)
                if data.get("id") == rid:
                    if "error" in data and data["error"]:
                        raise RuntimeError(data["error"])
                    return data

    def search_chunks(self, query: str, max_results: int = 5) -> list[NarsilChunk]:
        resp = self._request(
            "tools/call",
            {
                "name": "search_chunks",
                "arguments": {
                    "query": query,
                    "repo": self._repo_name,
                    "max_results": max_results,
                },
            },
        )
        return _parse_chunk_markdown(_tool_text(resp))

    def search_code(self, query: str, max_results: int = 5) -> list[NarsilChunk]:
        resp = self._request(
            "tools/call",
            {
                "name": "search_code",
                "arguments": {
                    "query": query,
                    "repo": self._repo_name,
                    "max_results": max_results,
                },
            },
        )
        return _parse_search_code_markdown(_tool_text(resp))

    def hybrid_search(self, query: str, max_results: int = 5, mode: str = "hybrid") -> list[NarsilChunk]:
        resp = self._request(
            "tools/call",
            {
                "name": "hybrid_search",
                "arguments": {
                    "query": query,
                    "repo": self._repo_name,
                    "max_results": max_results,
                    "mode": mode,
                },
            },
        )
        return _parse_hybrid_markdown(_tool_text(resp))

    def call_tool_text(self, name: str, arguments: dict[str, Any]) -> str:
        resp = self._request("tools/call", {"name": name, "arguments": arguments})
        return _tool_text(resp)


def _tool_text(resp: dict[str, Any]) -> str:
    try:
        return resp["result"]["content"][0]["text"]
    except (KeyError, IndexError, TypeError):
        return str(resp)


def _parse_chunk_markdown(text: str) -> list[NarsilChunk]:
    chunks: list[NarsilChunk] = []
    blocks = re.split(r"\n## \d+\. ", text)
    for block in blocks[1:]:
        lines = block.split("\n")
        cid = lines[0].strip()
        file_path = ""
        score = 0.0
        for ln in lines[1:20]:
            if ln.startswith("- **File**:"):
                file_path = ln.split(":", 1)[1].strip()
            if ln.startswith("- **Score**:"):
                try:
                    score = float(ln.split(":", 1)[1].strip())
                except ValueError:
                    pass
        m = re.search(r"```\n(.*?)```", block, re.S)
        body = m.group(1).strip() if m else block
        chunks.append(
            NarsilChunk(
                id=cid,
                text=body,
                score=score,
                file_path=file_path,
                start_line=int(m_line.group(1)) if (m_line := re.search(r"- \*\*Lines\*\*:\s*(\d+)-(\d+)", block)) else 0,
                end_line=int(m_line.group(2)) if m_line else 0,
            )
        )
    return chunks


def _parse_search_code_markdown(text: str) -> list[NarsilChunk]:
    chunks: list[NarsilChunk] = []
    for block in re.split(r"\n## \d+\. ", text)[1:]:
        header, _, rest = block.partition("\n")
        path = header.strip().strip("`")
        m_lines = re.search(r"Lines\s+(\d+)-(\d+)\s*\|\s*Score:\s*([\d.]+)", rest)
        m_code = re.search(r"```[^\n]*\n(.*?)```", rest, re.S)
        if not m_lines:
            continue
        start, end, score_s = int(m_lines.group(1)), int(m_lines.group(2)), m_lines.group(3)
        body = m_code.group(1).strip() if m_code else ""
        chunks.append(
            NarsilChunk(
                id=path,
                text=body,
                score=float(score_s),
                file_path=path,
                start_line=start,
                end_line=end,
            )
        )
    return chunks


def _parse_hybrid_markdown(text: str) -> list[NarsilChunk]:
    chunks: list[NarsilChunk] = []
    for block in re.split(r"\n## \d+\. ", text)[1:]:
        lines = block.split("\n")
        path = lines[0].strip()
        score = 0.0
        start_line = end_line = 0
        for ln in lines[1:12]:
            # Unscoped searches name the repo; paths are relative to it.
            if ln.startswith("- **Repo**:"):
                path = f"{ln.split(':', 1)[1].strip()}/{path}"
            if ln.startswith("- **Score**:"):
                score = float(ln.split(":", 1)[1].strip())
            m = re.match(r"- \*\*Lines\*\*:\s*(\d+)-(\d+)", ln)
            if m:
                start_line, end_line = int(m.group(1)), int(m.group(2))
        m_code = re.search(r"```\n(.*?)```", block, re.S)
        body = m_code.group(1).strip() if m_code else ""
        chunks.append(
            NarsilChunk(
                id=path,
                text=body,
                score=score,
                file_path=path,
                start_line=start_line,
                end_line=end_line,
            )
        )
    return chunks
