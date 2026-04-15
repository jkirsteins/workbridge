# Session Authorizations

This directory is the persistent record of session-scoped
authorizations for edits to otherwise-immutable project documents,
primarily `docs/invariants.md`. The mechanism exists because
`CLAUDE.md` treats `docs/invariants.md` as effectively immutable:
any edit is P0 unless covered by a specific, recorded session
authorization that names the exact bullet touched, the user's
rationale, and the scope boundary. Blanket "do whatever"
authorizations do not count; the authorization must be specific
enough that an adversarial reviewer reading only its text can
verify the edit matches.

## Why a separate directory

An authorization cannot sanction itself. If the authorization lived
inside `docs/invariants.md` (the very file it authorizes), any
forbidden edit could justify itself in the same diff, which
collapses the check to nothing. Moving the authorization to a
separate, append-only file under `docs/session-authorizations/`
gives adversarial reviewers (human or automated) a clear external
audit trail: the authorizing text lives in a file that is not the
immutable document. Reviewers can independently verify that the
changes to `docs/invariants.md` match the scope stated in the
authorization file, without having to trust any self-referential
documentation inside the edited file itself.

## Format

One file per authorization, named `YYYY-MM-DD-<scope>.md`. Each file
is append-only - once a pull request has been reviewed and the
authorization has been committed, do not rewrite or remove it. If
a subsequent edit refines or supersedes a prior authorization, add
a new file and reference the older one from its body.

Every authorization file MUST include:

- **Date and PR / work-item ID** - so reviewers can find the
  conversation transcript and the related code changes.
- **Bullet(s) touched** - the exact section, heading, or line-range
  being modified. Be specific enough that a reviewer can verify the
  diff matches.
- **User rationale** - quoted verbatim from the user's message when
  possible, so no paraphrasing or drift obscures intent.
- **Scope boundary** - what this authorization does NOT cover.
  Explicitly call out adjacent rules that are NOT relaxed, so a
  reviewer cannot reasonably read the authorization as a blanket
  license.
- **Authorization channel** - how the authorization was obtained
  (Codex adversarial review follow-up, claude-adversarial-loop
  session, direct user instruction in review). This lets future
  reviewers audit whether the authorization process was honoured.

## Current entries

- `2026-04-15-pr91-invariant-13.md` - authorizes relaxing the
  "one fresh Claude session per stage transition" contract for the
  restart-resume case within the same `(WorkItemId,
  WorkItemStatus)` tuple. Stage transitions still produce new
  deterministic UUIDs, so cross-stage isolation is preserved.
