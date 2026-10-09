//! Accidental-disclosure guards, not a sandbox for a malicious harness or local user.
//! The caller must verify the advisory/fork association and obtain model consent.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use agent_launcher_core::{
    BackendKind, IssueProvider, PrivateAdvisoryFork, Repository, RepositoryRemote,
};
use tokio::process::Command;
use uuid::Uuid;

use crate::{DispatchRequest, Error, Result};

pub(crate) const TITLE: &str = "private security review";
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(300);
const CLONE_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) async fn redact_errors<T>(
    confidential: bool,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    operation
        .await
        .map_err(|error| error.for_private(confidential))
}

fn remote(fork: &PrivateAdvisoryFork) -> Result<RepositoryRemote> {
    let host = &fork.host;
    let (dns, port) = host
        .split_once(':')
        .map_or((host.as_str(), None), |(h, p)| (h, Some(p)));
    if fork.id == 0
        || dns.len() > 253
        || dns.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        || port.is_some_and(|p| {
            !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u16>().map_or(true, |p| p == 0)
        })
    {
        return Err(Error::PrivateSecurity);
    }
    let parts: Vec<_> = fork.full_name.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || p.starts_with('-')
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        || fork.full_name.ends_with(".git")
    {
        return Err(Error::PrivateSecurity);
    }
    Ok(RepositoryRemote {
        name: "origin".into(),
        url: format!("https://{host}/{}.git", fork.full_name),
        host: host.clone(),
        repository: fork.full_name.clone(),
        provider: IssueProvider::Github,
    })
}

pub(crate) fn validate_request(request: &DispatchRequest) -> Result<()> {
    match (&request.issue.security_advisory, &request.private_fork) {
        (None, None) => Ok(()),
        (Some(_), Some(fork))
            if request.repository.remote.as_ref() == Some(&remote(fork)?)
                && request.target.is_none() =>
        {
            Ok(())
        },
        _ => Err(Error::PrivateSecurity),
    }
}

pub(crate) async fn guard_dispatch(request: &DispatchRequest, backend: BackendKind) -> Result<()> {
    validate_request(request)?;
    if let Some(fork) = &request.private_fork {
        if !matches!(backend, BackendKind::Native | BackendKind::Herdr) {
            return Err(Error::PrivateSecurity);
        }
        verify_private_checkout(&request.repository, fork).await?;
    }
    Ok(())
}

/// Remove inherited Git execution/configuration controls, retaining normal gh authentication.
pub(crate) fn clean_environment(command: &mut Command) {
    let keys: Vec<_> = std::env::vars_os()
        .map(|(key, _)| key)
        .chain(command.as_std().get_envs().map(|(key, _)| key.to_owned()))
        .collect();
    for key in keys {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env_remove("SSH_ASKPASS")
        .env_remove("GCM_INTERACTIVE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1");
}

fn git(path: &Path) -> Command {
    git_program(path, Path::new("git"))
}

fn git_program(path: &Path, program: &Path) -> Command {
    let mut command = Command::new(program);
    clean_environment(&mut command);
    command
        .current_dir(path)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .args([
            "-c",
            "credential.helper=",
            "-c",
            "credential.helper=!gh auth git-credential",
            "-c",
            "protocol.file.allow=never",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "http.followRedirects=false",
        ]);
    command
}

async fn output(command: &mut Command) -> Result<String> {
    output_with_timeout(command, Duration::from_secs(30)).await
}

async fn output_with_timeout(command: &mut Command, timeout: Duration) -> Result<String> {
    let result = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| Error::PrivateSecurity)?
        .map_err(|_| Error::PrivateSecurity)?;
    if !result.status.success() {
        return Err(Error::PrivateSecurity);
    }
    String::from_utf8(result.stdout).map_err(|_| Error::PrivateSecurity)
}

fn private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| Error::PrivateSecurity)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(Error::PrivateSecurity)
    }
}

fn no_symlinks(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::PrivateSecurity);
    }
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor).map_err(|_| Error::PrivateSecurity)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::PrivateSecurity);
        }
    }
    Ok(())
}

