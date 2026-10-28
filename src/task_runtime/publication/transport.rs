//! Narrow, pinned GitHub writes. A successful send is never landing evidence.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use crate::ci::{GitHub, GitHubApi};
use crate::error::{ErrorKind, FlowError, Result};
use crate::model::{self, J};
use crate::util::process::{run_command, CommandOptions, CommandOutput};

pub(super) struct Remote {
    policy: J,
}

pub(super) trait Endpoint: GitHubApi {
    fn push(&self, record: &J) -> Result<()>;
    fn create(&self, record: &J, title: &str, body: &str) -> Result<()>;
    fn merge(&self, record: &J) -> Result<()>;
    fn verify_open(&self, record: &J) -> Result<J>;
    fn branch(&self, record: &J) -> Result<J>;
}

impl Endpoint for Remote {
    fn push(&self, record: &J) -> Result<()> {
        Remote::push(self, record)
    }

    fn create(&self, record: &J, title: &str, body: &str) -> Result<()> {
        Remote::create(self, record, title, body)
    }

    fn merge(&self, record: &J) -> Result<()> {
        Remote::merge(self, record)
    }

    fn verify_open(&self, record: &J) -> Result<J> {
        Remote::verify_open(self, record)
    }

    fn branch(&self, record: &J) -> Result<J> {
        Remote::branch(self, record)
    }
}

impl Remote {
    pub(super) fn new(policy: &J) -> Self {
        Self {
            policy: policy.clone(),
        }
    }

    pub(super) fn push(&self, record: &J) -> Result<()> {
        validate_record(&self.policy, record)
            .and_then(|()| checkout(record))
            .and_then(|checkout| self.push_from(&checkout, record))
    }

    fn push_from(&self, checkout: &Path, record: &J) -> Result<()> {
        verify_checkout_head(checkout, record)
            .and_then(|()| verify_push_destination(checkout, &self.policy))
            .and_then(|()| push_argv(record))
            .and_then(|argv| run_command(&argv, &options(Some(checkout))))
            .and_then(successful)
            .map(drop)
    }

    pub(super) fn create(&self, record: &J, title: &str, body: &str) -> Result<()> {
        validate_record(&self.policy, record)
            .and_then(|()| require(!title.trim().is_empty(), "PR title must be nonempty"))
            .and_then(|()| create_payload(record, title, body))
            .and_then(|payload| self.send("POST", "pulls", &payload))
    }

    pub(super) fn merge(&self, record: &J) -> Result<()> {
        // The host completes the read-only preflight before recording sending.
        // From that durable boundary this function performs only the one PUT.
        validate_record(&self.policy, record).and_then(|()| self.send_merge(record))
    }

    fn send_merge(&self, record: &J) -> Result<()> {
        pr_number(record).and_then(|number| {
            merge_payload(record)
                .and_then(|payload| self.send("PUT", &format!("pulls/{number}/merge"), &payload))
        })
    }

    pub(super) fn verify_open(&self, record: &J) -> Result<J> {
        validate_record(&self.policy, record)
            .and_then(|()| pr_number(record))
            .and_then(|number| self.get(&format!("pulls/{number}")))
            .and_then(|pull| verify_pull(&self.policy, record, &pull).map(|()| pull))
    }

    pub(super) fn branch(&self, record: &J) -> Result<J> {
        validate_record(&self.policy, record)
            .and_then(|()| field(record, "head_ref"))
            .and_then(|name| {
                self.get(&format!("git/ref/heads/{name}"))
                    .and_then(|branch| verify_branch(&branch, name).map(|()| branch))
            })
    }

    fn send(&self, method: &str, suffix: &str, payload: &J) -> Result<()> {
        api_argv(&self.policy, method, suffix)
            .and_then(|argv| {
                payload_options(payload).and_then(|settings| run_command(&argv, &settings))
            })
            .and_then(successful)
            .map(drop)
    }
}

impl GitHubApi for Remote {
    fn get(&self, suffix: &str) -> Result<J> {
        validate_policy(&self.policy)
            .and_then(|()| validate_get_suffix(suffix))
            .and_then(|()| {
                GitHub::new(self.policy.clone())
                    .get(suffix)
                    .map_err(|error| {
                        FlowError::new(
                            ErrorKind::Transient,
                            format!("remote observation unavailable: {error}"),
                        )
                    })
            })
    }
}

