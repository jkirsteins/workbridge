//! `SharedServices` - the aggregate of trait objects + config that
//! every subsystem needs read-access (or write-access) to.
//!
//! Stage 2 of the Phase 4 logical decomposition specifies that every
//! subsystem method takes `&mut SharedServices` plus any other
//! subsystem refs it needs - NOT `&mut App` - so the app-wide fan-out
//! (`backend`, `worktree_service`, `github_client`, `pr_closer`,
//! `agent_backend`, `config`, `config_provider`) is available
//! uniformly without each subsystem having to copy the field list.
//!
//! `App` now holds a single `services: SharedServices` field instead
//! of seven separate trait-object / config fields. Access sites
//! migrate from `self.backend.foo(...)` to
//! `self.services.backend.foo(...)`. Call sites that need a subsystem
//! to see the services take `&mut SharedServices` explicitly, making
//! the dependency surface visible in the signature.
//!
//! The wrapper is deliberately minimal: no methods, just owned
//! fields. Dropping it is structural (the trait-object `Arc`s and
//! the owned `Config` / `Box<dyn ConfigProvider>` clean up the same
//! way they did when they were sibling fields on `App`).

use std::sync::Arc;

use crate::agent_backend::AgentBackend;
use crate::config::{Config, ConfigProvider};
use crate::github_client::GithubClient;
use crate::pr_service::PullRequestCloser;
use crate::work_item_backend::WorkItemBackend;
use crate::worktree_service::WorktreeService;

/// Owns the app-wide services: the persistence backend, the worktree
/// and GitHub service traits, the PR-close trait, the harness
/// adapter, the loaded config, and the config persistence provider.
///
/// Every subsystem that needs to reach one of these goes through the
/// `services` field on `App`. Sibling fields are gone so there is
/// exactly one place each dependency is declared.
pub struct SharedServices {
    /// Backend for persisting work item records. Held as `Arc` rather
    /// than `Box` so background threads (PR creation, review gate,
    /// delete cleanup) can clone the handle and perform backend I/O
    /// off the UI thread - see `docs/UI.md` "Blocking I/O Prohibition"
    /// for why `backend.read_plan(...)` and similar calls must not
    /// run on the main thread.
    pub backend: Arc<dyn WorkItemBackend>,
    /// Worktree service for creating/listing worktrees.
    pub worktree_service: Arc<dyn WorktreeService + Send + Sync>,
    /// GitHub client used by the merge precheck to re-fetch the live
    /// PR mergeable flag and CI rollup before admitting a merge.
    /// Injected via the trait so tests can drive the conflict /
    /// CI-failing / no-PR / error branches without shelling out to
    /// `gh`. Production threads a `GhCliClient` in via
    /// `App::with_config_worktree_and_github`; the test-only default
    /// constructor swaps in `StubGithubClient` which always reports
    /// "no open PR" so the precheck classifier falls through to the
    /// worktree-only classification.
    pub github_client: Arc<dyn GithubClient + Send + Sync>,
    /// GitHub pull-request closer, injected via trait so the
    /// background delete-cleanup thread can be exercised in tests
    /// without shelling out to `gh`. Production uses
    /// `GhPullRequestCloser`.
    pub pr_closer: Arc<dyn PullRequestCloser>,
    /// Pluggable LLM harness adapter that knows how to build argv
    /// for the three spawn profiles (work-item, review-gate, global)
    /// and write any backend-specific side-car files. Every place
    /// that previously hard-coded `claude` flags now goes through
    /// this trait object. See `crate::agent_backend` and
    /// `docs/harness-contract.md`.
    pub agent_backend: Arc<dyn AgentBackend>,
    /// The loaded configuration (repo paths, base dirs, defaults).
    pub config: Config,
    /// Abstracts config persistence so tests use an in-memory store.
    pub config_provider: Box<dyn ConfigProvider>,
}
