# Private security reviews

The Security tab lists private GitHub advisories; dispatching one needs explicit consent.

## Security Tab

The third tab, **Security**, lists private GitHub repository advisories separately from
Issues and PRs. `Tab` moves forward and `Shift+Tab` moves backward through all three;
clicking a tab selects it directly. Each tab retains its own filter, selection, scroll,
and sort. Typing, including digits and `d`, remains search text in the inbox. Search
advisories by title, GHSA, CVE, or severity. Details show the exact identifiers, severity,
state, and **PRIVATE** label; advisory bodies are intended only for this local UI.

The advisory source uses the effective GitHub host/repository and GitHub authentication:
`GH_TOKEN`, then `GITHUB_TOKEN`, then stored `gh auth` credentials for that host. Private
advisory access requires an account with access to the repository's security advisories,
not merely access to public issues. A classic personal access token needs `repo`; a
fine-grained token needs repository **Repository security advisories: read** permission
to list advisories. Fork creation/preparation requires the corresponding write permission
and sufficient repository/advisory collaborator access. Organization policies and host
support can also restrict access. The Security empty state distinguishes no advisory
source, unavailable/unauthorized access, and a successful empty list. Its health is
independent of ordinary issue-source health.

The list includes **triage and draft** advisories, even when ordinary issue loading is
empty; closed/published advisory history is excluded. Triage reports are read-only here:
accept them manually on GitHub before preparing a draft. To dispatch a draft, open its
details and press `d`, or use `Ctrl+G`, then `d` from Security. Choose a saved prompt or
the built-in private review, then choose the harness/model and optionally append text.
Leave the instructions field with Tab, then press Enter to review the privacy warning.
There is no compute-target chooser. **Enter never grants privacy consent.** The entire warning
and captured target must fit on screen before `y` can explicitly confirm. Resize or
cancel with Esc if it does not fit. Duplicate dispatches are blocked while preparation
is pending, and the runtime revalidates the captured backend and advisory. Custom profiles
are rendered against the freshly revalidated advisory after checkout; private content is
not saved into profile files, diagnostic logs, or ordinary run history. The fixed private
workflow safeguards remain in the prompt and backend regardless of profile selection.

Initially, private dispatch supports only **local Native with no configured compute
targets, or Herdr**. Other backends and Native configurations with any compute targets
are blocked, with **no automatic public fallback**. After consent, the request may
create the advisory's temporary private fork on GitHub and prepare an isolated local
clone with private remotes only. GitHub disables CI and integrations in temporary private
forks; do not rely on the usual repository automation to validate a fix there. The launcher
does not request automatic public PR publication, advisory publication, or merging, and
does not use the normal public worktree/PR workflow. These are workflow restrictions,
not a sandbox preventing an agent from using other tools. Private-clone cleanup is a
separate operation and is currently unavailable in this UI: `x` retains the clone and
explains the restriction, while `X` source deletion is unsupported. Normal runtime
worktree preview/deletion APIs also reject confidential runs before inspection or
cleanup. Existing private runs can still be opened, stopped, and sent input.

Advisory titles, descriptions, and metadata are intentionally cached locally in a dedicated
`security_advisory_cache` SQLite table, isolated by exact `security:github:host:repo` source
key from ordinary issues and checkpoints. The snapshot and its HTTP checkpoint are replaced
atomically only after a complete successful refresh. Persistent caching requires an owner-only
`0700` repository application directory and `0600` database file; new database files are
created with these permissions before content is written. Existing owned, single-link
`0644`/`0640` databases inside an owned `0700` application directory are hardened through
a no-follow, pinned file descriptor before SQLite opens them. Shared directories and
deliberately read-only files are not made writable. Other insecure or symlinked cache
paths are refused for private caching, with live data still available in memory.

Restored advisories are shown as cached and awaiting verification. Background advisory
refreshes use a five-minute TTL based on the persisted last successful full refresh, independently
of ordinary issue polling. Explicit Refresh bypasses both runtime and source-local TTLs,
retaining page ETags for HTTP verification, but never bypasses a server rate-limit deadline.
A local cache hit is not evidence of current access and remains awaiting verification.
Failed refreshes retain the last snapshot as stale and cancel pending dispatches;
loss of advisory access during refresh or either dispatch-time revalidation removes the
source's cached rows and cancels its pending jobs. Removed sources are pruned at startup.
Cached content never authorizes dispatch: live advisory/private-fork revalidation and explicit
model-provider consent remain required.

