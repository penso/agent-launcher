# Private Advisory Dispatch

The runner exports:

```rust,ignore
pub async fn prepare_private_checkout(
    fork: &PrivateAdvisoryFork,
    root: &Path,
) -> Result<Repository, Error>;

pub async fn verify_private_checkout(
    repository: &Repository,
    fork: &PrivateAdvisoryFork,
) -> Result<(), Error>;
```

The runtime must verify the fork's privacy and advisory association with GitHub
immediately before preparation. These helpers validate local isolation and the
destination, not server-side authorization. `root` must already exist, be an
absolute directory without symlink components, and be controlled by the caller.
Preparation creates fresh UUID directories with mode 0700; existing checkout data
is never used. Non-Unix systems fail closed until equivalent ACL support exists.

Set `DispatchRequest.private_fork = Some(fork)` together with
`Issue.security_advisory = Some(metadata)`, and use the returned repository
unchanged. Ordinary requests must set `private_fork = None`. The runtime must
obtain explicit model consent before including advisory text in `prompt` and
should supply a confidential issue clone with the generic title
`private security review`. The runner independently replaces workspace names and
session titles. Branch names are `private-` followed by a random UUID in its
32-hex-digit form; a supplied canonical UUIDv4 name is preserved so it can be named
in the consented prompt, and other names are replaced. Native requires `target = None` and an empty
`NativeConfig.ssh_targets`, including when global compute placement is configured.

Native launches locally inside the standalone clone. Herdr creates a local named
workspace at that clone, not a linked worktree or a workspace based on a public
primary checkout. Superset and Conductor reject private dispatch before any
backend command. The caller must use a trusted local Herdr server and harness.
Closing a private Herdr workspace retains its checkout. Native's ordinary linked
worktree deletion must not be used to delete this standalone primary clone.
Preparation failures/cancellation remove only the directory owned by that
preparation; successful checkouts and server-side forks are never auto-deleted.

Git uses canonical HTTPS, `gh auth git-credential`, cleared inherited Git
configuration, no system/global Git config, no redirects, an empty clone template,
controlled hooks, and no local/shared objects or alternates. Preparation never
pushes and writes no advisory body. Runner registry persistence omits confidential
prompt, question, and error text. A restarted Native run cannot automatically
replay its confidential initial prompt; the consenting user must reconcile an
unconfirmed delivery. Harnesses can still persist their own transcripts.

## Transport And Cleanup Contract

Private Native servers bind loopback and receive a unique UUIDv4 password in
`OPENCODE_SERVER_PASSWORD`, never argv. Health, session, prompt, refresh, and
control requests use Basic authentication. The password is persisted only in the
owner-only registry and is omitted from session Debug output. The process
environment is not protection against the same user or root; on typical Linux
systems other users cannot read `/proc/<pid>/environ`. Harness transcripts and
provider handling remain outside this transport protection.

Private Herdr transport is supported on Unix for release **0.8.2 / protocol 20**
and **0.9.0 / protocol 22** only. Other versions (including prereleases) fail
closed pending contract review. The audited source pins are
`herdrdev/herdr@9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c` (v0.8.2) and
`herdrdev/herdr@b99002ac99b09e00b4ca692436cb15a6b0d676f1` (v0.9.0), specifically
`src/api/client.rs`, `src/api/schema.rs`, `src/api/schema/agents.rs`,
`src/api/schema/response.rs`, `src/cli/agent.rs`, `src/session.rs`, and
`src/config/io.rs`, `src/app/api/agents.rs`, `src/api/server.rs`,
`src/protocol/wire.rs`, and `src/cli/status.rs`. Initial and follow-up text goes directly over the
newline-delimited JSON `agent.prompt` API (`target` and `text`). The
`agent_prompted` response confirms submission, followed by CLI `agent wait` with
only a generic target and status flags in argv. This is not proof of review
completion. There is no argv or generic transport fallback for private text.

Socket resolution matches the release CLI: `HERDR_SOCKET_PATH` overrides
`HERDR_SESSION`; otherwise use `$XDG_CONFIG_HOME/herdr` or `$HOME/.config/herdr`
(temporary-directory fallback when HOME is absent), with `sessions/<name>` for
non-default sessions. `HERDR_CONFIG_PATH` does not relocate sockets. Debug builds
must set `HERDR_SOCKET_PATH` explicitly. Socket type, exact 0600 mode, owner UID,
and connected peer UID are checked; responses are limited to 1 MiB and exchanges
to 15 seconds. Errors never include wire content.

Runtime integration must call exported `verify_private_herdr_transport(&runner)` **before
creating a private fork or checkout**. Dispatch repeats the check before branch
or workspace creation. It selects the configured Herdr backend executable, checks
CLI status compatibility and its reported socket against the resolved IPC target,
then checks direct ping against the same exact release/protocol pair.
Once backend dispatch starts, runtime must retain and await
the dispatch future, then await `stop(run_id)` if cancellation or access revocation
occurred, before acknowledging cancellation. Preparation alone is safe to drop.
Private Herdr stop closes the workspace and retains the checkout. The provisional
workspace guard awaits cleanup on ordinary errors and schedules best-effort cleanup
on abrupt future drop; runtime shutdown, a lost create response, or an unreachable
Herdr server can defeat that best-effort path. It is not a substitute for the
cooperative runtime contract.

Private Native children are kill-on-drop and have a provisional process-group
guard until registry ownership. Abrupt cancellation before ownership kills the
server and ordinary descendants. After ownership, runtime must use `stop`; private
local children also terminate when the registry is dropped. Detached/malicious
descendants and launcher SIGKILL are not covered by RAII.

Registry files are created exclusively at mode 0600 before any bytes, synced, and
atomically renamed. New parent directories use 0700. Existing private registry
parents must already be owner-owned 0700 directories with no symlink components;
caller-selected shared/config parents are never chmod'ed. Symlink and hardlink
file targets are rejected for private writes. Use a dedicated owner-only registry
directory, not a registry file directly under a shared config root. Clone and
overall preparation deadlines are five minutes.

Private refresh does not run Herdr `agent read` or fetch Native message transcripts.
Native private refresh never automatically reconciles/replays an uncertain initial
prompt using message history; an idle, unconfirmed launch stays Starting for explicit
reconciliation. Permission/question endpoints remain necessary for NeedsInput and
control identifiers, but private question text is not retained in the registry and
status messages are generic.

## Push Guard Limits

`origin` and its `pushurl` are pinned to the exact private HTTPS URL. Git's default
push is disabled. An explicit normal push such as
`git push origin private-<32-hex-digit-uuid>` invokes the controlled `pre-push`
hook. It accepts only one matching source/destination private branch and rejects
other destinations, tags, deletions, and non-fast-forward updates. It rejects
multi-ref mirror pushes. Git does not expose the original command flags to this
hook, so a redundant `--force` on an otherwise fast-forward update or a single-ref
mirror operation is not distinguishable from the permitted update.

These are accidental-disclosure protections, **not a security sandbox**.
`--no-verify`, explicit configuration overrides, edited hooks/config, malicious
shell startup files, a malicious harness, or concurrent same-user filesystem
modification can bypass them. Do not grant a harness unrestricted trust on the
strength of this hook. Runtime/UI documentation must disclose these limits and
model/provider data handling. An app-owned push action should reverify immediately
before pushing an explicit private branch to the pinned URL, without force,
mirror, deletion, or hook-bypass options.
