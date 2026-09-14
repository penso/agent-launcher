use std::{fs, io::Write, path::Path};

use agent_launcher_core::PromptProfile;
use rustix::fs::{AtFlags, Mode, OFlags, linkat, mkdirat, open, openat, renameat, unlinkat};

use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptDocument {
    pub name: String,
    pub source: String,
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.len() > 128
        || matches!(name, "." | "..")
        || name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return Err(Error::InvalidPromptName);
    }
    Ok(())
}

pub(crate) fn validate_source(name: &str, source: &str) -> Result<()> {
    if source.trim().is_empty() {
        return Err(Error::EmptyPromptProfile(name.into()));
    }
    minijinja::Environment::new()
        .add_template("prompt", source)
        .map_err(|source| Error::RenderPromptProfile {
            profile: name.into(),
            source,
        })
}

/// Shared startup/save inventory. Symlink profiles remain readable.
pub fn discover_prompt_profiles(root: &Path) -> Result<Vec<PromptProfile>> {
    discover(root, None)
}

fn discover(root: &Path, replacement: Option<&str>) -> Result<Vec<PromptProfile>> {
    let mut profiles = Vec::new();
    for entry in fs::read_dir(root).map_err(Error::PromptIo)? {
        let entry = entry.map_err(Error::PromptIo)?;
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path().join("prompt.md");
        if replacement == Some(name.as_str()) {
            profiles.push(PromptProfile { name, path });
            continue;
        }
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::ReadPromptProfile {
                    profile: name,
                    path,
                    source,
                });
            },
        };
        if source.trim().is_empty() {
            return Err(Error::EmptyPromptProfile(name));
        }
        profiles.push(PromptProfile { name, path });
    }
    profiles.sort_by(|a, b| {
        (a.name != "implementer")
            .cmp(&(b.name != "implementer"))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(profiles)
}

fn path_error(error: rustix::io::Errno) -> Error {
    if matches!(error, rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) {
        Error::UnsafePromptPath
    } else {
        Error::PromptIo(error.into())
    }
}

struct PromptLock(fs::File);

impl Drop for PromptLock {
    fn drop(&mut self) {
        // Closing alone can retain the lock in a forked child until exec, even
        // with CLOEXEC. Explicit unlock releases it through all shared handles.
        let _ = self.0.unlock();
    }
}

/// Launcher writers cooperate through an advisory lock. External editors do not;
/// comparison and replacement cannot be atomic with their nonparticipating writes.
pub(crate) fn save(
    root: &Path,
    name: String,
    source: String,
    expected: Option<String>,
) -> Result<(PromptDocument, Vec<PromptProfile>)> {
    validate_name(&name)?;
    validate_source(&name, &source)?;
    // Resolve ancestors (e.g. macOS /var), but never follow the configured root itself.
    let parent = root
        .parent()
        .ok_or(Error::UnsafePromptPath)?
        .canonicalize()
        .map_err(Error::PromptIo)?;
    let leaf = root.file_name().ok_or(Error::UnsafePromptPath)?;
    let parent = open(
        &parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(path_error)?;
    match mkdirat(&parent, leaf, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {},
        Err(error) => return Err(path_error(error)),
    }
    let root_path = root;
    let root = openat(
        &parent,
        leaf,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(path_error)?;
    let lock: fs::File = openat(
        &root,
        ".prompt-editor.lock",
        OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(path_error)?
    .into();
    if !lock.metadata().map_err(Error::PromptIo)?.is_file() {
        return Err(Error::UnsafePromptPath);
    }
    lock.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => Error::PromptBusy,
        fs::TryLockError::Error(error) => Error::PromptIo(error),
    })?;
    let _lock = PromptLock(lock);
    match mkdirat(&root, name.as_str(), Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {},
        Err(error) => return Err(path_error(error)),
    }
    let directory = openat(
        &root,
        name.as_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(path_error)?;
    let (current, permissions) = match openat(
        &directory,
        "prompt.md",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let mut file: fs::File = fd.into();
            let metadata = file.metadata().map_err(Error::PromptIo)?;
            if !metadata.is_file() {
                return Err(Error::UnsafePromptPath);
            }
            let permissions = metadata.permissions();
            if permissions.readonly() {
                return Err(Error::ReadOnlyPromptProfile(name));
            }
            let mut text = String::new();
            std::io::Read::read_to_string(&mut file, &mut text).map_err(Error::PromptIo)?;
            (Some(text), Some(permissions))
        },
        Err(rustix::io::Errno::NOENT) => (None, None),
        Err(error) => return Err(path_error(error)),
    };
    if current != expected {
        return Err(Error::PromptConflict);
    }
    // Prepare the inventory under the writer lock before committing, so a bad
    // sibling profile cannot turn a successful disk write into a failed save.
    let profiles = discover(root_path, Some(&name))?;
    let temporary = format!(".prompt-{}", uuid::Uuid::new_v4());
    let mut file: fs::File = openat(
        &directory,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(path_error)?
    .into();
    let result = (|| {
        file.write_all(source.as_bytes()).map_err(Error::PromptIo)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions).map_err(Error::PromptIo)?;
        }
        file.sync_all().map_err(Error::PromptIo)?;
        if expected.is_none() {
            linkat(
                &directory,
                temporary.as_str(),
                &directory,
                "prompt.md",
                AtFlags::empty(),
            )
            .map_err(|e| {
                if e == rustix::io::Errno::EXIST {
                    Error::PromptConflict
                } else {
                    path_error(e)
                }
            })?;
        } else {
            renameat(&directory, temporary.as_str(), &directory, "prompt.md")
                .map_err(path_error)?;
        }
        Ok((PromptDocument { name, source }, profiles))
    })();
    let _ = unlinkat(&directory, temporary.as_str(), AtFlags::empty());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_guard_unlocks_with_duplicate_alive_on_success_and_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lock");
        let lock = fs::File::create(&path).unwrap();
        lock.try_lock().unwrap();
        let duplicate = lock.try_clone().unwrap();
        let contender = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        drop(lock);
        // A duplicate models a child's inherited descriptor without racing fork/exec.
        assert!(matches!(
            contender.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        duplicate.unlock().unwrap();
        contender.try_lock().unwrap();
        contender.unlock().unwrap();
        drop(duplicate);

        for fail in [false, true] {
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            lock.try_lock().unwrap();
            let duplicate = lock.try_clone().unwrap();
            let result: Result<()> = (|| {
                let _lock = PromptLock(lock);
                if fail {
                    return Err(Error::PromptConflict);
                }
                Ok(())
            })();
            assert_eq!(result.is_err(), fail);
            contender.try_lock().unwrap();
            contender.unlock().unwrap();
            drop(duplicate);
        }
    }

    #[test]
    fn creates_compares_exact_source_and_preserves_invalid_edits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("agents");
        let original = "  {{ issue_title }}\n";
        save(&root, "zebra".into(), original.into(), None).unwrap();
        for expected in [None, Some(original.trim().into()), Some("stale".into())] {
            assert!(matches!(
                save(&root, "zebra".into(), "new".into(), expected),
                Err(Error::PromptConflict)
            ));
        }
        for source in [" ", "{% if %}"] {
            assert!(save(&root, "zebra".into(), source.into(), Some(original.into())).is_err());
        }
        assert_eq!(
            fs::read_to_string(root.join("zebra/prompt.md")).unwrap(),
            original
        );
        save(
            &root,
            "zebra".into(),
            "edited".into(),
            Some(original.into()),
        )
        .unwrap();
        for name in ["alpha", "implementer"] {
            save(&root, name.into(), "valid".into(), None).unwrap();
        }
        assert_eq!(
            discover_prompt_profiles(&root)
                .unwrap()
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["implementer", "alpha", "zebra"]
        );
        for name in [
            "",
            " ",
            ".",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            "a\n",
            &"a".repeat(129),
        ] {
            assert!(matches!(
                save(&root, name.into(), "valid".into(), None),
                Err(Error::InvalidPromptName)
            ));
        }
        fs::create_dir(root.join("broken")).unwrap();
        fs::write(root.join("broken/prompt.md"), " ").unwrap();
        assert!(
            save(
                &root,
                "zebra".into(),
                "should not save".into(),
                Some("edited".into())
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(root.join("zebra/prompt.md")).unwrap(),
            "edited"
        );
        // A broken target itself can still be repaired.
        save(&root, "broken".into(), "repaired".into(), Some(" ".into())).unwrap();
    }

    #[test]
    fn edits_reject_readonly_targets_and_preserve_writable_permissions() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("agents");
        save(&root, "profile".into(), "original".into(), None).unwrap();
        let path = root.join("profile/prompt.md");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        let original = fs::metadata(&path).unwrap();
        assert!(
            matches!(save(&root, "profile".into(), "changed".into(), Some("original".into())), Err(Error::ReadOnlyPromptProfile(name)) if name == "profile")
        );
        let unchanged = fs::metadata(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        assert_eq!(unchanged.permissions().mode() & 0o7777, 0o444);
        assert_eq!(unchanged.ino(), original.ino());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        save(
            &root,
            "profile".into(),
            "changed".into(),
            Some("original".into()),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "changed");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o640
        );
    }

    #[test]
    fn symlinks_are_readable_but_never_writable_and_lock_is_bounded() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("agents");
        save(&root, "original".into(), "original".into(), None).unwrap();
        let alias = temp.path().join("alias");
        symlink(&root, &alias).unwrap();
        assert!(matches!(
            save(
                &alias,
                "original".into(),
                "bad".into(),
                Some("original".into())
            ),
            Err(Error::UnsafePromptPath)
        ));
        symlink(root.join("original"), root.join("linked-dir")).unwrap();
        fs::create_dir(root.join("linked-file")).unwrap();
        symlink(
            root.join("original/prompt.md"),
            root.join("linked-file/prompt.md"),
        )
        .unwrap();
        assert_eq!(discover_prompt_profiles(&root).unwrap().len(), 3);
        for name in ["linked-dir", "linked-file"] {
            assert!(matches!(
                save(&root, name.into(), "bad".into(), Some("original".into())),
                Err(Error::UnsafePromptPath)
            ));
        }
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join(".prompt-editor.lock"))
            .unwrap();
        lock.lock().unwrap();
        assert!(matches!(
            save(
                &root,
                "original".into(),
                "bad".into(),
                Some("original".into())
            ),
            Err(Error::PromptBusy)
        ));
        assert_eq!(
            fs::read_to_string(root.join("original/prompt.md")).unwrap(),
            "original"
        );
    }

    #[test]
    fn concurrent_creates_have_one_winner() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("agents");
        let results = std::thread::scope(|s| {
            let a = s.spawn(|| save(&root, "new".into(), "first".into(), None));
            let b = s.spawn(|| save(&root, "new".into(), "second".into(), None));
            [a.join().unwrap(), b.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        let (winner, _) = results.into_iter().find_map(Result::ok).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("new/prompt.md")).unwrap(),
            winner.source
        );
    }
}