Revocation also writes an opaque, empty `0600` marker in the private application directory.
If SQLite deletion fails, the marker blocks cache restoration across restarts. Only a
successfully committed fresh live snapshot clears it. A changed database mode does not
prevent deletion through the already-open connection. If deletion and marker persistence
both fail, the launcher warns: it cannot guarantee durable revocation or local erasure.
These are logical cache protections, not secure erasure of SQLite free pages or backups.

The cache and private checkouts are **not encrypted**. Their confidentiality relies on local
account permissions and disk protection such as FileVault; backups may retain old data.
Private run output is not written to launcher SQLite or diagnostic logs, and raw private
output is not retained as runtime event history. Advisory bodies are not diagnostic log data.
This does not make the backend or harness
memory-only: its own transcripts, sessions, and history may persist. Private checkouts use `0700`
directory permissions; this is access control, **not encryption**. Private checkouts
remain on disk. The selected harness/model provider receives the confidential advisory
and private code and may be a cloud service. Review its privacy and retention policy
before consenting. Harnesses can write their own sessions/history and other local
copies. Git remote checks and hooks are defense in depth, **not an OS sandbox**; an
agent with the user's permissions can bypass them or disclose data through other tools.
Do not automatically publish this private worktree or treat these protections as a
guarantee against disclosure.

GitHub references:
- [List repository security advisories](https://docs.github.com/en/rest/security-advisories/repository-advisories#list-repository-security-advisories)
- [Create a temporary private fork](https://docs.github.com/en/rest/security-advisories/repository-advisories#create-a-temporary-private-fork)
- [Collaborating in a temporary private fork](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/collaborating-in-a-temporary-private-fork-to-resolve-a-security-vulnerability)

## Security Reviewer Profile

Select **security reviewer** in the issue dispatch prompt chooser for a focused,
read-only security audit rather than the implementation-oriented `reviewer` profile.
The bundled template is [`prompts/security-reviewer.md`](../prompts/security-reviewer.md).
New installations seed it as the fourth default at
`~/.config/agent-launcher/agents/security reviewer/prompt.md`. Seeding happens only
when the `agents` directory is absent: existing, edited, deleted, or custom profiles
are never automatically replaced or replenished. Existing installations can add a
profile named exactly `security reviewer` using the bundled template. It can also be
selected for PR or private security dispatch, where it supplements the contextual
review prompt rather than replacing PR verification or private-workspace safeguards.

The profile establishes issue/diff scope, researches full-file context and existing
controls, and traces attacker-controlled inputs to sensitive operations. It includes
conditional Rust checks for unsafe/FFI invariants, panics, deserialization bounds,
async locks, cancellation, and resource exhaustion alongside authorization, CLI/SSH,
filesystem, web/SSRF, secret persistence, and verified dependency-advisory checks.
Reports separate severity-ranked, evidence-backed findings from hypotheses and
suggested fixes/tests, and disclose coverage gaps and tests not run. No findings does
not certify security.

It instructs the agent not to edit, autofix, stash, commit, push, upload, dump secrets,
install tools, or probe live services. Tests require explicit authorization and a
reviewed, safe isolated environment; `cargo test` is not inherently harmless because
build scripts, proc macros, and tests execute code. These are **prompt constraints,
not a read-only sandbox**: the agent harness and tool permissions must enforce the
boundary. Selecting this profile does not change backend permissions or configuration.

This is an original synthesis, not a copied upstream prompt or an upstream Rust policy:

- [Anthropic security review](https://github.com/anthropics/claude-code-security-review/blob/main/claudecode/prompts.py)
  informed high-confidence exploitability, context research, comparative controls, and
  source-to-sink analysis. Its DoS/on-disk-secret exclusions and numeric confidence
  thresholds are deliberately not adopted.
- [OpenAI security best practices](https://github.com/openai/skills/blob/main/skills/.curated/security-best-practices/SKILL.md)
  informed stack identification, severity/location reporting, and separating fixes from
  review. Its supported-language scope does not include Rust; this checklist adds Rust
  coverage and does not adopt its implementation or report-file-writing modes.
- Local inspiration: Moltis
  `crates/skills/src/assets/software-development/requesting-code-review/SKILL.md`
  for the reviewer/untrusted-code boundary (not its autofix/commit workflow), OpenCode
  `packages/core/src/plugin/command/review.txt` for full-file context and avoiding
  theoretical false positives, and Moltis `CLAUDE.md` for SSRF, secret/Debug exposure,
  async lock scope, and provenance awareness. These are references, not runtime
  dependencies or instructions to fetch content.

[← README](../README.md)