/// Validate only path segments: compare uses an embedded `...` separator and
/// query values are data, rather than repository-relative path components.
fn validate_get_suffix(suffix: &str) -> Result<()> {
    require(
        !suffix.contains('#'),
        "GitHub suffix must not contain a fragment",
    )
    .and_then(|()| decode_get_path(suffix))
    .and_then(|path| validate_get_path(&path))
}

fn decode_get_path(suffix: &str) -> Result<String> {
    let path = suffix.split('?').next().unwrap_or_default();
    let mut remaining = path.as_bytes();
    let mut decoded = Vec::with_capacity(remaining.len());
    while let Some((&byte, rest)) = remaining.split_first() {
        let (byte, rest) = decode_path_byte(byte, rest)?;
        decoded.push(byte);
        remaining = rest;
    }
    String::from_utf8(decoded).map_err(|_| denied("GitHub suffix path must be valid UTF-8"))
}

fn decode_path_byte(byte: u8, rest: &[u8]) -> Result<(u8, &[u8])> {
    if byte != b'%' {
        return Ok((byte, rest));
    }
    let (digits, remaining) = rest
        .split_first_chunk::<2>()
        .ok_or_else(|| denied("GitHub suffix has an invalid path escape"))?;
    path_hex(digits[0]).and_then(|high| path_hex(digits[1]).map(|low| (high * 16 + low, remaining)))
}

fn path_hex(byte: u8) -> Result<u8> {
    char::from(byte)
        .to_digit(16)
        .map(|value| value as u8)
        .ok_or_else(|| denied("GitHub suffix has an invalid path escape"))
}

fn validate_get_path(path: &str) -> Result<()> {
    require(
        !path.starts_with('/'),
        "GitHub suffix must be repository relative",
    )
    .and_then(|()| {
        require(
            !path.bytes().any(ambiguous_path_byte),
            "GitHub suffix contains an ambiguous path byte",
        )
    })
    .and_then(|()| {
        path.split('/').try_for_each(|segment| {
            require(
                !matches!(segment, "." | ".."),
                "GitHub suffix must not traverse paths",
            )
        })
    })
}

fn ambiguous_path_byte(byte: u8) -> bool {
    byte.is_ascii_control() || b"\\%?#".contains(&byte)
}

fn denied(message: impl Into<String>) -> FlowError {
    FlowError::new(ErrorKind::PolicyDenied, message)
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(denied(message))
    }
}

fn field<'a>(value: &'a J, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(J::as_str)
        .ok_or_else(|| denied(format!("publication {key} must be a string")))
}

fn validate_policy(policy: &J) -> Result<()> {
    field_is(policy, "host", "github.com")
        .and_then(|()| field(policy, "repo").and_then(validate_repo))
        .and_then(|()| field(policy, "base_ref").and_then(validate_branch))
        .and_then(|()| field_is(policy, "merge_method", "merge"))
}

fn field_is(value: &J, key: &str, expected: &str) -> Result<()> {
    field(value, key).and_then(|actual| {
        require(
            actual == expected,
            "publication identity or policy field differs",
        )
    })
}

fn validate_repo(repo: &str) -> Result<()> {
    let parts: Vec<_> = repo.split('/').collect();
    require(
        parts.len() == 2,
        "publication repository must be owner/repo",
    )
    .and_then(|()| {
        parts
            .iter()
            .try_for_each(|part| require(valid_repo_part(part), "invalid publication repository"))
    })
}

fn valid_repo_part(part: &str) -> bool {
    !matches!(part, "" | "." | "..") && part.bytes().all(repo_byte)
}

fn repo_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._-".contains(&byte)
}

fn validate_record(policy: &J, record: &J) -> Result<()> {
    validate_policy(policy)
        .and_then(|()| {
            ["host", "repo", "base_ref"]
                .iter()
                .try_for_each(|key| matching_field(policy, record, key))
        })
        .and_then(|()| validate_heads(record))
}

fn matching_field(policy: &J, record: &J, key: &str) -> Result<()> {
    field(policy, key).and_then(|expected| field_is(record, key, expected))
}

fn validate_heads(record: &J) -> Result<()> {
    ["head", "target_base"]
        .iter()
        .try_for_each(|key| full_sha_field(record, key))
        .and_then(|()| field(record, "head_ref").and_then(validate_branch))
        .and_then(|()| different_branches(record))
}