fn config(url: &str, hooks: &Path) -> Result<String> {
    let hooks = hooks.to_str().ok_or(Error::PrivateSecurity)?;
    if hooks.chars().any(char::is_control) {
        return Err(Error::PrivateSecurity);
    }
    let hooks = hooks.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!(
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\thooksPath = \"{hooks}\"\n\tlogAllRefUpdates = false\n[remote \"origin\"]\n\turl = {url}\n\tpushurl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[push]\n\tdefault = nothing\n[protocol]\n\tallow = never\n[protocol \"https\"]\n\tallow = always\n[protocol \"file\"]\n\tallow = never\n[protocol \"ext\"]\n\tallow = never\n[http]\n\tfollowRedirects = false\n[credential]\n\thelper =\n\thelper = !gh auth git-credential\n[submodule]\n\trecurse = false\n"
    ))
}

fn hook(url: &str) -> String {
    format!(
        r#"#!/bin/sh
# Guard against accidental disclosure; --no-verify/config overrides can bypass this.
test "$2" = '{url}' || exit 1
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS
count=0
while read -r local_ref local_oid remote_ref remote_oid; do
    count=$((count + 1))
    test "$count" = 1 || exit 1
    if test "$local_ref" = HEAD; then
        test "$(git symbolic-ref -q HEAD)" = "$remote_ref" || exit 1
        test "$(git rev-parse --verify HEAD)" = "$local_oid" || exit 1
    else
        test "$local_ref" = "$remote_ref" || exit 1
    fi
    case "$remote_ref" in refs/heads/private-*) ;; *) exit 1 ;; esac
    suffix=${{remote_ref#refs/heads/private-}}
    test "${{#suffix}}" = 32 || exit 1
    case "$suffix" in *[!0-9a-f]*) exit 1 ;; esac
    case "$local_oid" in *[!0]*) ;; *) exit 1 ;; esac
    case "$remote_oid" in
        *[!0]*) git merge-base --is-ancestor "$remote_oid" "$local_oid" || exit 1 ;;
    esac
done
test "$count" = 1
"#
    )
}

struct OwnedDirectory(Option<PathBuf>);
impl Drop for OwnedDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Clone only a caller-verified private advisory fork, without any public checkout data.
/// `root` must already exist and have no symlink components. No push is performed.
pub async fn prepare_private_checkout(
    fork: &PrivateAdvisoryFork,
    root: &Path,
) -> Result<Repository> {
    prepare_using(fork, root, Path::new("git")).await
}

async fn prepare_using(
    fork: &PrivateAdvisoryFork,
    root: &Path,
    clone_program: &Path,
) -> Result<Repository> {
    let remote = remote(fork)?;
    no_symlinks(root)?;
    let owned = root.join(Uuid::new_v4().simple().to_string());
    private_dir(&owned)?;
    let mut cleanup = OwnedDirectory(Some(owned.clone()));
    let result = tokio::time::timeout(PREPARATION_TIMEOUT, async {
        let template = owned.join("template");
        let hooks = owned.join("hooks");
        let checkout = owned.join(Uuid::new_v4().simple().to_string());
        private_dir(&template)?;
        private_dir(&hooks)?;
        private_dir(&checkout)?;
        let mut clone = git_program(&owned, clone_program);
        clone
            .args([
                "clone",
                "--no-checkout",
                "--no-local",
                "--no-recurse-submodules",
            ])
            .arg(format!("--template={}", template.display()))
            .args([
                "--config",
                &format!("core.hooksPath={}", hooks.display()),
                "--config",
                "protocol.file.allow=never",
                "--config",
                "protocol.ext.allow=never",
                "--config",
                "push.default=nothing",
                "--config",
                "core.logAllRefUpdates=false",
            ])
            .arg("--")
            .arg(&remote.url)
            .arg(&checkout);
        output_with_timeout(&mut clone, CLONE_TIMEOUT).await?;
        let repository = Repository {
            git_dir: checkout.join(".git"),
            root: checkout,
            remote: Some(remote.clone()),
            has_beads: false,
        };
        std::fs::write(
            repository.git_dir.join("config"),
            config(&remote.url, &hooks)?,
        )
        .map_err(|_| Error::PrivateSecurity)?;
        let hook_path = hooks.join("pre-push");
        std::fs::write(&hook_path, hook(&remote.url)).map_err(|_| Error::PrivateSecurity)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error::PrivateSecurity)?;
        }
        verify_private_checkout(&repository, fork).await?;
        output(git(&repository.root).args([
            "check-ref-format",
            &format!("refs/heads/{}", fork.default_branch),
        ]))
        .await?;
        output(git(&repository.root).args([
            "checkout",
            "--detach",
            "--no-recurse-submodules",
            &format!("refs/remotes/origin/{}", fork.default_branch),
            "--",
        ]))
        .await?;
        Ok::<_, Error>(repository)
    })
    .await
    .map_err(|_| Error::PrivateSecurity)??;
    cleanup.0 = None;
    Ok(result)
}

