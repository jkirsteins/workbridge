# Claude Adversarial Review Loop - Review Gate MCP Refactor

## Requirements

**User-facing goals:**
1. Review gate verdict should be communicated reliably (no text parsing bugs)
2. Visual feedback (spinner) in the right panel while the review gate runs

**Implementation goals:**
1. Replace `claude --print` + text parsing with `workbridge_review_gate_result` MCP tool
2. Review gate runs as a headless Claude session with MCP server
3. Spinner in right panel when gate is active
4. Tests for MCP tool, event handling, and state lifecycle

## Session Config
- Base: master
- Commit strategy: amend last
- Build: cargo build
- Lint: cargo clippy
- Test: cargo test

## Rounds

### Round 1

**Verdict:** needs-attention

**Findings:**
- [F-1] No liveness check for review gate session - silent permanent hang if Claude exits without calling MCP tool (0.95) -- ACCEPTED, FIXED
- [F-2] Review gate session excluded from shutdown path - child process leak on quit (0.9) -- ACCEPTED, FIXED
- [F-3] Re-entrant spawn_review_gate leaks prior gate's session and MCP server (0.7) -- ACCEPTED, FIXED

**Triage notes (Round 1 -> Round 2):**
- F-1: Added liveness check for review_gate_session in check_liveness(). If the session dies without delivering a verdict, gate state is cleared and an error message is surfaced.
- F-2: Included review_gate_session in send_sigterm_all(), all_dead(), and force_kill_all(). Gate state is fully cleaned up on force kill.
- F-3: Added guard at top of spawn_review_gate() - returns false if review_gate_wi is already Some.

### Round 2

**Verdict:** needs-attention

**Findings:**
- [F-4] Review gate bypass - workbridge_review_gate_result callable from any session, handler doesn't verify gate is running (0.9) -- ACCEPTED, FIXED

**Triage notes (Round 2 -> Round 3):**
- F-4: Added guard at top of McpEvent::ReviewGateResult handler (after parsing wi_id, before clearing state). If self.review_gate_wi does not match the incoming work item ID, the event is silently discarded via continue. This prevents any session from self-approving when no gate is running or when the gate is running for a different work item.

### Round 3

**Verdict:** needs-attention

**Findings:**
- [F-5] Gate bypass via second set_status("Review") while gate running - spawn_review_gate returns false (already running), callers treat false as "no gate needed" and fall through to apply_stage_change (0.95) -- ACCEPTED, FIXED

**Triage notes (Round 3 -> Round 4):**
- F-5: Added early-return guards in BOTH callers (poll_mcp_status_updates and advance_stage) BEFORE calling spawn_review_gate. If review_gate_wi is already set for the work item, the caller returns/continues without advancing to apply_stage_change. This prevents a second set_status("Review") from bypassing the gate while it is already running.

### Round 4

**Verdict:** needs-attention

**Findings:**
- [F-6] Race between check_liveness and poll_mcp_status_updates loses valid gate verdicts (0.92) -- ACCEPTED, FIXED
- [F-7] Implementing session can self-approve via same work item's MCP (0.85) -- ACCEPTED, FIXED
- [F-8] Review gate overwrites implementing session's .mcp.json (0.80) -- ACCEPTED, FIXED

**Triage notes (Round 4 -> Round 5):**
- F-6: Swapped call order in salsa.rs timer handler - poll_mcp_status_updates() now runs BEFORE check_liveness(). This ensures a review gate verdict arriving in the same tick as session exit is processed before the dead-session check clears review_gate_wi.
- F-7: The workbridge_review_gate_result tool is now only advertised in tools/list when context_json contains "stage": "ReviewGate". Non-gate sessions never see the tool. Additionally, the tools/call handler for this tool verifies the context and returns error -32601 if a non-gate session attempts to call it directly. Added tests for both behaviors.
- F-8: Removed the .mcp.json write from spawn_review_gate. The gate session gets its MCP config via --mcp-config (temp file), so writing .mcp.json to the worktree cwd was unnecessary and overwrote the implementing session's config.
