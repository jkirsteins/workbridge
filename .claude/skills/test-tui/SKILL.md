---
name: test-tui
description: End-to-end visual test of the workbridge TUI. Builds the binary, launches it in a tmux test harness, sends keystrokes, captures pane output at each step, and reports pass/fail. Use this skill after making UI changes, after refactoring rendering code, or whenever you need to verify the TUI renders and responds correctly. Also use proactively after significant code changes to catch regressions.
---

# TUI Screenshot Test

Run an end-to-end visual test of the workbridge TUI by launching it inside a
temporary tmux session, sending keystrokes, and capturing pane output at each
step to verify the UI renders correctly.

This uses tmux purely as a test harness (to give workbridge a real PTY) - tmux
is not a runtime dependency of workbridge itself.

## Prerequisites

- `tmux` must be installed (test harness only)
- `cargo` must be available to build the binary

## Test Procedure

Use a unique tmux session name like `wb-test-<pid>` to avoid collisions.
Use a fixed terminal size (120x40) for reproducible output.

### Step 1: Build

Run `cargo build --release`. If it fails, report the error and stop.

### Step 2: Launch

```bash
tmux new-session -d -s <session> -x 120 -y 40 \
  "TERM=xterm-256color ./target/release/workbridge"
```

Wait 2 seconds for startup.

### Step 3: Test sequence

For each test case below, capture the pane after the action:

```bash
tmux capture-pane -t <session> -e -p
```

The `-e` flag preserves ANSI escape sequences so you can verify colors.

#### Test 1: Initial welcome screen

Capture immediately. Verify:
- Two bordered panels visible (left "Tabs", right "Claude Code")
- Left panel shows "No tabs." and "Press Ctrl+N"
- Right panel shows "Welcome to workbridge" and keybinding help
- Borders render correctly (no broken box-drawing characters)

#### Test 2: Create a tab

Send `Ctrl+N`:
```bash
tmux send-keys -t <session> C-n
```
Wait 3 seconds (claude needs time to start). Capture. Verify:
- Left panel shows "Tab 0" as selected (highlighted)
- Right panel shows Claude Code UI (logo, version, prompt line with `>`)
- No error messages in the status bar

#### Test 3: Input forwarding

Send `Enter` to focus right panel, then type "hello":
```bash
tmux send-keys -t <session> Enter
sleep 1
tmux send-keys -t <session> -l "hello"
sleep 1
```
Capture. Verify:
- Right panel title changes to `[INPUT]`
- Border color changes (green for focused)
- "hello" appears at the prompt line
- Status bar shows focus hint ("press Ctrl+] to return")

#### Test 4: Return to tab list

Send `Ctrl+]` (or `Ctrl+5` which crossterm maps to the same thing):
```bash
tmux send-keys -t <session> C-]
```
Wait 1 second. Capture. Verify:
- Left panel border returns to cyan (focused)
- Right panel border returns to white (unfocused)
- Right panel title no longer shows `[INPUT]`

#### Test 5: Quit

Send `q` twice (first triggers confirm, second quits):
```bash
tmux send-keys -t <session> q
sleep 0.5
tmux send-keys -t <session> q
sleep 2
```

Verify the tmux session is gone:
```bash
tmux has-session -t <session> 2>/dev/null
```
Should return non-zero (session ended because workbridge exited).

### Step 4: Cleanup

Kill the tmux session if it is still alive (e.g., a test failed mid-way):
```bash
tmux kill-session -t <session> 2>/dev/null
```

### Step 5: Report

Present results as a table:

```
| Test                  | Result | Notes                    |
|-----------------------|--------|--------------------------|
| Initial welcome       | PASS   |                          |
| Create tab            | PASS   | Claude Code logo visible |
| Input forwarding      | PASS   | "hello" at prompt        |
| Return to tab list    | PASS   |                          |
| Quit                  | PASS   | Session cleaned up       |
```

Include the raw captured pane output for any FAIL so the user can see what
went wrong.

#### Test 6: Board view toggle

Send `Tab` to switch to board view:
```bash
tmux send-keys -t <session> Tab
```
Wait 1 second. Capture. Verify:
- Four column headers visible: "Backlog", "Planning", "Implementing", "Review"
- Column borders render correctly
- Tab created in Test 2 is no longer visible (board view replaces flat list)

#### Test 7: Board view - create and move item through all stages

This test verifies no text clipping in any board column. Create a work item
with a representative title, then move it through all stages:

1. Press Tab to return to flat list view, then Ctrl+N to create:
```bash
tmux send-keys -t <session> Tab
sleep 0.5
tmux send-keys -t <session> C-n
sleep 1
tmux send-keys -t <session> -l "Add response caching layer"
sleep 0.5
tmux send-keys -t <session> Enter
sleep 1
```

2. Switch to board view:
```bash
tmux send-keys -t <session> Tab
sleep 1
```
Capture. Verify: "Add response caching layer" visible in Backlog column,
not truncated.

3. Move to Planning (Shift+Right):
```bash
tmux send-keys -t <session> S-Right
sleep 1
```
Capture. Verify: title visible in Planning column, not truncated.

4. Move to Implementing (Shift+Right):
```bash
tmux send-keys -t <session> S-Right
sleep 1
```
Capture. Verify: title visible in Implementing column, not truncated.

5. Move to Review (Shift+Right):
```bash
tmux send-keys -t <session> S-Right
sleep 1
```
Capture. Verify: title visible in Review column, not truncated.

At each step, assert the full string "Add response caching" appears in the
captured pane output (the full title may wrap but must not be truncated with
"..").

#### Test 8: Board view - return to flat list

Send Tab to return to flat list:
```bash
tmux send-keys -t <session> Tab
sleep 1
```
Capture. Verify:
- Left panel shows "Work Items" title (not column headers)
- The work item is visible in the list

## What counts as PASS

For each verification point, look for the described strings or patterns in the
captured pane output. ANSI escape sequences are present but the underlying text
content is what matters for verification. A test PASSes if all its verification
points are met.

## Failure modes

- If `claude` is not in PATH, tab creation will show a process that exits
  immediately. The tab should show as `[dead]`. This is expected if claude is
  not installed - note it but do not count it as a test failure of workbridge
  itself.
- If tmux is not installed, the test cannot run. Report this clearly.