/// Verify the standalone clone and its exact controlled configuration before launching.
/// This checks local isolation, not server-side privacy or the advisory/fork association.
pub async fn verify_private_checkout(
    repository: &Repository,
    fork: &PrivateAdvisoryFork,
) -> Result<()> {
    let remote = remote(fork)?;
    if repository.remote.as_ref() != Some(&remote)
        || repository.git_dir != repository.root.join(".git")
    {
        return Err(Error::PrivateSecurity);
    }
    no_symlinks(&repository.git_dir)?;
    let parent = repository.root.parent().ok_or(Error::PrivateSecurity)?;
    let hooks = parent.join("hooks");
    no_symlinks(&hooks)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [parent, repository.root.as_path(), hooks.as_path()] {
            if std::fs::metadata(path)
                .map_err(|_| Error::PrivateSecurity)?
                .permissions()
                .mode()
                & 0o777
                != 0o700
            {
                return Err(Error::PrivateSecurity);
            }
        }
    }
    #[cfg(not(unix))]
    {
        return Err(Error::PrivateSecurity);
    }
    for (path, expected) in [
        (
            repository.git_dir.join("config"),
            config(&remote.url, &hooks)?,
        ),
        (hooks.join("pre-push"), hook(&remote.url)),
    ] {
        no_symlinks(&path)?;
        if std::fs::read_to_string(path).map_err(|_| Error::PrivateSecurity)? != expected {
            return Err(Error::PrivateSecurity);
        }
    }
    let entries = std::fs::read_dir(&hooks).map_err(|_| Error::PrivateSecurity)?;
    for entry in entries {
        if entry.map_err(|_| Error::PrivateSecurity)?.file_name() != "pre-push" {
            return Err(Error::PrivateSecurity);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(hooks.join("pre-push"))
            .map_err(|_| Error::PrivateSecurity)?
            .permissions()
            .mode()
            & 0o777
            != 0o700
        {
            return Err(Error::PrivateSecurity);
        }
    }
    let mut directories = vec![repository.git_dir.clone()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).map_err(|_| Error::PrivateSecurity)? {
            let entry = entry.map_err(|_| Error::PrivateSecurity)?;
            let metadata = entry.metadata().map_err(|_| Error::PrivateSecurity)?;
            if entry
                .file_type()
                .map_err(|_| Error::PrivateSecurity)?
                .is_symlink()
            {
                return Err(Error::PrivateSecurity);
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.is_file() && metadata.nlink() != 1 {
                    return Err(Error::PrivateSecurity);
                }
            }
        }
    }
    for file in [
        "commondir",
        "objects/info/alternates",
        "objects/info/http-alternates",
        "config.worktree",
    ] {
        if std::fs::symlink_metadata(repository.git_dir.join(file)).is_ok() {
            return Err(Error::PrivateSecurity);
        }
    }
    // No includes, URL rewrites, custom filters, extra remotes or worktree overrides can
    // survive the exact config comparison above; query Git as an independent check.
    let remotes = output(git(&repository.root).args([
        "config",
        "--local",
        "--no-includes",
        "--get-regexp",
        "^remote\\.",
    ]))
    .await?;
    let expected = format!(
        "remote.origin.url {}\nremote.origin.pushurl {}\nremote.origin.fetch +refs/heads/*:refs/remotes/origin/*\n",
        remote.url, remote.url
    );
    if remotes != expected {
        return Err(Error::PrivateSecurity);
    }
    let common = output(git(&repository.root).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]))
    .await?;
    if Path::new(common.trim_end()) != repository.git_dir {
        return Err(Error::PrivateSecurity);
    }
    Ok(())
}

