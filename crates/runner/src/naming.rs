pub fn sanitize_workspace_name(value: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            result.push(character);
            separator = false;
        } else if !separator && !result.is_empty() {
            result.push('-');
            separator = true;
        }
        if result.len() >= 64 {
            break;
        }
    }
    while result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() {
        "workspace".into()
    } else {
        result
    }
}

pub fn sanitize_branch(value: &str) -> String {
    let components = value
        .split('/')
        .filter_map(|component| {
            let mut clean = String::new();
            let mut separator = false;
            for character in component.chars().flat_map(char::to_lowercase) {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    clean.push(character);
                    separator = false;
                } else if character == '.' {
                    if !clean.is_empty() && !clean.ends_with('.') {
                        clean.push('.');
                    }
                    separator = false;
                } else if !separator && !clean.is_empty() {
                    clean.push('-');
                    separator = true;
                }
            }
            let mut clean = clean
                .trim_matches(|character: char| matches!(character, '.' | '-'))
                .to_string();
            if clean.ends_with(".lock") {
                clean.truncate(clean.len() - 5);
                while clean.ends_with(['.', '-']) {
                    clean.pop();
                }
                clean.push_str("-lock");
            }
            (!clean.is_empty()).then_some(clean)
        })
        .collect::<Vec<_>>();
    let result = components.join("/");
    if result.is_empty() || result == "@" {
        "agent/workspace".into()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_workspace_names() {
        assert_eq!(
            sanitize_workspace_name(" Fix: Login / SSO! "),
            "fix-login-sso"
        );
        assert_eq!(sanitize_workspace_name("***"), "workspace");
    }

    #[test]
    fn sanitizes_git_branches() {
        assert_eq!(
            sanitize_branch(" Fix Login/@{bad}..lock "),
            "fix-login/bad-lock"
        );
        assert_eq!(sanitize_branch("///"), "agent/workspace");
        assert!(!sanitize_branch("topic.lock").ends_with(".lock"));
    }

    #[test]
    fn produces_git_check_ref_format_compatible_branches() {
        let cases = [
            "../escape",
            "topic/.../child",
            ".hidden/component",
            "good/foo.lock/bar.lock/baz",
            "name@{revision}",
            " leading / repeated//slashes ",
            "back\\slash/~caret^colon:question?star*bracket[",
            "@",
            "-leading-dash",
            "trailing-dot.",
        ];
        for input in cases {
            let branch = sanitize_branch(input);
            assert!(!branch.contains(".."), "{input:?} became {branch:?}");
            assert!(!branch.split('/').any(|part| part.starts_with('.')));
            assert!(!branch.split('/').any(|part| part.ends_with(".lock")));
            assert!(!branch.contains("@{"));
            assert!(!branch.starts_with('-'));
            assert!(!branch.ends_with('.'));
            assert_ne!(branch, "@");
            assert!(
                std::process::Command::new("git")
                    .args(["check-ref-format", "--branch", &branch])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .expect("git should be available for ref validation")
                    .success(),
                "{input:?} became invalid branch {branch:?}"
            );
        }
    }
}
