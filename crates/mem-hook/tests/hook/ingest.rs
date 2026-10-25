//! `mem-hook ingest`: which files are sent, what is sent again, pruning, dry runs, failures.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::env::TestEnv;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn sha(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

fn git_init(root: &Path) {
    let status = Command::new("git").arg("-C").arg(root).args(["init", "-q"]).stdout(Stdio::null()).status().unwrap();
    assert!(status.success());
}

fn uris(posted: &[Value]) -> Vec<&str> {
    let mut uris: Vec<&str> = posted.iter().map(|b| b["uri"].as_str().unwrap()).collect();
    uris.sort();
    uris
}

fn listed(uri: &str, content: &str) -> Value {
    json!({"uri": uri, "content_hash": sha(content), "title": uri, "chunks": 1})
}

#[test]
fn sends_docs_and_skips_binaries_big_files_and_vendored_directories() {
    let (fake, env) = TestEnv::with_fake();
    let root = &env.cwd;
    write(root, "README.md", "# Homelab\n\nHow it works.");
    write(root, "docs/adr/0001-postgres.md", "# ADR 1\n\nUse Postgres.");
    write(root, "notes.txt", "plain notes");
    write(root, "guide.rst", "Guide\n=====\n");
    write(root, "src/main.rs", "fn main() {}");
    write(root, "node_modules/pkg/README.md", "vendored");
    write(root, "target/doc/index.md", "generated");
    write(root, ".git/description.md", "git internals");
    write(root, "empty.md", "  \n");
    write(root, "big.md", &"x".repeat(401 * 1024));
    std::fs::write(root.join("binary.md"), b"# not text\0\x01\x02").unwrap();

    let out = env.ingest(&["."]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let posted = fake.posted_sources();
    assert_eq!(uris(&posted), ["README.md", "docs/adr/0001-postgres.md", "guide.rst", "notes.txt"]);
    let readme = posted.iter().find(|b| b["uri"] == "README.md").unwrap();
    assert_eq!(readme["content_type"], "markdown");
    assert_eq!(readme["content"], "# Homelab\n\nHow it works.");
    assert_eq!(readme["content_hash"], sha("# Homelab\n\nHow it works."));
    assert_eq!(readme["scope"], json!({"kind": "project", "key": "local/proj"}));
    assert!(
        posted
            .iter()
            .filter(|b| b["uri"] != "README.md" && b["uri"] != "docs/adr/0001-postgres.md")
            .all(|b| b["content_type"] == "text")
    );
    for skipped in ["skip big.md (too large)", "skip binary.md (binary)", "skip empty.md (empty)"] {
        assert!(out.stdout.contains(skipped), "{skipped}: {}", out.stdout);
    }
    assert!(out.stdout.contains("send 4 new and 0 changed"), "{}", out.stdout);
    let list = fake.matching("GET", "/v1/sources");
    assert_eq!(list[0].path, "/v1/sources?scope=project:local/proj");
    assert_eq!(list[0].header("authorization"), Some("Bearer mem_abcd1234_secret"));
}

#[test]
fn a_second_run_sends_only_the_files_that_changed() {
    let (fake, env) = TestEnv::with_fake();
    write(&env.cwd, "README.md", "# One\n");
    write(&env.cwd, "docs/a.md", "# A\n");
    write(&env.cwd, "docs/b.md", "# B\n");
    // The server has README.md and docs/a.md as they are on disk, and docs/b.md as an older version.
    fake.state().sources =
        vec![listed("README.md", "# One\n"), listed("docs/a.md", "# A\n"), listed("docs/b.md", "# B old\n")];

    let out = env.ingest(&[]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(uris(&fake.posted_sources()), ["docs/b.md"]);
    assert!(out.stdout.contains("~ docs/b.md") && out.stdout.contains("2 unchanged"), "{}", out.stdout);

    write(&env.cwd, "docs/new.md", "# New\n");
    let out = env.ingest(&[]);
    assert!(out.stdout.contains("+ docs/new.md") && out.stdout.contains("~ docs/b.md"), "{}", out.stdout);
}

#[test]
fn dry_run_lists_without_sending() {
    let (fake, env) = TestEnv::with_fake();
    write(&env.cwd, "README.md", "# One\n");
    write(&env.cwd, "gone.md", "# Gone\n");
    fake.state().sources = vec![listed("old.md", "x")];
    let out = env.ingest(&["--dry-run", "--prune"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("+ README.md") && out.stdout.contains("- old.md"), "{}", out.stdout);
    assert!(out.stdout.contains("would send 2 new") && out.stdout.contains("nothing sent"), "{}", out.stdout);
    assert!(fake.posted_sources().is_empty());
    assert!(fake.matching("DELETE", "/v1/sources").is_empty());
}

#[test]
fn prune_removes_the_sources_of_files_that_disappeared_under_the_directory() {
    let (fake, env) = TestEnv::with_fake();
    git_init(&env.cwd);
    write(&env.cwd, "docs/a.md", "# A\n");
    write(&env.cwd, "docs/old name & more.md", "# Kept\n");
    fake.state().sources = vec![
        listed("docs/a.md", "# A\n"),
        listed("docs/old name & more.md", "# Kept\n"),
        listed("docs/removed.md", "x"),
        listed("docs/sub/removed too.md", "x"),
        listed("README.md", "x"),
        listed("docs-other/x.md", "x"),
    ];

    // Without --prune nothing is deleted.
    assert_eq!(env.ingest(&["docs"]).code, Some(0));
    assert!(fake.matching("DELETE", "/v1/sources").is_empty());

    let out = env.ingest(&["docs", "--prune"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let mut deleted: Vec<String> = fake.matching("DELETE", "/v1/sources").into_iter().map(|r| r.path).collect();
    deleted.sort();
    assert_eq!(
        deleted,
        [
            "/v1/sources?uri=docs/removed.md&scope=project:local/proj",
            "/v1/sources?uri=docs/sub/removed%20too.md&scope=project:local/proj",
        ]
    );
    assert!(out.stdout.contains("remove 2"), "{}", out.stdout);
    // The uris are relative to the git root, not to the directory that was named.
    assert!(fake.posted_sources().is_empty());

    // A file cannot be pruned.
    let out = env.ingest(&["docs/a.md", "--prune"]);
    assert_eq!(out.code, Some(1));
    assert!(out.stderr.contains("--prune applies to a directory"), "{}", out.stderr);
}

#[test]
fn a_single_file_and_the_global_scope() {
    let (fake, env) = TestEnv::with_fake();
    write(&env.cwd, "notes/vim.txt", "use :wq");
    let file = std::fs::canonicalize(env.cwd.join("notes/vim.txt")).unwrap();
    let out = env.ingest(&["notes/vim.txt", "--scope", "global"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let posted = fake.posted_sources();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0]["scope"], json!({"kind": "global", "key": ""}));
    assert_eq!(posted[0]["uri"], file.to_str().unwrap());
    assert_eq!(posted[0]["content_type"], "text");
    assert_eq!(fake.matching("GET", "/v1/sources")[0].path, "/v1/sources?scope=global:");

    // A named file is sent whatever its extension.
    write(&env.cwd, "Makefile", "build:\n\tcargo build\n");
    assert_eq!(env.ingest(&["Makefile"]).code, Some(0));
    assert_eq!(fake.posted_sources().last().unwrap()["uri"], "Makefile");
}

#[test]
fn glob_picks_the_files() {
    let (fake, env) = TestEnv::with_fake();
    write(&env.cwd, "README.md", "# One\n");
    write(&env.cwd, "deploy/app.yaml", "kind: Deployment\n");
    write(&env.cwd, "deploy/db/pg.yaml", "kind: StatefulSet\n");
    let out = env.ingest(&[".", "--glob", "**/*.yaml"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let posted = fake.posted_sources();
    assert_eq!(uris(&posted), ["deploy/app.yaml", "deploy/db/pg.yaml"]);
    assert!(posted.iter().all(|b| b["content_type"] == "text"));
}

#[test]
fn rate_limited_posts_are_retried() {
    let (fake, env) = TestEnv::with_fake();
    write(&env.cwd, "README.md", "# One\n");
    fake.state().sources_throttled = 2;
    let out = env.ingest(&[]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(fake.posted_sources().len(), 3, "two refusals and the post that went through");
    assert!(out.stdout.contains("send 1 new"), "{}", out.stdout);
}

#[test]
fn failures_exit_non_zero_and_are_reported_per_file() {
    let (fake, env) = TestEnv::with_fake();
    write(&env.cwd, "ok.md", "# Fine\n");
    write(&env.cwd, "too-big.md", "# Big\n");
    fake.state().sources_reject = Some("too-big".into());
    let out = env.ingest(&[]);
    assert_eq!(out.code, Some(1), "{}{}", out.stdout, out.stderr);
    assert!(out.stderr.contains("error: too-big.md: 413 content is too big"), "{}", out.stderr);
    assert!(out.stdout.contains("+ ok.md") && !out.stdout.contains("+ too-big.md"), "{}", out.stdout);
    assert!(out.stdout.contains("1 failed"), "{}", out.stdout);
    assert_eq!(fake.posted_sources().len(), 2, "the other file is still sent");
}

#[test]
fn an_unreachable_server_a_missing_config_and_bad_arguments() {
    let (fake, env) = TestEnv::with_fake();
    write(&env.cwd, "README.md", "# One\n");
    fake.state().down = true;
    let out = env.ingest(&[]);
    assert_eq!(out.code, Some(1));
    assert!(out.stderr.contains("cannot list sources: 503"), "{}", out.stderr);

    std::fs::remove_file(env.home.join("config")).unwrap();
    let out = env.ingest(&[]);
    assert_eq!(out.code, Some(1));
    assert!(out.stderr.contains("MEMORY_URL"), "{}", out.stderr);

    for args in [&["--scope", "device"][..], &["--bogus"], &["a", "b"]] {
        let out = env.ingest(args);
        assert_eq!(out.code, Some(2), "{args:?}");
        assert!(out.stderr.contains("usage: mem-hook ingest"), "{}", out.stderr);
    }
    let out = env.ingest(&["missing-dir"]);
    assert_eq!(out.code, Some(1));
}

#[test]
fn the_observer_guard_does_not_swallow_ingest() {
    let (_fake, mut env) = TestEnv::with_fake();
    write(&env.cwd, "README.md", "# One\n");
    env.extra_env.push(("MEMORY_OBSERVER".into(), "1".into()));
    let out = env.ingest(&["--dry-run"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("would send 1 new"), "{}", out.stdout);
}