pub(crate) async fn private_branch(request: &mut DispatchRequest) -> Result<()> {
    if let Some(fork) = &request.private_fork {
        verify_private_checkout(&request.repository, fork).await?;
        // Runtime may have already named this branch in the consented prompt.
        let branch = request
            .branch
            .as_ref()
            .filter(|branch| {
                branch.strip_prefix("private-").is_some_and(|suffix| {
                    suffix.len() == 32
                        && suffix
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                        && Uuid::parse_str(suffix).is_ok_and(|id| {
                            id.get_version_num() == 4 && id.get_variant() == uuid::Variant::RFC4122
                        })
                })
            })
            .cloned()
            .unwrap_or_else(|| format!("private-{}", Uuid::new_v4().simple()));
        output(git(&request.repository.root).args([
            "checkout",
            "--no-recurse-submodules",
            "--no-track",
            "-b",
            &branch,
            "HEAD",
            "--",
        ]))
        .await?;
        request.branch = Some(branch);
        request.base_branch = None;
        request.workspace_name = Some(TITLE.into());
        request.issue.title = TITLE.into();
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::{
        Backend, ConductorBackend, ConductorConfig, NativeBackend, NativeConfig, NativeSshConfig,
        Runner, SessionRegistry, SupersetBackend, SupersetConfig,
    };

    fn fork() -> PrivateAdvisoryFork {
        PrivateAdvisoryFork {
            id: 1,
            host: "github.example:8443".into(),
            full_name: "owner/private-fork".into(),
            default_branch: "main".into(),
        }
    }

    fn temp() -> OwnedDirectory {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("runner-private-test-{}", Uuid::new_v4().simple()));
        private_dir(&root).unwrap();
        OwnedDirectory(Some(root))
    }

    fn executable(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    async fn fixture(root: &Path) -> Repository {
        let fake = root.join("fake-git");
        // The clone executable records literal argv/environment and creates only a
        // local fixture. All verification/checkout operations use real Git.
        executable(
            &fake,
            r#"#!/bin/sh
set -eu
printf '%s\n' "$@" > clone-argv
env > clone-env
for destination do :; done
git init --quiet --template=template "$destination"
git -C "$destination" -c core.hooksPath=/dev/null -c user.name=Fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false commit --quiet --allow-empty -m fixture
git -C "$destination" update-ref refs/remotes/origin/main HEAD
"#,
        );
        prepare_using(&fork(), root, &fake).await.unwrap()
    }

    fn request(repository: Repository) -> DispatchRequest {
        DispatchRequest {
            repository, private_fork: Some(fork()),
            issue: serde_json::from_value(serde_json::json!({
                "key": {"provider": "github", "host": "github.example:8443", "repository": "owner/public", "native_id": "GHSA-test-test-test"},
                "identifier": "GHSA-test-test-test", "title": "secret vulnerability summary", "state": "open", "labels": [], "blocked_by": [],
                "security_advisory": {"ghsa_id": "GHSA-test-test-test", "cve_id": null, "severity": null}
            })).unwrap(),
            prompt: "consented private review".into(), agent: "opencode".into(), branch: Some("public-leak".into()),
            workspace_name: Some("secret-title".into()), base_branch: Some("public/main".into()),
            model: None, effort: None, target: None,
        }
    }

    #[tokio::test]
    async fn native_private_auth_and_delete_guards_use_only_local_fixture() {
        let temp = temp();
        let root = temp.0.as_ref().unwrap();
        let repository = fixture(root).await;
        let fake = root.join("http-fixture");
        executable(
            &fake,
            r#"#!/usr/bin/env python3
import base64, http.server, json, os, sys, time
assert os.environ['OPENCODE_SERVER_USERNAME'] == 'opencode'
password = os.environ['OPENCODE_SERVER_PASSWORD']
assert len(password) == 32 and password not in ' '.join(sys.argv)
with open('argv', 'w') as f: json.dump(sys.argv, f)
with open('server-pid', 'w') as f: f.write(str(os.getpid()))
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self): self.reply()
    def do_POST(self): self.reply()
    def reply(self):
        expected = 'Basic ' + base64.b64encode(('opencode:' + password).encode()).decode()
        authorized = self.headers.get('Authorization') == expected
        if not authorized:
            self.send_response(401); self.end_headers(); return
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        with open('requests', 'a') as f: f.write(self.path + '\n')
        if self.path == '/session' and os.path.exists('block-session'):
            with open('session-waiting', 'w') as f: f.write('ready')
            time.sleep(60)
        result = {'id': 'fixture-session'} if self.path == '/session' else {}
        payload = json.dumps(result).encode()
        self.send_response(200); self.send_header('Content-Length', str(len(payload))); self.end_headers()
        self.wfile.write(payload)
http.server.HTTPServer(('127.0.0.1', int(sys.argv[-1])), Handler).serve_forever()
"#,
        );
        let registry = SessionRegistry::load(Some(root.join("registry/sessions.json")))
            .await
            .unwrap();
        let backend = std::sync::Arc::new(NativeBackend::new(
            NativeConfig {
                opencode_executable: fake,
                // A freshly written script can take many seconds to first start on
                // CI runners (macOS scans new executables); the product default is 15s.
                startup_timeout: std::time::Duration::from_secs(60),
                ..Default::default()
            },
            registry.clone(),
        ));
        let checkout = repository.root.clone();
        let mut request = request(repository);
        request.prompt = "PRIVATE_SENTINEL_NOT_ARGV".into();
        let dispatched = backend.dispatch(request.clone()).await.unwrap();
        let id = &dispatched.run.id;
        let before = registry.get(id).await.unwrap();
        let crate::registry::BackendSession::Native {
            base_url,
            server_password,
            ..
        } = &before.session
        else {
            panic!()
        };
        assert!(server_password.is_some());
        assert!(!format!("{:?}", before.session).contains(server_password.as_deref().unwrap()));
        let response = reqwest::Client::new()
            .get(format!("{base_url}session/fixture-session"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        let status = backend.refresh(id).await.unwrap();
        assert!(status.output.is_none());
        let mut pending = registry.get(id).await.unwrap();
        pending.summary.state = agent_launcher_core::RunState::Starting;
        if let crate::registry::BackendSession::Native { initial_prompt, .. } = &mut pending.session
        {
            *initial_prompt = Some(
                serde_json::json!({"messageID": "unconfirmed", "parts": [{"text": "PRIVATE_UNCONFIRMED_SENTINEL"}]}),
            );
        }
        registry.update(pending).await.unwrap();
        let status = backend.refresh(id).await.unwrap();
        assert!(status.output.is_none());
        assert_eq!(status.run.state, agent_launcher_core::RunState::Starting);
        backend
            .send_input(id, "PRIVATE_FOLLOWUP_NOT_ARGV")
            .await
            .unwrap();
        assert!(backend.inspect_worktree(id).await.is_err());
        for force in [false, true] {
            assert!(backend.delete_worktree(id, force, None).await.is_err());
        }
        assert!(registry.get(id).await.unwrap().deletion.is_none());
        assert!(checkout.join(".git").exists());
        assert!(
            registry
                .children
                .lock()
                .await
                .get_mut(id)
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
        let argv = std::fs::read_to_string(checkout.join("argv")).unwrap();
        assert!(!argv.contains("PRIVATE_"));
        assert!(!argv.contains(server_password.as_deref().unwrap()));
        backend.stop(id).await.unwrap();
        let paths = std::fs::read_to_string(checkout.join("requests")).unwrap();
        assert!(!paths.lines().any(|path| path.contains("/message")));
        assert_eq!(
            paths
                .lines()
                .filter(|path| path.ends_with("/prompt_async"))
                .count(),
            2
        );
        for path in [
            "/global/health",
            "/session",
            "/session/fixture-session/prompt_async",
            "/session/status",
            "/session/fixture-session/abort",
        ] {
            assert!(
                paths.lines().any(|line| line == path),
                "missing authenticated request: {path}"
            );
        }
        // Drop after authenticated readiness, while session creation is in flight.
        std::fs::write(checkout.join("block-session"), "").unwrap();
        let launching = backend.clone();
        let task = tokio::spawn(async move { launching.dispatch(request).await });
        // This dispatch starts a new fixture server: allow it the same slow first
        // start as the fixture's startup timeout.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !checkout.join("session-waiting").exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(checkout.join("session-waiting").exists());
        let pid = std::fs::read_to_string(checkout.join("server-pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        for _ in 0..200 {
            if rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap())
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("server survived cancellation during session creation");
    }

    #[test]
    fn validates_only_literal_https_destinations() {
        for host in [
            "",
            "-bad.example",
            "good.example/path",
            "good.example?token=x",
            "user@host",
            "host:22:3",
            "host:+443",
            "host:0",
            "host\n",
            "host%2fother",
        ] {
            let mut fork = fork();
            fork.host = host.into();
            assert!(remote(&fork).is_err(), "{host:?}");
        }
        for name in [
            "owner/../public",
            "owner/repo?x",
            "owner/repo#x",
            "owner/repo.git",
            "owner/repo%2fpublic",
            "owner/..",
            "owner/-repo",
        ] {
            let mut fork = fork();
            fork.full_name = name.into();
            assert!(remote(&fork).is_err(), "{name:?}");
        }
        assert_eq!(
            remote(&fork()).unwrap().url,
            "https://github.example:8443/owner/private-fork.git"
        );
    }

    #[test]
    fn clears_poisoned_git_environment_without_clearing_auth() {
        let mut command = Command::new("git");
        let poisoned = [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_INDEX_FILE",
            "GIT_EXEC_PATH",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
        ];
        for key in poisoned {
            command.env(key, "poison");
        }
        command.env("GH_TOKEN", "test-only-auth");
        clean_environment(&mut command);
        let env: std::collections::HashMap<_, _> = command.as_std().get_envs().collect();
        for key in poisoned {
            assert_eq!(env.get(std::ffi::OsStr::new(key)), Some(&None), "{key}");
        }
        assert_eq!(
            env[std::ffi::OsStr::new("GH_TOKEN")],
            Some(std::ffi::OsStr::new("test-only-auth"))
        );
        assert_eq!(
            env[std::ffi::OsStr::new("GIT_CONFIG_GLOBAL")],
            Some(std::ffi::OsStr::new("/dev/null"))
        );
    }

    #[tokio::test]
    async fn isolated_fixture_clone_has_pinned_config_and_opaque_branch() {
        let temp = temp();
        let repository = fixture(temp.0.as_ref().unwrap()).await;
        verify_private_checkout(&repository, &fork()).await.unwrap();
        let parent = repository.root.parent().unwrap();
        let argv = std::fs::read_to_string(parent.join("clone-argv")).unwrap();
        for argument in [
            "--no-checkout",
            "--no-local",
            "--no-recurse-submodules",
            "credential.helper=",
            "credential.helper=!gh auth git-credential",
            "protocol.file.allow=never",
            "protocol.ext.allow=never",
            "push.default=nothing",
            "core.logAllRefUpdates=false",
            &remote(&fork()).unwrap().url,
        ] {
            assert!(argv.lines().any(|line| line == argument), "{argument}");
        }
        assert!(argv.lines().any(|line| line.starts_with("--template=")));
        let env = std::fs::read_to_string(parent.join("clone-env")).unwrap();
        assert!(env.contains("GIT_CONFIG_GLOBAL=/dev/null\n"));
        let mut request = request(repository);
        private_branch(&mut request).await.unwrap();
        let branch = request.branch.as_deref().unwrap();
        assert_eq!(branch.len(), "private-".len() + 32);
        assert!(branch.starts_with("private-"));
        assert_eq!(request.workspace_name.as_deref(), Some(TITLE));
        assert_eq!(request.issue.title, TITLE);
        assert!(request.base_branch.is_none());
        verify_private_checkout(&request.repository, &fork())
            .await
            .unwrap();
        let supplied = format!("private-{}", Uuid::new_v4().simple());
        request.branch = Some(supplied.clone());
        private_branch(&mut request).await.unwrap();
        assert_eq!(request.branch.as_deref(), Some(supplied.as_str()));
    }

    #[tokio::test]
    async fn poisoned_environment_cannot_rewrite_url_or_execute_checkout_hook() {
        let temp = temp();
        let root = temp.0.as_ref().unwrap();
        let repository = fixture(root).await;
        let malicious_hooks = root.join("malicious-hooks");
        private_dir(&malicious_hooks).unwrap();
        let marker = root.join("hook-ran");
        executable(
            &malicious_hooks.join("post-checkout"),
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let global = root.join("global-config");
        std::fs::write(
            &global,
            "[url \"https://public.example/\"]\n insteadOf = https://github.example:8443/\n",
        )
        .unwrap();
        for args in [vec!["remote", "get-url", "origin"], vec![
            "checkout", "--detach", "HEAD", "--",
        ]] {
            let mut command = git(&repository.root);
            command
                .args(&args)
                .env("GIT_CONFIG_GLOBAL", &global)
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.hooksPath")
                .env("GIT_CONFIG_VALUE_0", &malicious_hooks)
                .env("GIT_DIR", "/nonexistent/public.git")
                .env("GIT_WORK_TREE", "/nonexistent/public");
            clean_environment(&mut command);
            let result = output(&mut command).await.unwrap();
            if args[0] == "remote" {
                assert_eq!(result.trim(), remote(&fork()).unwrap().url);
            }
        }
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn rejects_changed_config_extra_hooks_alternates_and_public_targets() {
        let temp = temp();
        let repository = fixture(temp.0.as_ref().unwrap()).await;
        let config_path = repository.git_dir.join("config");
        let original = std::fs::read_to_string(&config_path).unwrap();
        for extra in [
            "[url \"https://public.example/\"]\n insteadOf = https://github.example:8443/\n",
            "[remote \"public\"]\n url = https://public.example/a/b\n",
            "[include]\n path = /tmp/unsafe\n",
            "[filter \"lfs\"]\n process = unsafe-command\n",
            "[core]\n hooksPath = /tmp/unsafe\n",
        ] {
            std::fs::write(&config_path, format!("{original}{extra}")).unwrap();
            assert!(matches!(
                verify_private_checkout(&repository, &fork()).await,
                Err(Error::PrivateSecurity)
            ));
        }
        std::fs::write(&config_path, original).unwrap();
        let extra = repository
            .root
            .parent()
            .unwrap()
            .join("hooks/post-checkout");
        executable(&extra, "#!/bin/sh\nexit 99\n");
        assert!(verify_private_checkout(&repository, &fork()).await.is_err());
        std::fs::remove_file(extra).unwrap();
        let alternate = repository.git_dir.join("objects/info/alternates");
        std::fs::write(&alternate, "/public/objects\n").unwrap();
        assert!(verify_private_checkout(&repository, &fork()).await.is_err());
        std::fs::remove_file(alternate).unwrap();
        let mut public = repository.clone();
        public.remote.as_mut().unwrap().repository = "owner/public".into();
        assert!(verify_private_checkout(&public, &fork()).await.is_err());
        let mut request = request(repository);
        request.private_fork = None;
        assert!(matches!(request.validate(), Err(Error::PrivateSecurity)));
        request.private_fork = Some(fork());
        request.issue.security_advisory = None;
        assert!(matches!(request.validate(), Err(Error::PrivateSecurity)));
    }

    #[tokio::test]
    async fn rejects_symlink_roots_and_cleans_only_owned_failed_clone() {
        let temp = temp();
        let root = temp.0.as_ref().unwrap();
        let link = root.join("link");
        symlink(root, &link).unwrap();
        assert!(
            prepare_using(&fork(), &link, Path::new("missing"))
                .await
                .is_err()
        );
        let sentinel = root.join("keep");
        std::fs::write(&sentinel, "keep").unwrap();
        assert!(
            prepare_using(&fork(), root, Path::new("missing"))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep");
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn cancelling_preparation_kills_clone_and_removes_owned_directory() {
        let temp = temp();
        let root = temp.0.as_ref().unwrap();
        let fake = root.join("sleeping-git");
        executable(
            &fake,
            "#!/bin/sh\nprintf '%s' $$ > ../child-pid\nexec sleep 60\n",
        );
        let task_root = root.clone();
        let task = tokio::spawn(async move { prepare_using(&fork(), &task_root, &fake).await });
        let pid_path = root.join("child-pid");
        for _ in 0..200 {
            if pid_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = std::fs::read_to_string(&pid_path).unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        for _ in 0..200 {
            let alive = Command::new("/bin/kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .await
                .unwrap()
                .success();
            if !alive {
                assert_eq!(std::fs::read_dir(root).unwrap().count(), 2);
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("cancelled clone remained alive");
    }

    #[tokio::test]
    async fn unsupported_backends_and_remote_native_fail_before_commands() {
        let temp = temp();
        let root = temp.0.as_ref().unwrap();
        let registry = SessionRegistry::load(Some(root.join("registry.json")))
            .await
            .unwrap();
        let repository = Repository {
            root: root.join("missing"),
            git_dir: root.join("missing/.git"),
            remote: Some(remote(&fork()).unwrap()),
            has_beads: false,
        };
        let request = request(repository);
        let runner = Runner::new([]);
        for backend in [BackendKind::Superset, BackendKind::Conductor] {
            assert!(matches!(
                runner.dispatch(backend, request.clone()).await,
                Err(Error::PrivateSecurity)
            ));
        }
        let superset = SupersetBackend::new(
            SupersetConfig {
                executable: root.join("missing"),
                ..Default::default()
            },
            registry.clone(),
        );
        assert!(matches!(
            superset.dispatch(request.clone()).await,
            Err(Error::PrivateSecurity)
        ));
        let conductor = ConductorBackend::new(ConductorConfig::default(), registry.clone());
        assert!(matches!(
            conductor.dispatch(request.clone()).await,
            Err(Error::PrivateSecurity)
        ));
        let native = NativeBackend::new(
            NativeConfig {
                ssh_targets: vec![NativeSshConfig {
                    id: "remote".into(),
                    name: "remote".into(),
                    destination: "never-connect".into(),
                    workspace_root: root.clone(),
                    max_active_runs: None,
                    wake: None,
                }],
                ..Default::default()
            },
            registry.clone(),
        );
        assert!(matches!(
            native.dispatch(request).await,
            Err(Error::PrivateSecurity)
        ));
        assert!(registry.summaries().await.is_empty());
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn push_hook_accepts_only_private_non_deleting_fast_forward_refs() {
        use tokio::io::AsyncWriteExt;
        let temp = temp();
        let repository = fixture(temp.0.as_ref().unwrap()).await;
        let hook_path = repository.root.parent().unwrap().join("hooks/pre-push");
        let reference = format!("refs/heads/private-{}", Uuid::new_v4().simple());
        let oid = output(git(&repository.root).args(["rev-parse", "HEAD"]))
            .await
            .unwrap()
            .trim()
            .to_owned();
        let zero = "0".repeat(40);
        let unrelated = output(git(&repository.root).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit-tree",
            "HEAD^{tree}",
            "-m",
            "unrelated",
        ]))
        .await
        .unwrap()
        .trim()
        .to_owned();
        let url = remote(&fork()).unwrap().url;
        output(git(&repository.root).args([
            "checkout",
            "-b",
            reference.strip_prefix("refs/heads/").unwrap(),
        ]))
        .await
        .unwrap();
        for (destination, local, local_oid, target, remote_oid, expected) in [
            (
                url.as_str(),
                "HEAD",
                oid.as_str(),
                reference.as_str(),
                zero.as_str(),
                true,
            ),
            (
                url.as_str(),
                "HEAD",
                unrelated.as_str(),
                reference.as_str(),
                zero.as_str(),
                false,
            ),
            (
                url.as_str(),
                "HEAD",
                oid.as_str(),
                "refs/heads/main",
                zero.as_str(),
                false,
            ),
            (
                url.as_str(),
                reference.as_str(),
                oid.as_str(),
                reference.as_str(),
                zero.as_str(),
                true,
            ),
            (
                url.as_str(),
                reference.as_str(),
                oid.as_str(),
                reference.as_str(),
                oid.as_str(),
                true,
            ),
            (
                "https://public.example/owner/public.git",
                reference.as_str(),
                oid.as_str(),
                reference.as_str(),
                zero.as_str(),
                false,
            ),
            (
                url.as_str(),
                reference.as_str(),
                zero.as_str(),
                reference.as_str(),
                oid.as_str(),
                false,
            ),
            (
                url.as_str(),
                "refs/heads/main",
                oid.as_str(),
                "refs/heads/main",
                zero.as_str(),
                false,
            ),
            (
                url.as_str(),
                "refs/tags/private-test",
                oid.as_str(),
                "refs/tags/private-test",
                zero.as_str(),
                false,
            ),
            (
                url.as_str(),
                "refs/heads/private-summary",
                oid.as_str(),
                "refs/heads/private-summary",
                zero.as_str(),
                false,
            ),
            (
                url.as_str(),
                reference.as_str(),
                oid.as_str(),
                reference.as_str(),
                "1111111111111111111111111111111111111111",
                false,
            ),
            (
                url.as_str(),
                reference.as_str(),
                oid.as_str(),
                reference.as_str(),
                unrelated.as_str(),
                false,
            ),
        ] {
            let mut command = Command::new(&hook_path);
            clean_environment(&mut command);
            let mut child = command
                .current_dir(&repository.root)
                .args(["origin", destination])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(format!("{local} {local_oid} {target} {remote_oid}\n").as_bytes())
                .await
                .unwrap();
            assert_eq!(
                child.wait().await.unwrap().success(),
                expected,
                "{destination} {target}"
            );
        }
    }
}