fn full_sha_field(record: &J, key: &str) -> Result<()> {
    field(record, key).and_then(|value| {
        require(
            model::is_full_sha(value),
            "publication requires full commit SHAs",
        )
    })
}

fn different_branches(record: &J) -> Result<()> {
    field(record, "head_ref").and_then(|head| {
        field(record, "base_ref").and_then(|base| {
            require(
                head != base,
                "publication head must differ from target branch",
            )
        })
    })
}

fn validate_branch(branch: &str) -> Result<()> {
    require(
        (1..=200).contains(&branch.len()),
        "invalid publication branch length",
    )
    .and_then(|()| {
        require(
            !branch.starts_with('-'),
            "publication branch must not start with a dash",
        )
    })
    .and_then(|()| {
        require(
            branch.bytes().all(branch_byte),
            "invalid publication branch characters",
        )
    })
    .and_then(|()| {
        require(
            !branch.contains(".."),
            "invalid publication branch sequence",
        )
    })
    .and_then(|()| {
        branch.split('/').try_for_each(|part| {
            require(
                valid_branch_part(part),
                "invalid publication branch component",
            )
        })
    })
}

fn branch_byte(byte: u8) -> bool {
    repo_byte(byte) || byte == b'/'
}

fn valid_branch_part(part: &str) -> bool {
    [
        part.is_empty(),
        part.starts_with('.'),
        part.ends_with('.'),
        part.ends_with(".lock"),
    ]
    .into_iter()
    .all(|invalid| !invalid)
}

fn pr_number(record: &J) -> Result<u64> {
    record
        .get("number")
        .and_then(J::as_u64)
        .filter(|number| *number > 0)
        .ok_or_else(|| denied("publication requires a positive PR number"))
}

fn checkout(record: &J) -> Result<PathBuf> {
    field(record, "checkout")
        .map(PathBuf::from)
        .and_then(|path| {
            require(path.is_absolute(), "publication checkout must be absolute").map(|()| path)
        })
}

fn verify_checkout_head(checkout: &Path, record: &J) -> Result<()> {
    git(checkout, &["rev-parse", "--verify", "HEAD^{commit}"])
        .and_then(|output| field_is(record, "head", output.stdout_text().trim()))
}

fn verify_push_destination(checkout: &Path, policy: &J) -> Result<()> {
    configured_push_urls(checkout).and_then(|configured| {
        git(
            checkout,
            &["remote", "get-url", "--push", "--all", "origin"],
        )
        .and_then(|effective| verify_push_urls(policy, &configured, &effective.stdout_text()))
    })
}

fn configured_push_urls(checkout: &Path) -> Result<String> {
    run_command(
        &git_argv(&["config", "--get-all", "remote.origin.pushurl"]),
        &options(Some(checkout)),
    )
    .and_then(|output| configured_urls_output(checkout, output))
}

fn configured_urls_output(checkout: &Path, output: CommandOutput) -> Result<String> {
    if output.status == 1 {
        return git(checkout, &["config", "--get-all", "remote.origin.url"])
            .map(|output| output.stdout_text());
    }
    successful(output).map(|output| output.stdout_text())
}

fn verify_push_urls(policy: &J, configured: &str, effective: &str) -> Result<()> {
    single_url(configured).and_then(|raw| {
        single_url(effective)
            .and_then(|resolved| {
                require(raw == resolved, "Git push URL rewriting is not permitted")
            })
            .and_then(|()| approved_url(policy, raw))
            .and_then(|approved| {
                require(
                    approved,
                    "Git push destination differs from frozen repository",
                )
            })
    })
}

fn single_url(output: &str) -> Result<&str> {
    let urls: Vec<_> = output.lines().collect();
    require(
        urls.len() == 1,
        "publication requires exactly one Git push destination",
    )
    .and_then(|()| require(!urls[0].is_empty(), "Git push destination is empty"))
    .map(|()| urls[0])
}

fn approved_url(policy: &J, url: &str) -> Result<bool> {
    field(policy, "repo").and_then(|repo| {
        field(policy, "host").map(|host| {
            let prefixes = [
                format!("https://{host}/"),
                format!("git@{host}:"),
                format!("ssh://git@{host}/"),
            ];
            prefixes
                .iter()
                .any(|prefix| url_repo(url, prefix) == Some(repo))
        })
    })
}

