use std::{env, fs, path::Path, process::Command};

fn main() {
    println!("cargo::rerun-if-env-changed=LSPC_BUILD_COMMIT");
    println!("cargo::rerun-if-changed=.cargo_vcs_info.json");

    let commit = env::var("LSPC_BUILD_COMMIT")
        .ok()
        .or_else(packaged_commit)
        .or_else(|| {
            watch_git_head();
            git_output(&["rev-parse", "--verify", "HEAD"])
        })
        .expect("set LSPC_BUILD_COMMIT when building outside a Git or Cargo package checkout");

    assert!(
        (7..=64).contains(&commit.len()) && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "LSPC_BUILD_COMMIT must be a 7 to 64 character hexadecimal Git commit"
    );

    println!(
        "cargo::rustc-env=LSPC_BUILD_COMMIT={}",
        commit.to_ascii_lowercase()
    );
    println!(
        "cargo::rustc-env=LSPC_BUILD_TARGET={}",
        env::var("TARGET").unwrap()
    );
}

fn git_output(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(env::var("CARGO_MANIFEST_DIR").ok()?)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn watch_git_head() {
    let mut names = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    if let Some(reference) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        names.push(reference);
    }

    for name in names {
        if let Some(path) = git_output(&["rev-parse", "--git-path", &name]) {
            println!("cargo::rerun-if-changed={path}");
        }
    }
}

fn packaged_commit() -> Option<String> {
    let contents = fs::read_to_string(
        Path::new(&env::var("CARGO_MANIFEST_DIR").ok()?).join(".cargo_vcs_info.json"),
    )
    .ok()?;
    let after_key = contents.split_once("\"sha1\"")?.1;
    Some(after_key.split('"').nth(1)?.to_owned())
}
