# Workbridge

## Setup

### Git hooks

The `hooks/` directory contains git hooks that enforce code quality:

- **pre-commit** - runs `cargo fmt --check` and `cargo clippy` (lint + format)
- **pre-push** - checks for unstaged/untracked files, then runs `cargo test`

To enable them:

```sh
git config core.hooksPath hooks
```

This is a per-repo setting and only needs to be run once after cloning.