fn url_repo<'a>(url: &'a str, prefix: &str) -> Option<&'a str> {
    url.strip_prefix(prefix)
        .map(|repo| repo.strip_suffix(".git").unwrap_or(repo))
}

fn git(checkout: &Path, arguments: &[&str]) -> Result<CommandOutput> {
    successful(run_command(&git_argv(arguments), &options(Some(checkout)))?)
}

// Preserve authentication configuration while preventing repository-controlled
// hooks and fsmonitor programs from running inside the publisher.
fn git_argv(arguments: &[&str]) -> Vec<String> {
    [
        "git",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "remote.origin.mirror=false",
    ]
    .into_iter()
    .chain(arguments.iter().copied())
    .map(str::to_string)
    .collect()
}

fn push_argv(record: &J) -> Result<Vec<String>> {
    field(record, "head").and_then(|head| {
        field(record, "head_ref").map(|branch| {
            let lease = format!("--force-with-lease=refs/heads/{branch}:");
            let refspec = format!("{head}:refs/heads/{branch}");
            git_argv(&[
                "push",
                "--porcelain",
                "--no-follow-tags",
                "--recurse-submodules=no",
                &lease,
                "origin",
                &refspec,
            ])
        })
    })
}

fn options(checkout: Option<&Path>) -> CommandOptions {
    CommandOptions {
        cwd: checkout.map(Path::to_path_buf),
        timeout: Duration::from_secs(45),
        remove_git_env: true,
        kill_on_parent_death: true,
        stdout_limit: 1024 * 1024,
        stderr_limit: 64 * 1024,
        env: [("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    }
}

fn successful(output: CommandOutput) -> Result<CommandOutput> {
    if output.status == 0 {
        return Ok(output);
    }
    Err(FlowError::new(
        ErrorKind::Transport,
        format!(
            "publication command failed (exit {}); reconcile remote state before retrying",
            output.status
        ),
    ))
}

fn api_argv(policy: &J, method: &str, suffix: &str) -> Result<Vec<String>> {
    field(policy, "host").and_then(|host| {
        field(policy, "repo").map(|repo| {
            vec![
                "gh".into(),
                "api".into(),
                "--hostname".into(),
                host.into(),
                "--method".into(),
                method.into(),
                format!("repos/{repo}/{suffix}"),
                "--input".into(),
                "-".into(),
            ]
        })
    })
}

fn create_payload(record: &J, title: &str, body: &str) -> Result<J> {
    field(record, "head_ref").and_then(|head| field(record, "base_ref").map(|base|
        json!({"title":title, "body":body, "head":head, "base":base, "draft":false, "maintainer_can_modify":false})))
}

fn merge_payload(record: &J) -> Result<J> {
    Ok(json!({"sha":field(record, "head")?, "merge_method":"merge"}))
}

fn verify_pull(policy: &J, record: &J, pull: &J) -> Result<()> {
    pr_number(record)
        .and_then(|number| {
            require(
                pull.get("number").and_then(J::as_u64) == Some(number),
                "GitHub PR number differs",
            )
        })
        .and_then(|()| field_is(pull, "state", "open"))
        .and_then(|()| {
            require(
                pull.get("draft") == Some(&json!(false)),
                "GitHub PR is draft or draft status is unknown",
            )
        })
        .and_then(|()| {
            require(
                pull.get("merged") == Some(&json!(false)),
                "GitHub PR is merged or merge status is unknown",
            )
        })
        .and_then(|()| verify_pull_side(policy, record, pull, "head", "head_ref", "head"))
        .and_then(|()| verify_pull_side(policy, record, pull, "base", "base_ref", "target_base"))
}

fn verify_pull_side(
    policy: &J,
    record: &J,
    pull: &J,
    side: &str,
    branch: &str,
    sha: &str,
) -> Result<()> {
    pull.get(side)
        .ok_or_else(|| denied("GitHub PR side is missing"))
        .and_then(|actual| {
            field(record, branch)
                .and_then(|expected| field_is(actual, "ref", expected))
                .and_then(|()| field(record, sha))
                .and_then(|expected| field_is(actual, "sha", expected))
                .and_then(|()| verify_pull_repo(policy, actual))
        })
}

fn verify_pull_repo(policy: &J, side: &J) -> Result<()> {
    side.get("repo")
        .ok_or_else(|| denied("GitHub PR repository is missing"))
        .and_then(|repo| {
            field(policy, "repo")
                .and_then(|expected| field_is(repo, "full_name", expected))
                .and_then(|()| repository_html_url(policy))
                .and_then(|url| field_is(repo, "html_url", &url))
        })
}

fn repository_html_url(policy: &J) -> Result<String> {
    field(policy, "host")
        .and_then(|host| field(policy, "repo").map(|repo| format!("https://{host}/{repo}")))
}

fn verify_branch(branch: &J, name: &str) -> Result<()> {
    field_is(branch, "ref", &format!("refs/heads/{name}"))
        .and_then(|()| {
            branch
                .get("object")
                .ok_or_else(|| denied("GitHub branch object is missing"))
        })
        .and_then(verify_branch_object)
}

fn verify_branch_object(object: &J) -> Result<()> {
    field_is(object, "type", "commit").and_then(|()| full_sha_field(object, "sha"))
}

fn payload_options(payload: &J) -> Result<CommandOptions> {
    serde_json::to_vec(payload)
        .map(|stdin| CommandOptions {
            stdin: Some(stdin),
            ..options(None)
        })
        .map_err(FlowError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn policy() -> J {
        json!({"host":"github.com", "repo":"owner/repo", "base_ref":"main", "merge_method":"merge"})
    }

    fn record() -> J {
        json!({"host":"github.com", "repo":"owner/repo", "head":"a".repeat(40), "target_base":"b".repeat(40), "head_ref":"publish/task-1", "base_ref":"main", "number":17, "checkout":"/owned/checkout"})
    }

    fn pull() -> J {
        let repository =
            json!({"full_name":"owner/repo", "html_url":"https://github.com/owner/repo"});
        json!({"number":17, "state":"open", "draft":false, "merged":false, "mergeable":true,
            "head":{"sha":"a".repeat(40), "ref":"publish/task-1", "repo":repository},
            "base":{"sha":"b".repeat(40), "ref":"main", "repo":repository}})
    }

    fn local_repo() -> tempfile::TempDir {
        let directory = crate::util::fs::test_tempdir();
        git(directory.path(), &["init", "--quiet"]).unwrap();
        directory
    }

    #[test]
    fn records_are_bound_to_policy_full_shas_and_safe_refs() {
        validate_record(&policy(), &record()).unwrap();
        for (key, value) in [
            ("host", "elsewhere"),
            ("repo", "other/repo"),
            ("base_ref", "other"),
            ("head", "abc"),
            ("target_base", "bad"),
            ("head_ref", "main"),
        ] {
            let mut changed = record();
            changed[key] = json!(value);
            assert!(validate_record(&policy(), &changed).is_err(), "{key}");
        }
        for name in [
            "", "-branch", "foo..bar", ".hidden", "foo.lock", "foo/", "foo//bar", "foo?bad",
            "foo@{1}", "foo.",
        ] {
            assert!(validate_branch(name).is_err(), "{name}");
        }
        for repository in [
            "",
            "one",
            "owner/repo/extra",
            "../repo",
            "owner/.",
            "owner/evil?repo",
        ] {
            assert!(validate_repo(repository).is_err(), "{repository}");
        }
        let mut unsupported = policy();
        unsupported["merge_method"] = json!("squash");
        assert!(validate_policy(&unsupported).is_err());
        assert!(pr_number(&json!({"number":0})).is_err());
        assert!(checkout(&json!({"checkout":"relative"})).is_err());
    }

    #[test]
    fn api_commands_use_pinned_repository_and_json_stdin() {
        let title = "quotes \" and $(touch forbidden)";
        let body = "line one\nline two `command`";
        let payload = create_payload(&record(), title, body).unwrap();
        let settings = payload_options(&payload).unwrap();
        let decoded: J = serde_json::from_slice(settings.stdin.as_ref().unwrap()).unwrap();
        assert_eq!(decoded["title"], title);
        assert_eq!(decoded["body"], body);
        assert_eq!(decoded["head"], "publish/task-1");
        assert_eq!(decoded["base"], "main");
        assert_eq!(
            merge_payload(&record()).unwrap(),
            json!({"sha":"a".repeat(40), "merge_method":"merge"})
        );
        let argv = api_argv(&policy(), "PUT", "pulls/17/merge").unwrap();
        assert_eq!(
            argv,
            [
                "gh",
                "api",
                "--hostname",
                "github.com",
                "--method",
                "PUT",
                "repos/owner/repo/pulls/17/merge",
                "--input",
                "-"
            ]
        );
        assert!(settings.kill_on_parent_death);
        assert!(settings.remove_git_env);
        assert!(!settings.clear_env);
        assert_eq!(settings.stdout_limit, 1024 * 1024);
        assert_eq!(settings.stderr_limit, 64 * 1024);
        assert!(!settings.env.contains_key("TMPDIR"));
        assert!(!settings
            .env
            .keys()
            .any(|key| key.to_ascii_lowercase().contains("proxy")));
    }

    #[test]
    fn live_pr_identity_state_and_both_exact_commits_are_required() {
        verify_pull(&policy(), &record(), &pull()).unwrap();
        for (path, value) in [
            (vec!["number"], json!(18)),
            (vec!["state"], json!("closed")),
            (vec!["draft"], json!(true)),
            (vec!["merged"], json!(true)),
            (vec!["head", "sha"], json!("c".repeat(40))),
            (vec!["base", "sha"], json!("c".repeat(40))),
            (vec!["head", "ref"], json!("other")),
            (vec!["base", "ref"], json!("other")),
            (vec!["head", "repo", "full_name"], json!("fork/repo")),
            (
                vec!["base", "repo", "html_url"],
                json!("https://other/owner/repo"),
            ),
            (vec!["head", "repo"], J::Null),
        ] {
            let mut changed = pull();
            *model::dig_mut(&mut changed, &path).unwrap() = value;
            assert!(
                verify_pull(&policy(), &record(), &changed).is_err(),
                "{path:?}"
            );
        }
        assert!(verify_pull(&policy(), &record(), &json!({})).is_err());
    }

    #[test]
    fn exact_ref_query_response_cannot_be_a_prefix_match_or_tag() {
        let mut branch = json!({"ref":"refs/heads/publish/task-1", "object":{"type":"commit", "sha":"a".repeat(40)}});
        verify_branch(&branch, "publish/task-1").unwrap();
        assert!(verify_branch(&branch, "publish/task").is_err());
        branch["object"]["type"] = json!("tag");
        assert!(verify_branch(&branch, "publish/task-1").is_err());
        branch["object"]["type"] = json!("commit");
        branch["object"]["sha"] = json!("abc");
        assert!(verify_branch(&branch, "publish/task-1").is_err());
    }

    #[test]
    fn only_one_unchanged_approved_push_url_is_allowed() {
        for url in [
            "https://github.com/owner/repo",
            "https://github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo",
        ] {
            verify_push_urls(&policy(), url, url).unwrap();
        }
        for url in [
            "",
            "https://github.com/owner/repo/",
            "https://github.com/other/repo",
            "https://github.com.evil/owner/repo",
            "https://user@github.com/owner/repo",
            "file:///repo",
            "https://github.com/owner/repo\nhttps://github.com/owner/repo",
        ] {
            assert!(verify_push_urls(&policy(), url, url).is_err(), "{url}");
        }
        assert!(
            verify_push_urls(&policy(), "approved-alias", "https://github.com/owner/repo").is_err()
        );
    }

    #[test]
    fn actual_git_pushurl_and_rewrite_configuration_are_checked() {
        let directory = local_repo();
        let root = directory.path();
        git(
            root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        )
        .unwrap();
        verify_push_destination(root, &policy()).unwrap();
        git(
            root,
            &[
                "config",
                "remote.origin.pushurl",
                "https://github.com/other/repo.git",
            ],
        )
        .unwrap();
        assert!(verify_push_destination(root, &policy()).is_err());
        git(root, &["config", "--unset-all", "remote.origin.pushurl"]).unwrap();
        git(
            root,
            &[
                "config",
                "url.https://elsewhere/.pushInsteadOf",
                "https://github.com/",
            ],
        )
        .unwrap();
        assert!(verify_push_destination(root, &policy()).is_err());
        git(
            root,
            &[
                "config",
                "--unset-all",
                "url.https://elsewhere/.pushInsteadOf",
            ],
        )
        .unwrap();
        git(
            root,
            &[
                "config",
                "url.https://elsewhere/.insteadOf",
                "https://github.com/",
            ],
        )
        .unwrap();
        assert!(verify_push_destination(root, &policy()).is_err());
        git(
            root,
            &["config", "--unset-all", "url.https://elsewhere/.insteadOf"],
        )
        .unwrap();
        git(
            root,
            &[
                "config",
                "--add",
                "remote.origin.url",
                "https://github.com/other/repo.git",
            ],
        )
        .unwrap();
        assert!(verify_push_destination(root, &policy()).is_err());
    }

    #[test]
    fn real_git_empty_lease_creates_only_fresh_ref_and_blocks_repository_hooks() {
        let directory = local_repo();
        let root = directory.path();
        git(root, &["config", "user.name", "Publication test"]).unwrap();
        git(
            root,
            &["config", "user.email", "publication@example.invalid"],
        )
        .unwrap();
        git(
            root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        let remote = root.join("remote.git");
        git(
            root,
            &["init", "--bare", "--quiet", remote.to_str().unwrap()],
        )
        .unwrap();
        git(root, &["remote", "add", "origin", remote.to_str().unwrap()]).unwrap();
        let hooks = root.join("owned-hooks");
        fs::create_dir(&hooks).unwrap();
        let marker = root.join("hook-ran");
        let hook = hooks.join("pre-push");
        fs::write(&hook, "#!/bin/sh\ntouch \"$PWD/hook-ran\"\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        git(root, &["config", "core.hooksPath", hooks.to_str().unwrap()]).unwrap();
        git(root, &["config", "remote.origin.mirror", "true"]).unwrap();
        git(root, &["config", "push.followTags", "true"]).unwrap();
        git(
            root,
            &[
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "-m",
                "extra",
                "extra-tag",
            ],
        )
        .unwrap();
        let sha = git(root, &["rev-parse", "HEAD"])
            .unwrap()
            .stdout_text()
            .trim()
            .to_string();
        let mut candidate = record();
        candidate["head"] = json!(sha);
        verify_checkout_head(root, &candidate).unwrap();
        let argv = push_argv(&candidate).unwrap();
        assert!(argv.contains(&"--force-with-lease=refs/heads/publish/task-1:".to_string()));
        successful(run_command(&argv, &options(Some(root))).unwrap()).unwrap();
        assert!(
            !marker.exists(),
            "publication push executed repository pre-push hook"
        );
        assert_eq!(
            git(&remote, &["for-each-ref", "--format=%(refname)"])
                .unwrap()
                .stdout_text(),
            "refs/heads/publish/task-1\n"
        );
        git(
            root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "next",
            ],
        )
        .unwrap();
        assert!(verify_checkout_head(root, &candidate).is_err());
        candidate["head"] = json!(git(root, &["rev-parse", "HEAD"])
            .unwrap()
            .stdout_text()
            .trim());
        let failed = run_command(&push_argv(&candidate).unwrap(), &options(Some(root))).unwrap();
        assert!(successful(failed).is_err());
        let observed = git(&remote, &["rev-parse", "refs/heads/publish/task-1"]).unwrap();
        assert_eq!(observed.stdout_text().trim(), sha);
    }

    #[test]
    fn real_git_status_blocks_the_configured_repository_fsmonitor() {
        let directory = local_repo();
        let root = directory.path();
        fs::write(root.join("tracked"), "indexed content\n").unwrap();
        git(root, &["add", "tracked"]).unwrap();
        let monitor = root.join("repository-fsmonitor");
        fs::write(
            &monitor,
            "#!/bin/sh\ntouch \"$PWD/fsmonitor-ran\"\nprintf 'fixture-token\\0/\\0'\n",
        )
        .unwrap();
        fs::set_permissions(&monitor, fs::Permissions::from_mode(0o700)).unwrap();
        git(
            root,
            &["config", "core.fsmonitor", monitor.to_str().unwrap()],
        )
        .unwrap();
        git(root, &["config", "core.fsmonitorHookVersion", "2"]).unwrap();
        git(root, &["status", "--porcelain"]).unwrap();
        assert!(
            !root.join("fsmonitor-ran").exists(),
            "publication Git command executed repository fsmonitor"
        );
    }

    #[test]
    fn git_keeps_system_global_authentication_and_proxy_configuration() {
        let directory = local_repo();
        let root = directory.path();
        let helper = root.join("fake-credential-helper");
        fs::write(&helper, "#!/bin/sh\n[ \"$1\" = get ] || exit 1\ntouch \"$PWD/fake-credential-ran\"\nprintf 'username=fixture-user\\npassword=fixture-token\\n'\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let system = root.join("fake-system.gitconfig");
        let global = root.join("fake-global.gitconfig");
        fs::write(&system, "[fixture]\n fromSystem = present\n").unwrap();
        fs::write(&global, format!("[fixture]\n fromGlobal = present\n[credential]\n helper =\n helper = {}\n[http \"https://publication-auth-fixture.invalid\"]\n proxy = http://fixture-proxy.invalid:4123\n", helper.display())).unwrap();
        let mut settings = options(Some(root));
        assert!(!settings.clear_env);
        for key in [
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_COUNT",
        ] {
            assert!(
                !settings.env.contains_key(key),
                "publication overrides authentication configuration: {key}"
            );
        }
        // Substitute only this child's two Git configuration files. The real
        // global/system files, credentials, HOME and proxy environment are untouched.
        settings.env.insert(
            "GIT_CONFIG_SYSTEM".into(),
            system.to_string_lossy().into_owned(),
        );
        settings.env.insert(
            "GIT_CONFIG_GLOBAL".into(),
            global.to_string_lossy().into_owned(),
        );
        for key in ["fixture.fromSystem", "fixture.fromGlobal"] {
            let output =
                successful(run_command(&git_argv(&["config", "--get", key]), &settings).unwrap())
                    .unwrap();
            assert_eq!(output.stdout_text().trim(), "present");
        }
        let proxy = successful(
            run_command(
                &git_argv(&[
                    "config",
                    "--get-urlmatch",
                    "http.proxy",
                    "https://publication-auth-fixture.invalid/repo",
                ]),
                &settings,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            proxy.stdout_text().trim(),
            "http://fixture-proxy.invalid:4123"
        );
        settings.stdin =
            Some(b"protocol=https\nhost=publication-auth-fixture.invalid\n\n".to_vec());
        let credential =
            successful(run_command(&git_argv(&["credential", "fill"]), &settings).unwrap())
                .unwrap();
        assert!(root.join("fake-credential-ran").is_file());
        assert!(credential.stdout_text().contains("username=fixture-user"));
        assert!(credential.stdout_text().contains("password=fixture-token"));
    }

    #[test]
    fn real_get_path_validator_accepts_compare_and_other_nontraversal_dots() {
        let base = "a".repeat(40);
        let head = "b".repeat(40);
        for suffix in [
            format!("compare/{base}...{head}"),
            format!("compare/{base}%2E%2e%2E{head}"),
            "compare/main...feature/topic".to_string(),
            "actions/workflows/ci..yml/runs?event=pull_request".to_string(),
            "branches/release%2F1.0".to_string(),
            "pulls?head=owner%3Afeature%2F..%2Ftopic&state=all".to_string(),
        ] {
            validate_get_suffix(&suffix).unwrap_or_else(|error| panic!("{suffix}: {error}"));
        }
        assert_eq!(
            decode_get_path(&format!("compare/{base}...{head}")).unwrap(),
            format!("compare/{base}...{head}")
        );
    }

    #[test]
    fn real_get_adapter_refuses_raw_encoded_and_ambiguous_traversal_before_network() {
        let remote = Remote::new(&policy());
        for suffix in [
            "../pulls",
            "pulls/../issues",
            "./pulls",
            "pulls/./1",
            "/pulls",
            "%2fpulls",
            "git/ref/heads/feature%2F..%2Fmain",
            "pulls/%2e%2e/issues",
            "pulls/.%2e/issues",
            "pulls/%2e./issues",
            "pulls/%2E%2E%2Fissues",
            "pulls%2F..%2Fissues",
            "pulls/%252e%252e/issues",
            "pulls/%252f../issues",
            "pulls\\..\\issues",
            "pulls/%5c..%5cissues",
            "pulls/%",
            "pulls/%2",
            "pulls/%zz",
            "pulls/%ff",
            "pulls/%00",
            "pulls#../issues",
            "pulls/%3f../issues",
        ] {
            let error = remote.get(suffix).unwrap_err();
            assert_eq!(error.kind, ErrorKind::PolicyDenied, "{suffix}: {error}");
        }
    }

    #[test]
    fn invalid_inputs_fail_before_any_remote_write() {
        let remote = Remote::new(&policy());
        assert!(remote.push(&json!({})).is_err());
        assert!(remote.create(&record(), " ", "body").is_err());
        assert!(remote.merge(&json!({})).is_err());
        assert!(remote.branch(&json!({})).is_err());
        assert!(remote.get("/absolute").is_err());
        assert!(remote.get("../elsewhere").is_err());
    }
}
