#![allow(
    clippy::result_large_err,
    reason = "ContractFailure is the shared unboxed command-handler error type"
)]

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{cli::ParsedInvocation, contract::ContractFailure};

const MARKER_NAME: &str = ".lspc-managed.json";
const JOURNAL_PREFIX: &str = ".lspc-journal-";
const LOCK_NAME: &str = ".lspc-install.lock";
const SKILL_FILES: &[(&str, &[u8])] = &[
    (
        "CONFIGURATION.md",
        include_bytes!("../skills/lspc/CONFIGURATION.md"),
    ),
    (
        "MUTATIONS.md",
        include_bytes!("../skills/lspc/MUTATIONS.md"),
    ),
    ("QUERYING.md", include_bytes!("../skills/lspc/QUERYING.md")),
    ("SKILL.md", include_bytes!("../skills/lspc/SKILL.md")),
];

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedMarker {
    format_version: u8,
    manager: String,
    skill_version: String,
    digest: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InstallJournal {
    format_version: u8,
    destination: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
    digest: String,
    outcome: String,
    previous_skill_version: Option<String>,
    previous_digest: Option<String>,
}

#[derive(Debug)]
struct ExistingInstallation {
    exists: bool,
    marker: Option<ManagedMarker>,
    actual_digest: Option<String>,
}

pub(crate) fn install(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let global = invocation.has_option("--global");
    let scope = if global { "global" } else { "local" };
    let base = if global {
        directories::BaseDirs::new()
            .ok_or_else(|| user_path_failure("The home directory is unavailable."))?
            .home_dir()
            .to_path_buf()
    } else {
        std::env::current_dir().map_err(|error| install_failure(scope, Path::new("."), &error))?
    };
    let destination = base.join(".agent/skills/lspc");
    let result = install_to(&destination, scope, invocation.has_option("--replace"))?;
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": invocation.command_path(),
        "result": result
    }))
}

