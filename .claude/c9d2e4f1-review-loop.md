# Claude Adversarial Review Loop - MCP/Manual UX Parity

## Requirements

**Hard invariant (from user):** No UX inconsistencies allowed between manual (keyboard) and MCP-driven state changes.

**User-facing goals:**
1. Every state transition produces identical side effects whether triggered by user or Claude via MCP
2. Activity log entries, PR creation, status messages, review gate behavior must be path-independent

**Implementation goals:**
1. MCP path must mirror all guards from manual path (status_derived, etc.)
2. Blocked state must have sensible UX for both entry (MCP) and exit (keyboard)
3. Review gate rejection should feed back into session prompt (like rework does)
4. Status messages should not lose information when MCP overrides them

## Session Config
- Base: master
- Commit strategy: amend last
- Build: cargo build
- Lint: cargo clippy
- Test: cargo test
- Format: cargo fmt --check

## Rounds

### Round 1 - Claude Adversarial Review (UX Parity focused)

[F-1] MCP path does not check status_derived (HIGH - structural divergence)
[F-2] Blocked Shift+Left shows misleading "Already at first stage" (MEDIUM)
[F-3] MCP apply_stage_change status message (PR URL) overwritten (HIGH)
[F-4] Review gate rejection doesn't populate rework prompt (MEDIUM)
[F-5] MCP "Done" still in enum schema despite being blocked (LOW)

### Round 1 - GAN Fixes

**Triage:**
- [F-1] ACCEPTED - MCP poll_mcp_status_updates lacked the status_derived guard present in advance_stage/retreat_stage
- [F-2] ACCEPTED - Blocked.prev_stage() returned None, causing misleading "Already at first stage" message
- [F-3] ACCEPTED - apply_stage_change set status_message with PR URL, then MCP handler overwrote it
- [F-4] ACCEPTED - Review gate rejection logged to activity log but did not populate rework_reasons for next session
- [F-5] REJECTED - Already fixed: MCP enum schema at mcp.rs:276 lists only [Backlog, Planning, Implementing, Blocked, Review]. The Done parse in poll_mcp_status_updates is defense-in-depth and is immediately rejected by the "Block Done via MCP" check.

**Fixes applied:**

F-1: Added status_derived check in poll_mcp_status_updates (app.rs ~line 1166) after fetching the work item reference, before transition validation. Shows "MCP: status is derived from merged PR" and continues to next event.

F-2: Changed Blocked.prev_stage() from None to Some(Implementing) in work_item.rs. Blocked is documented as a sub-state of Implementing, so "back" naturally returns to Implementing. Updated the corresponding unit test.

F-3: In poll_mcp_status_updates (app.rs ~line 1217), instead of unconditionally overwriting status_message, the code now reads the existing message from apply_stage_change, extracts any "PR created" info, and composes a combined message like "Claude moved to [RV] - PR created: URL - reason".

F-4: In poll_review_gate rejection branch (app.rs ~line 2187), added self.rework_reasons.insert(wi_id.clone(), result.detail.clone()) before the status message. The next Claude session will now use the implementing_rework prompt template with the gate's specific rejection feedback.

**Verification:** cargo fmt --check (clean), cargo clippy (clean), cargo test (226 passed, 0 failed), cargo build (success).
