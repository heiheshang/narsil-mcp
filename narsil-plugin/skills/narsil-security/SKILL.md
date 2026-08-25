---
name: narsil-security
description: Run and interpret narsil-mcp security analysis - vulnerability scans, taint tracing, dependency CVEs, licences, SBOM. Use when asked to audit code for security issues, check injection or OWASP/CWE exposure, judge whether a finding is exploitable, or assess dependency and supply-chain risk.
allowed-tools: mcp__narsil-mcp__*
---

# Security analysis with narsil

## Order of work

1. `get_security_summary` - posture and counts before any detail. Cheap, tells
   you whether a full scan is worth its output.
2. Scope the scan:
   - whole repo: `scan_security`
   - one class of bug: `find_injection_vulnerabilities`, `check_owasp_top10`,
     `check_cwe_top25`
   - one file: pass `path`
   Use `severity_threshold` (`critical`, `high`, ...) instead of reading every
   low-severity hit.
3. For a finding that matters: `trace_taint` from the source, or
   `get_taint_sources` to see what enters the program. `get_typed_taint_flow`
   adds inferred types where the language supports it.
4. Only then `explain_vulnerability` and `suggest_fix`.

Supply chain is a separate ladder: `check_dependencies` (CVEs) →
`find_upgrade_path` (what version clears them) → `check_licenses` →
`generate_sbom` (artifact for someone else).

## A finding is a hypothesis

Every scanner result is a pattern match until you have shown two things:

- **The code is what the finding says.** Open it: `get_excerpt` at `path:line`.
  Report the line you read, not the rule name.
- **The path is reachable.** A tainted sink matters when a real source reaches
  it. `trace_taint` shows the flow; `get_callers` shows whether the function is
  called at all (mind that skill's caveats - "no callers" is not proof of dead
  code).

Say which of the two you verified. "SQL built by concatenation at `db.go:118`,
reached from the HTTP handler `ListOrders` via `buildQuery`" is a finding.
"scan_security reports 14 criticals" is a scan log.

## Reporting

- Group by root cause, not by rule id: ten hits from one helper are one bug.
- Lead with what an attacker gets, then the fix. Do not paste severity tables
  that repeat the tool output.
- Distinguish *vulnerable* from *outdated* for dependencies: a CVE in a package
  you never call on the affected path is worth a line, not an alarm.
- Never propose a fix for code you did not open. `suggest_fix` output is a draft
  against a line, and it does not know the surrounding contract.

## Anti-patterns

| Don't | Do |
|---|---|
| Dump the whole `scan_security` output into the answer | Summarise by cause, link `path:line` |
| Claim exploitability from a pattern match | Trace the taint or say "unverified" |
| Run every checker in parallel "to be thorough" | Summary first, then the checker that matches the question |
| Treat `check_dependencies` output as a to-do list | Check reachability and `find_upgrade_path` before recommending bumps |
| Report the same issue once per call site | One entry, N occurrences |

## When the tools are silent

Empty security output usually means the analyser does not cover that language,
not that the code is clean. Check `get_index_status` for what is indexed, and say
plainly which languages the scan actually covered.