fn install_to(destination: &Path, scope: &str, replace: bool) -> Result<Value, ContractFailure> {
    let destination = absolute_utf8_path(destination, scope)?;
    let parent = destination.parent().unwrap();
    create_safe_parent(parent).map_err(|error| install_failure(scope, &destination, &error))?;
    let lock_path = parent.join(LOCK_NAME);
    reject_unsafe_path(&lock_path).map_err(|error| install_failure(scope, &destination, &error))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| install_failure(scope, &destination, &error))?;
    lock.try_lock().map_err(|error| {
        install_failure(
            scope,
            &destination,
            &io::Error::other(format!("installation is already active: {error}")),
        )
    })?;

    if let Some(journal) = read_journal(parent, scope, &destination)?
        && finish_journal(parent, scope, &journal)?
    {
        return Ok(success_result(scope, &journal));
    }

    let embedded_digest = bundle_digest(SKILL_FILES.iter().map(|(path, bytes)| (*path, *bytes)));
    let existing = inspect_installation(&destination)
        .map_err(|error| conflict(scope, &destination, &error.to_string(), false))?;
    let current_version = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    let unchanged = existing.marker.as_ref().is_some_and(|marker| {
        marker.format_version == 1
            && marker.manager == "lspc"
            && marker.skill_version == env!("CARGO_PKG_VERSION")
            && marker.digest == embedded_digest
            && existing.actual_digest.as_ref() == Some(&embedded_digest)
    });
    if unchanged {
        return Ok(json!({
            "scope": scope,
            "resolvedPath": destination,
            "skillVersion": env!("CARGO_PKG_VERSION"),
            "digest": embedded_digest,
            "outcome": "unchanged",
            "previousSkillVersion": env!("CARGO_PKG_VERSION"),
            "previousDigest": embedded_digest,
            "files": installed_files()
        }));
    }

    let valid_managed = existing.marker.as_ref().is_some_and(|marker| {
        marker.format_version == 1
            && marker.manager == "lspc"
            && existing.actual_digest.as_ref() == Some(&marker.digest)
    });
    let older_managed = valid_managed
        && existing.marker.as_ref().is_some_and(|marker| {
            Version::parse(&marker.skill_version).is_ok_and(|version| version < current_version)
        });
    let outcome = if !existing.exists {
        "installed"
    } else if older_managed {
        "upgraded"
    } else if replace {
        "replaced"
    } else {
        return Err(conflict(
            scope,
            &destination,
            if valid_managed {
                "The managed installation is modified or is not older than this binary."
            } else {
                "The destination is unmanaged or has an invalid managed marker."
            },
            true,
        ));
    };

    let previous_skill_version = existing
        .marker
        .as_ref()
        .filter(|marker| {
            marker.manager == "lspc"
                && marker.format_version == 1
                && Version::parse(&marker.skill_version).is_ok()
        })
        .map(|marker| marker.skill_version.clone());
    let previous_digest = existing
        .marker
        .as_ref()
        .filter(|marker| {
            marker.manager == "lspc"
                && marker.format_version == 1
                && valid_sha256_digest(&marker.digest)
        })
        .map(|marker| marker.digest.clone());
    let stage = create_random_sibling(parent, ".lspc-stage-")
        .map_err(|error| install_failure(scope, &destination, &error))?;
    if let Err(error) = write_embedded_skill(&stage, &embedded_digest)
        .and_then(|()| verify_embedded_installation(&stage, &embedded_digest))
    {
        let _ = fs::remove_dir_all(&stage);
        return Err(install_failure(scope, &destination, &error));
    }
    let suffix = stage.file_name().unwrap().to_string_lossy().into_owned();
    let journal = InstallJournal {
        format_version: 1,
        destination: destination.clone(),
        stage,
        backup: parent.join(format!(".lspc-backup-{suffix}")),
        digest: embedded_digest,
        outcome: outcome.to_owned(),
        previous_skill_version,
        previous_digest,
    };
    write_journal(parent, &journal)
        .map_err(|error| install_failure(scope, &destination, &error))?;
    let completed = finish_journal(parent, scope, &journal)?;
    debug_assert!(completed);
    Ok(success_result(scope, &journal))
}

fn success_result(scope: &str, journal: &InstallJournal) -> Value {
    json!({
        "scope": scope,
        "resolvedPath": journal.destination,
        "skillVersion": env!("CARGO_PKG_VERSION"),
        "digest": journal.digest,
        "outcome": journal.outcome,
        "previousSkillVersion": journal.previous_skill_version,
        "previousDigest": journal.previous_digest,
        "files": installed_files()
    })
}

fn installed_files() -> Vec<&'static str> {
    let mut files = vec![MARKER_NAME];
    files.extend(SKILL_FILES.iter().map(|(path, _)| *path));
    files.sort_unstable();
    files
}

fn inspect_installation(path: &Path) -> io::Result<ExistingInstallation> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ExistingInstallation {
                exists: false,
                marker: None,
                actual_digest: None,
            });
        }
        Err(error) => return Err(error),
        Ok(metadata) if unsafe_metadata(&metadata) || !metadata.is_dir() => {
            return Err(io::Error::other(
                "destination must be a directory without symlinks or reparse points",
            ));
        }
        Ok(_) => {}
    }
    let files = read_tree(path)?;
    let marker = files
        .get(MARKER_NAME)
        .and_then(|bytes| serde_json::from_slice(bytes).ok());
    let actual_digest = bundle_digest(
        files
            .iter()
            .filter(|(name, _)| name.as_str() != MARKER_NAME)
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
    );
    Ok(ExistingInstallation {
        exists: true,
        marker,
        actual_digest: Some(actual_digest),
    })
}

fn read_tree(root: &Path) -> io::Result<BTreeMap<String, Vec<u8>>> {
    fn visit(
        root: &Path,
        directory: &Path,
        files: &mut BTreeMap<String, Vec<u8>>,
    ) -> io::Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if unsafe_metadata(&metadata) {
                return Err(io::Error::other(
                    "symlinks and reparse points are not allowed",
                ));
            }
            if metadata.is_dir() {
                visit(root, &entry.path(), files)?;
            } else if metadata.is_file() {
                let relative = entry.path().strip_prefix(root).unwrap().to_path_buf();
                let relative = relative
                    .components()
                    .map(|component| component.as_os_str().to_str())
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| io::Error::other("skill paths must be UTF-8"))?
                    .join("/");
                files.insert(relative, fs::read(entry.path())?);
            } else {
                return Err(io::Error::other("unsupported skill filesystem entry"));
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn bundle_digest<'a>(files: impl Iterator<Item = (&'a str, &'a [u8])>) -> String {
    let files = files.collect::<BTreeMap<_, _>>();
    let mut hasher = Sha256::new();
    hasher.update(b"lspc-skill-v1\0");
    hasher.update((files.len() as u64).to_be_bytes());
    for (path, bytes) in files {
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn write_embedded_skill(stage: &Path, digest: &str) -> io::Result<()> {
    for (relative, bytes) in SKILL_FILES {
        write_synced(&stage.join(relative), bytes)?;
    }
    let marker = ManagedMarker {
        format_version: 1,
        manager: "lspc".to_owned(),
        skill_version: env!("CARGO_PKG_VERSION").to_owned(),
        digest: digest.to_owned(),
    };
    write_synced(&stage.join(MARKER_NAME), &serde_json::to_vec(&marker)?)?;
    sync_directory(stage)
}

fn create_random_sibling(parent: &Path, prefix: &str) -> io::Result<PathBuf> {
    for _ in 0..32 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let path = parent.join(format!("{prefix}{}", hex::encode(random)));
        match fs::create_dir(&path) {
            Ok(()) => {
                restrict_directory(&path)?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique staging directory",
    ))
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    restrict_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn verify_embedded_installation(path: &Path, expected: &str) -> io::Result<()> {
    let installation = inspect_installation(path)?;
    let marker = installation
        .marker
        .ok_or_else(|| io::Error::other("staged marker is invalid"))?;
    if marker.format_version != 1
        || marker.manager != "lspc"
        || marker.skill_version != env!("CARGO_PKG_VERSION")
        || marker.digest != expected
        || installation.actual_digest.as_deref() != Some(expected)
    {
        return Err(io::Error::other("staged skill verification failed"));
    }
    Ok(())
}

fn write_journal(parent: &Path, journal: &InstallJournal) -> io::Result<()> {
    let path = journal_path(parent, journal);
    reject_unsafe_path(&path)?;
    let mut file = AtomicWriteFile::open(&path)?;
    file.write_all(&serde_json::to_vec(journal)?)?;
    file.commit()?;
    sync_directory(parent)
}

fn read_journal(
    parent: &Path,
    scope: &str,
    destination: &Path,
) -> Result<Option<InstallJournal>, ContractFailure> {
    let mut journals = fs::read_dir(parent)
        .map_err(|error| install_failure(scope, destination, &error))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(JOURNAL_PREFIX) && name.ends_with(".json"))
        })
        .collect::<Vec<_>>();
    journals.sort();
    if journals.is_empty() {
        return Ok(None);
    }
    if journals.len() != 1 {
        return Err(install_failure(
            scope,
            destination,
            &io::Error::other("multiple installation journals require manual inspection"),
        ));
    }
    let path = &journals[0];
    reject_unsafe_path(path).map_err(|error| install_failure(scope, destination, &error))?;
    let bytes = fs::read(path).map_err(|error| install_failure(scope, destination, &error))?;
    let journal: InstallJournal = serde_json::from_slice(&bytes).map_err(|error| {
        install_failure(scope, destination, &io::Error::other(error.to_string()))
    })?;
    if journal.format_version != 1 || journal.destination != destination {
        return Err(install_failure(
            scope,
            destination,
            &io::Error::other("installation journal is incompatible"),
        ));
    }
    Ok(Some(journal))
}

fn finish_journal(
    parent: &Path,
    scope: &str,
    journal: &InstallJournal,
) -> Result<bool, ContractFailure> {
    let fail = |error: io::Error| install_failure(scope, &journal.destination, &error);
    if verify_embedded_installation(&journal.destination, &journal.digest).is_err() {
        if verify_embedded_installation(&journal.stage, &journal.digest).is_err() {
            let current_digest =
                bundle_digest(SKILL_FILES.iter().map(|(path, bytes)| (*path, *bytes)));
            if current_digest == journal.digest {
                if journal.stage.exists() {
                    inspect_installation(&journal.stage).map_err(fail)?;
                    fs::remove_dir_all(&journal.stage).map_err(fail)?;
                }
                fs::create_dir(&journal.stage).map_err(fail)?;
                write_embedded_skill(&journal.stage, &journal.digest).map_err(fail)?;
                verify_embedded_installation(&journal.stage, &journal.digest).map_err(fail)?;
            } else {
                restore_journal_backup(parent, journal).map_err(fail)?;
                return Ok(false);
            }
        }
        reject_unsafe_path(&journal.backup).map_err(fail)?;
        if journal.destination.exists() {
            if journal.backup.exists() {
                return Err(fail(io::Error::other(
                    "both destination and backup exist during recovery",
                )));
            }
            inspect_installation(&journal.destination).map_err(fail)?;
            fs::rename(&journal.destination, &journal.backup).map_err(fail)?;
            sync_directory(parent).map_err(fail)?;
        }
        fs::rename(&journal.stage, &journal.destination).map_err(fail)?;
        sync_directory(parent).map_err(fail)?;
        verify_embedded_installation(&journal.destination, &journal.digest).map_err(fail)?;
    }
    if journal.backup.exists() {
        inspect_installation(&journal.backup).map_err(fail)?;
        fs::remove_dir_all(&journal.backup).map_err(fail)?;
    }
    if journal.stage.exists() {
        inspect_installation(&journal.stage).map_err(fail)?;
        fs::remove_dir_all(&journal.stage).map_err(fail)?;
    }
    fs::remove_file(journal_path(parent, journal)).map_err(fail)?;
    sync_directory(parent).map_err(fail)?;
    Ok(true)
}

fn restore_journal_backup(parent: &Path, journal: &InstallJournal) -> io::Result<()> {
    if journal.destination.exists() && journal.backup.exists() {
        return Err(io::Error::other(
            "cannot restore while destination and backup both exist",
        ));
    }
    if !journal.destination.exists() && journal.backup.exists() {
        inspect_installation(&journal.backup)?;
        fs::rename(&journal.backup, &journal.destination)?;
    }
    if journal.stage.exists() {
        inspect_installation(&journal.stage)?;
        fs::remove_dir_all(&journal.stage)?;
    }
    fs::remove_file(journal_path(parent, journal))?;
    sync_directory(parent)
}

fn journal_path(parent: &Path, journal: &InstallJournal) -> PathBuf {
    let suffix = journal
        .stage
        .file_name()
        .unwrap()
        .to_string_lossy()
        .trim_start_matches(".lspc-stage-")
        .to_owned();
    parent.join(format!("{JOURNAL_PREFIX}{suffix}.json"))
}

fn create_safe_parent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !unsafe_metadata(&metadata) && metadata.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::other("installation parent is unsafe")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("installation parent is unavailable"))?;
            create_safe_parent(parent)?;
            fs::create_dir(path)
        }
        Err(error) => Err(error),
    }
}

fn reject_unsafe_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if unsafe_metadata(&metadata) => Err(io::Error::other(
            "symlinks and reparse points are not allowed",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn unsafe_metadata(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    false
}

fn valid_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn restrict_directory(path: &Path) -> io::Result<()> {
    crate::state_permissions::restrict_directory(path)
}

fn restrict_file(path: &Path) -> io::Result<()> {
    crate::state_permissions::restrict_file(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn absolute_utf8_path(path: &Path, scope: &str) -> Result<PathBuf, ContractFailure> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| install_failure(scope, path, &error))?
            .join(path)
    };
    if path.to_str().is_none() {
        return Err(install_failure(
            scope,
            &path,
            &io::Error::other("installation path is not UTF-8"),
        ));
    }
    Ok(path)
}

fn conflict(scope: &str, path: &Path, reason: &str, replace_required: bool) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "skill_install_conflict",
        message: "The companion skill destination cannot be replaced safely.".to_owned(),
        stage: "install_skill",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({
            "scope": scope,
            "resolvedPath": path,
            "reason": reason,
            "replaceRequired": replace_required
        }),
    }
}

fn install_failure(scope: &str, path: &Path, error: &io::Error) -> ContractFailure {
    let mut data = json!({"scope": scope, "resolvedPath": path});
    if let Some(os_code) = error.raw_os_error() {
        data["osCode"] = json!(os_code);
    }
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "skill_install_failed",
        message: "The companion skill could not be installed.".to_owned(),
        stage: "install_skill",
        delivery: "not_applicable",
        retry: "after_change",
        data,
    }
}

fn user_path_failure(reason: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "user_path_unavailable",
        message: "The user home directory is unavailable.".to_owned(),
        stage: "load_configuration",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"kind": "home", "reason": reason}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_upgrades_refuses_and_replaces() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join(".agent/skills/lspc");

        assert_eq!(
            install_to(&destination, "local", false).unwrap()["outcome"],
            "installed"
        );
        assert_eq!(
            install_to(&destination, "local", false).unwrap()["outcome"],
            "unchanged"
        );

        fs::write(destination.join("SKILL.md"), b"modified").unwrap();
        let error = install_to(&destination, "local", false).unwrap_err();
        assert_eq!(error.code, "skill_install_conflict");
        assert_eq!(fs::read(destination.join("SKILL.md")).unwrap(), b"modified");
        assert_eq!(
            install_to(&destination, "local", true).unwrap()["outcome"],
            "replaced"
        );

        let old_files = [("SKILL.md", b"old".as_slice())];
        let old_digest = bundle_digest(old_files.into_iter());
        fs::remove_dir_all(&destination).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("SKILL.md"), b"old").unwrap();
        fs::write(
            destination.join(MARKER_NAME),
            serde_json::to_vec(&ManagedMarker {
                format_version: 1,
                manager: "lspc".to_owned(),
                skill_version: "0.9.0".to_owned(),
                digest: old_digest,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            install_to(&destination, "local", false).unwrap()["outcome"],
            "upgraded"
        );
    }

    #[test]
    fn resumes_after_destination_was_moved_to_backup() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("lspc");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("custom"), b"old").unwrap();
        let digest = bundle_digest(SKILL_FILES.iter().map(|(path, bytes)| (*path, *bytes)));
        let stage = tempfile::Builder::new()
            .prefix(".lspc-stage-")
            .tempdir_in(temporary.path())
            .unwrap()
            .keep();
        write_embedded_skill(&stage, &digest).unwrap();
        let journal = InstallJournal {
            format_version: 1,
            destination: destination.clone(),
            stage,
            backup: temporary.path().join(".lspc-backup-test"),
            digest,
            outcome: "replaced".to_owned(),
            previous_skill_version: None,
            previous_digest: None,
        };
        write_journal(temporary.path(), &journal).unwrap();
        fs::rename(&destination, &journal.backup).unwrap();
        fs::remove_dir_all(&journal.stage).unwrap();

        assert!(finish_journal(temporary.path(), "local", &journal).unwrap());
        verify_embedded_installation(&destination, &journal.digest).unwrap();
        assert!(!journal.backup.exists());
        assert!(!journal_path(temporary.path(), &journal).exists());
    }
}
