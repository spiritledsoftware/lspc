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

const MARKER_NAME: &str = ".lspctl-managed.json";
const JOURNAL_PREFIX: &str = ".lspctl-journal-";
const LOCK_NAME: &str = ".lspctl-install.lock";
const SKILL_FILES: &[(&str, &[u8])] = &[
    (
        "CONFIGURATION.md",
        include_bytes!("../skills/lspctl/CONFIGURATION.md"),
    ),
    (
        "MUTATIONS.md",
        include_bytes!("../skills/lspctl/MUTATIONS.md"),
    ),
    (
        "QUERYING.md",
        include_bytes!("../skills/lspctl/QUERYING.md"),
    ),
    ("SKILL.md", include_bytes!("../skills/lspctl/SKILL.md")),
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
    let result = install_to(&base, scope, invocation.has_option("--replace"))?;
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": invocation.command_path(),
        "result": result
    }))
}

fn install_to(base: &Path, scope: &str, replace: bool) -> Result<Value, ContractFailure> {
    // Resolve only the selected base; aliases above it are legitimate (e.g. macOS /tmp).
    let selected_destination = base.join(".agent/skills/lspctl");
    let base = dunce::canonicalize(base).map_err(|error| install_failure(scope, base, &error))?;
    let destination = base.join(".agent/skills/lspctl");
    if destination.to_str().is_none() {
        return Err(install_failure(
            scope,
            &destination,
            &io::Error::other("installation path is not UTF-8"),
        ));
    }
    let parent = destination.parent().unwrap();
    create_safe_parent(&base, parent)
        .map_err(|error| install_failure(scope, &destination, &error))?;
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

    if let Some((path, journal)) = read_journal(parent, scope, &destination, &selected_destination)?
    {
        finish_journal(&path, scope, &journal, replace)?;
        return Ok(success_result(scope, &journal));
    }

    let embedded_digest = bundle_digest(SKILL_FILES.iter().map(|(path, bytes)| (*path, *bytes)));
    let existing = inspect_installation(&destination)
        .map_err(|error| conflict(scope, &destination, &error.to_string(), false))?;
    let unchanged = existing.marker.as_ref().is_some_and(|marker| {
        marker.format_version == 1
            && marker.manager == "lspctl"
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

    let outcome = replacement_outcome(&existing, replace)
        .map_err(|reason| conflict(scope, &destination, reason, true))?;

    let previous_skill_version = existing
        .marker
        .as_ref()
        .filter(|marker| {
            marker.manager == "lspctl"
                && marker.format_version == 1
                && Version::parse(&marker.skill_version).is_ok()
        })
        .map(|marker| marker.skill_version.clone());
    let previous_digest = existing
        .marker
        .as_ref()
        .filter(|marker| {
            marker.manager == "lspctl"
                && marker.format_version == 1
                && valid_sha256_digest(&marker.digest)
        })
        .map(|marker| marker.digest.clone());
    let stage = create_random_sibling(parent, ".lspctl-stage-")
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
        backup: parent.join(format!(".lspctl-backup-{suffix}")),
        digest: embedded_digest,
        outcome: outcome.to_owned(),
        previous_skill_version,
        previous_digest,
    };
    write_journal(parent, &journal)
        .map_err(|error| install_failure(scope, &destination, &error))?;
    finish_journal(&journal_path(parent, &journal), scope, &journal, replace)?;
    Ok(success_result(scope, &journal))
}

fn replacement_outcome(
    existing: &ExistingInstallation,
    replace: bool,
) -> Result<&'static str, &'static str> {
    let valid_managed = existing.marker.as_ref().is_some_and(|marker| {
        marker.format_version == 1
            && marker.manager == "lspctl"
            && existing.actual_digest.as_ref() == Some(&marker.digest)
    });
    let current_version = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    let older_managed = valid_managed
        && existing.marker.as_ref().is_some_and(|marker| {
            Version::parse(&marker.skill_version).is_ok_and(|version| version < current_version)
        });
    if !existing.exists {
        Ok("installed")
    } else if older_managed {
        Ok("upgraded")
    } else if replace {
        Ok("replaced")
    } else if valid_managed {
        Err("The managed installation is modified or is not older than this binary.")
    } else {
        Err("The destination is unmanaged or has an invalid managed marker.")
    }
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
    hasher.update(b"lspctl-skill-v1\0");
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
        manager: "lspctl".to_owned(),
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
                crate::state_permissions::restrict_directory(&path)?;
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
    crate::state_permissions::restrict_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn verify_embedded_installation(path: &Path, expected: &str) -> io::Result<()> {
    let installation = inspect_installation(path)?;
    // The embedded bundle is flat; its file digest does not cover empty directories.
    for entry in fs::read_dir(path)? {
        if !entry?.file_type()?.is_file() {
            return Err(io::Error::other(
                "unexpected staged directory or special entry",
            ));
        }
    }
    let marker = installation
        .marker
        .ok_or_else(|| io::Error::other("staged marker is invalid"))?;
    if marker.format_version != 1
        || marker.manager != "lspctl"
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
    validate_journal(&path, &parent.join("lspctl"), journal)?;
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
    selected_destination: &Path,
) -> Result<Option<(PathBuf, InstallJournal)>, ContractFailure> {
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
    let mut journal: InstallJournal = serde_json::from_slice(&bytes).map_err(|error| {
        install_failure(scope, destination, &io::Error::other(error.to_string()))
    })?;
    if selected_destination.as_os_str() != destination.as_os_str()
        && journal.destination.as_os_str() == selected_destination.as_os_str()
    {
        // Older v1 journals used the selected base's spelling. Accept only that known
        // alias, already resolved above, never canonicalize journal-controlled paths.
        let selected_path = selected_destination
            .parent()
            .unwrap()
            .join(path.file_name().unwrap());
        validate_journal(&selected_path, selected_destination, &journal)
            .map_err(|error| install_failure(scope, destination, &error))?;
        journal.destination = destination.to_path_buf();
        journal.stage = parent.join(journal.stage.file_name().unwrap());
        journal.backup = parent.join(journal.backup.file_name().unwrap());
    }
    validate_journal(path, destination, &journal)
        .map_err(|error| install_failure(scope, destination, &error))?;
    Ok(Some((path.clone(), journal)))
}

fn validate_journal(path: &Path, destination: &Path, journal: &InstallJournal) -> io::Result<()> {
    let incompatible = || io::Error::other("installation journal is incompatible");
    let parent = destination.parent().ok_or_else(incompatible)?;
    let id = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(JOURNAL_PREFIX))
        .and_then(|name| name.strip_suffix(".json"))
        .filter(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(incompatible)?;
    // Compare raw paths: Path equality normalizes some traversal aliases such as `./`.
    if journal.format_version != 1
        || !destination.is_absolute()
        || destination.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        || journal.destination.as_os_str() != destination.as_os_str()
        || journal.stage.as_os_str() != parent.join(format!(".lspctl-stage-{id}")).as_os_str()
        || journal.backup.as_os_str()
            != parent
                .join(format!(".lspctl-backup-.lspctl-stage-{id}"))
                .as_os_str()
        || path.as_os_str()
            != parent
                .join(format!("{JOURNAL_PREFIX}{id}.json"))
                .as_os_str()
        || !valid_sha256_digest(&journal.digest)
        || !matches!(
            journal.outcome.as_str(),
            "installed" | "upgraded" | "replaced"
        )
        || journal
            .previous_digest
            .as_deref()
            .is_some_and(|digest| !valid_sha256_digest(digest))
        || journal
            .previous_skill_version
            .as_deref()
            .is_some_and(|version| Version::parse(version).is_err())
    {
        return Err(incompatible());
    }
    Ok(())
}

fn finish_journal(
    path: &Path,
    scope: &str,
    journal: &InstallJournal,
    replace: bool,
) -> Result<(), ContractFailure> {
    let fail = |error: io::Error| install_failure(scope, &journal.destination, &error);
    let parent = path
        .parent()
        .ok_or_else(|| fail(io::Error::other("journal parent is unavailable")))?;
    validate_journal(path, &parent.join("lspctl"), journal).map_err(fail)?;
    let current_digest = bundle_digest(SKILL_FILES.iter().map(|(path, bytes)| (*path, *bytes)));
    if journal.digest != current_digest {
        return Err(fail(io::Error::other(
            "the journal bundle is not recognized by this binary",
        )));
    }
    let stage = inspect_installation(&journal.stage).map_err(fail)?;
    if stage.exists {
        verify_embedded_installation(&journal.stage, &journal.digest).map_err(fail)?;
    }
    let destination = inspect_installation(&journal.destination).map_err(fail)?;
    let backup = inspect_installation(&journal.backup).map_err(fail)?;
    let installed = verify_embedded_installation(&journal.destination, &journal.digest).is_ok();
    if !installed {
        if destination.exists && backup.exists {
            return Err(fail(io::Error::other(
                "both destination and backup exist during recovery",
            )));
        }
        replacement_outcome(&destination, replace)
            .map_err(|reason| conflict(scope, &journal.destination, reason, true))?;
    }
    if backup.exists {
        let recognized_predecessor = replacement_outcome(&backup, false) == Ok("upgraded")
            && backup.marker.as_ref().is_some_and(|marker| {
                Some(&marker.digest) == journal.previous_digest.as_ref()
                    && Some(&marker.skill_version) == journal.previous_skill_version.as_ref()
            });
        if !recognized_predecessor && !replace {
            return Err(conflict(
                scope,
                &journal.destination,
                "The recovered backup is not a verified managed predecessor; replacement consent must be renewed.",
                true,
            ));
        }
    }
    // No filesystem changes until all existing resources and current consent are checked.
    if !installed {
        if !stage.exists {
            fs::create_dir(&journal.stage).map_err(fail)?;
            crate::state_permissions::restrict_directory(&journal.stage).map_err(fail)?;
            write_embedded_skill(&journal.stage, &journal.digest).map_err(fail)?;
            verify_embedded_installation(&journal.stage, &journal.digest).map_err(fail)?;
        }
        if destination.exists {
            fs::rename(&journal.destination, &journal.backup).map_err(fail)?;
            sync_directory(parent).map_err(fail)?;
        }
        fs::rename(&journal.stage, &journal.destination).map_err(fail)?;
        sync_directory(parent).map_err(fail)?;
        verify_embedded_installation(&journal.destination, &journal.digest).map_err(fail)?;
    }
    if backup.exists || (!installed && destination.exists) {
        fs::remove_dir_all(&journal.backup).map_err(fail)?;
    }
    if installed && stage.exists {
        fs::remove_dir_all(&journal.stage).map_err(fail)?;
    }
    fs::remove_file(path).map_err(fail)?;
    sync_directory(parent).map_err(fail)?;
    Ok(())
}

fn journal_path(parent: &Path, journal: &InstallJournal) -> PathBuf {
    let suffix = journal
        .stage
        .file_name()
        .unwrap()
        .to_string_lossy()
        .trim_start_matches(".lspctl-stage-")
        .to_owned();
    parent.join(format!("{JOURNAL_PREFIX}{suffix}.json"))
}

fn create_safe_parent(base: &Path, path: &Path) -> io::Result<()> {
    if path == base {
        return Ok(());
    }
    if !path.starts_with(base) {
        return Err(io::Error::other(
            "installation parent is outside the selected base",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("installation parent is unavailable"))?;
    create_safe_parent(base, parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !unsafe_metadata(&metadata) && metadata.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::other("installation parent is unsafe")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(path),
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
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
        let destination = temporary.path().join(".agent/skills/lspctl");

        assert_eq!(
            install_to(temporary.path(), "local", false).unwrap()["outcome"],
            "installed"
        );
        assert_eq!(
            install_to(temporary.path(), "local", false).unwrap()["outcome"],
            "unchanged"
        );

        fs::write(destination.join("SKILL.md"), b"modified").unwrap();
        let error = install_to(temporary.path(), "local", false).unwrap_err();
        assert_eq!(error.code, "skill_install_conflict");
        assert_eq!(fs::read(destination.join("SKILL.md")).unwrap(), b"modified");
        assert_eq!(
            install_to(temporary.path(), "local", true).unwrap()["outcome"],
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
                manager: "lspctl".to_owned(),
                skill_version: "0.0.9".to_owned(),
                digest: old_digest,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            install_to(temporary.path(), "local", false).unwrap()["outcome"],
            "upgraded"
        );
    }

    fn recovery_journal(parent: &Path) -> InstallJournal {
        let id = "0123456789abcdef0123456789abcdef";
        InstallJournal {
            format_version: 1,
            destination: parent.join("lspctl"),
            stage: parent.join(format!(".lspctl-stage-{id}")),
            backup: parent.join(format!(".lspctl-backup-.lspctl-stage-{id}")),
            digest: bundle_digest(SKILL_FILES.iter().map(|(path, bytes)| (*path, *bytes))),
            outcome: "installed".to_owned(),
            previous_skill_version: None,
            previous_digest: None,
        }
    }

    #[test]
    fn skill_recovery_rejects_unbound_paths() {
        for case in [
            "external stage",
            "external backup",
            "relative stage",
            "relative backup",
            "stage aliases destination",
            "backup aliases destination",
            "stage aliases backup",
            "traversal",
            "dot component",
            "invalid suffix",
            "wrong journal name",
            "destination alias",
            "invalid digest",
            "unsupported format",
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let parent = dunce::canonicalize(temporary.path())
                .unwrap()
                .join(".agent/skills");
            fs::create_dir_all(&parent).unwrap();
            let destination = parent.join("lspctl");
            fs::create_dir(&destination).unwrap();
            let mut journal = recovery_journal(&parent);
            write_embedded_skill(&destination, &journal.digest).unwrap();
            let outside = temporary.path().join("unrelated");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("sentinel"), b"keep me").unwrap();
            let mut path = journal_path(&parent, &journal);
            match case {
                "external stage" => journal.stage = outside.clone(),
                "external backup" => journal.backup = outside.clone(),
                "relative stage" => journal.stage = PathBuf::from("relative-stage"),
                "relative backup" => journal.backup = PathBuf::from("relative-backup"),
                "stage aliases destination" => journal.stage = destination.clone(),
                "backup aliases destination" => journal.backup = destination.clone(),
                "stage aliases backup" => journal.stage = journal.backup.clone(),
                "traversal" => {
                    journal.stage = parent
                        .join("../skills")
                        .join(journal.stage.file_name().unwrap())
                }
                "dot component" => {
                    journal.stage = parent.join(".").join(journal.stage.file_name().unwrap())
                }
                "invalid suffix" => journal.stage = parent.join(".lspctl-stage-not-an-id"),
                "wrong journal name" => {
                    path = parent.join(".lspctl-journal-ffffffffffffffffffffffffffffffff.json")
                }
                "destination alias" => journal.destination = parent.join("./lspctl"),
                "invalid digest" => journal.digest = "not-a-digest".to_owned(),
                "unsupported format" => journal.format_version = 2,
                _ => unreachable!(),
            }
            let bytes = serde_json::to_vec(&journal).unwrap();
            fs::write(&path, &bytes).unwrap();
            let before = read_tree(&destination).unwrap();
            let result = install_to(temporary.path(), "local", true);
            assert_eq!(
                fs::read(outside.join("sentinel")).ok().as_deref(),
                Some(b"keep me".as_slice()),
                "{case}"
            );
            assert_eq!(read_tree(&destination).unwrap(), before, "{case}");
            assert_eq!(
                fs::read(&path).ok().as_deref(),
                Some(bytes.as_slice()),
                "{case}"
            );
            assert!(result.is_err(), "{case}: {result:?}");
        }
    }

    #[test]
    fn skill_recovery_does_not_infer_replace_consent() {
        for phase in [
            "before move",
            "after move",
            "after install",
            "missing stage",
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let parent = dunce::canonicalize(temporary.path())
                .unwrap()
                .join(".agent/skills");
            fs::create_dir_all(&parent).unwrap();
            let mut journal = recovery_journal(&parent);
            journal.outcome = "replaced".to_owned();
            fs::create_dir(&journal.destination).unwrap();
            fs::write(journal.destination.join("custom"), b"unmanaged predecessor").unwrap();
            if phase != "missing stage" {
                fs::create_dir(&journal.stage).unwrap();
                write_embedded_skill(&journal.stage, &journal.digest).unwrap();
            }
            if matches!(phase, "after move" | "after install") {
                fs::rename(&journal.destination, &journal.backup).unwrap();
            }
            if phase == "after install" {
                fs::rename(&journal.stage, &journal.destination).unwrap();
            }
            write_journal(&parent, &journal).unwrap();
            let path = journal_path(&parent, &journal);
            let before = read_tree(&parent).unwrap();
            let error = install_to(temporary.path(), "local", false).unwrap_err();
            assert!(
                read_tree(&parent)
                    .unwrap()
                    .iter()
                    .filter(|(name, _)| name.as_str() != LOCK_NAME)
                    .eq(before.iter()),
                "{phase}: contents changed"
            );
            assert!(path.is_file(), "{phase}");
            assert_eq!(error.code, "skill_install_conflict", "{phase}");
            assert_eq!(error.data["replaceRequired"], true, "{phase}");
            assert_eq!(
                install_to(temporary.path(), "local", true).unwrap()["outcome"],
                "replaced",
                "{phase}"
            );
            verify_embedded_installation(&journal.destination, &journal.digest).unwrap();
            assert!(!journal.stage.exists());
            assert!(!journal.backup.exists());
            assert!(!path.exists());
        }
    }

    #[test]
    fn skill_recovery_preserves_unrecognized_stage() {
        for installed in [false, true] {
            for case in [
                "unexpected file",
                "invalid marker",
                "wrong bundle",
                "empty directory",
                "old bundle",
            ] {
                let temporary = tempfile::tempdir().unwrap();
                let parent = dunce::canonicalize(temporary.path())
                    .unwrap()
                    .join(".agent/skills");
                fs::create_dir_all(&parent).unwrap();
                let mut journal = recovery_journal(&parent);
                fs::create_dir(&journal.stage).unwrap();
                write_embedded_skill(&journal.stage, &journal.digest).unwrap();
                if installed {
                    fs::create_dir(&journal.destination).unwrap();
                    write_embedded_skill(&journal.destination, &journal.digest).unwrap();
                }
                match case {
                    "unexpected file" => {
                        fs::write(journal.stage.join("custom"), b"keep me").unwrap()
                    }
                    "invalid marker" => fs::write(journal.stage.join(MARKER_NAME), b"{}").unwrap(),
                    "wrong bundle" => {
                        fs::write(journal.stage.join("SKILL.md"), b"unrecognized bytes").unwrap()
                    }
                    "empty directory" => fs::create_dir(journal.stage.join("unknown")).unwrap(),
                    "old bundle" => {
                        journal.digest = format!("sha256:{}", "0".repeat(64));
                        fs::write(journal.stage.join("custom"), b"unknown old stage").unwrap();
                        fs::create_dir(&journal.backup).unwrap();
                        fs::write(journal.backup.join("custom"), b"old predecessor").unwrap();
                    }
                    _ => unreachable!(),
                }
                write_journal(&parent, &journal).unwrap();
                let before = read_tree(&parent).unwrap();
                let result = install_to(temporary.path(), "local", true);
                let mut after = read_tree(&parent).unwrap();
                after.remove(LOCK_NAME);
                assert!(
                    after == before,
                    "{case}, installed={installed}: contents changed"
                );
                assert!(journal.stage.is_dir());
                if case == "empty directory" {
                    assert!(journal.stage.join("unknown").is_dir());
                }
                assert!(result.is_err(), "{case}, installed={installed}: {result:?}");
            }
        }
    }

    #[test]
    fn skill_recovery_resumes_valid_interruption() {
        for predecessor in ["absent", "managed", "unmanaged"] {
            for phase in [
                "before move",
                "after move",
                "after install",
                "pending cleanup",
                "missing stage",
            ] {
                let temporary = tempfile::tempdir().unwrap();
                let parent = dunce::canonicalize(temporary.path())
                    .unwrap()
                    .join(".agent/skills");
                fs::create_dir_all(&parent).unwrap();
                let mut journal = recovery_journal(&parent);
                if predecessor != "absent" {
                    fs::create_dir(&journal.destination).unwrap();
                    fs::write(journal.destination.join("SKILL.md"), b"old").unwrap();
                    journal.outcome = if predecessor == "managed" {
                        "upgraded"
                    } else {
                        "replaced"
                    }
                    .to_owned();
                    if predecessor == "managed" {
                        let digest = bundle_digest([("SKILL.md", b"old".as_slice())].into_iter());
                        let marker = ManagedMarker {
                            format_version: 1,
                            manager: "lspctl".to_owned(),
                            skill_version: "0.0.9".to_owned(),
                            digest: digest.clone(),
                        };
                        fs::write(
                            journal.destination.join(MARKER_NAME),
                            serde_json::to_vec(&marker).unwrap(),
                        )
                        .unwrap();
                        journal.previous_skill_version = Some(marker.skill_version);
                        journal.previous_digest = Some(digest);
                    }
                }
                if phase != "missing stage" {
                    fs::create_dir(&journal.stage).unwrap();
                    write_embedded_skill(&journal.stage, &journal.digest).unwrap();
                }
                write_journal(&parent, &journal).unwrap();
                if predecessor != "absent"
                    && matches!(phase, "after move" | "after install" | "pending cleanup")
                {
                    fs::rename(&journal.destination, &journal.backup).unwrap();
                }
                if matches!(phase, "after install" | "pending cleanup") {
                    fs::rename(&journal.stage, &journal.destination).unwrap();
                }
                if phase == "pending cleanup" && predecessor != "absent" {
                    fs::remove_dir_all(&journal.backup).unwrap();
                }
                let result =
                    install_to(temporary.path(), "local", predecessor == "unmanaged").unwrap();
                assert_eq!(result["outcome"], journal.outcome, "{predecessor}, {phase}");
                verify_embedded_installation(&journal.destination, &journal.digest).unwrap();
                assert!(!journal.stage.exists());
                assert!(!journal.backup.exists());
                assert!(!journal_path(&parent, &journal).exists());
                assert_eq!(
                    install_to(temporary.path(), "local", false).unwrap()["outcome"],
                    "unchanged"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn skill_recovery_resumes_selected_base_alias() {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().join("base");
        fs::create_dir(&base).unwrap();
        let alias = temporary.path().join("alias");
        std::os::unix::fs::symlink(&base, &alias).unwrap();
        let parent = alias.join(".agent/skills");
        fs::create_dir_all(&parent).unwrap();
        let journal = recovery_journal(&parent);
        fs::create_dir(&journal.stage).unwrap();
        write_embedded_skill(&journal.stage, &journal.digest).unwrap();
        // Format v1 binaries retained the selected HOME spelling in their journals.
        write_journal(&parent, &journal).unwrap();
        let result = install_to(&alias, "global", false);
        assert!(result.is_ok(), "{result:?}");
        verify_embedded_installation(&journal.destination, &journal.digest).unwrap();
        assert!(!journal_path(&parent, &journal).exists());
        assert!(!journal.stage.exists());
    }

    #[cfg(unix)]
    #[test]
    fn skill_recovery_rejects_symlink_resources() {
        for resource in ["stage", "backup", "destination", "journal"] {
            for dangling in [false, true] {
                let temporary = tempfile::tempdir().unwrap();
                let parent = dunce::canonicalize(temporary.path())
                    .unwrap()
                    .join(".agent/skills");
                fs::create_dir_all(&parent).unwrap();
                let journal = recovery_journal(&parent);
                let path = journal_path(&parent, &journal);
                let outside = temporary.path().join("unrelated");
                fs::create_dir(&outside).unwrap();
                fs::write(outside.join("sentinel"), b"preserve external content").unwrap();
                let bytes = serde_json::to_vec(&journal).unwrap();
                fs::write(&path, &bytes).unwrap();
                let link = match resource {
                    "stage" => &journal.stage,
                    "backup" => &journal.backup,
                    "destination" => &journal.destination,
                    "journal" => {
                        fs::remove_file(&path).unwrap();
                        &path
                    }
                    _ => unreachable!(),
                };
                let target = if dangling {
                    outside.join("missing")
                } else {
                    outside.clone()
                };
                std::os::unix::fs::symlink(&target, link).unwrap();
                let result = install_to(temporary.path(), "local", true);
                assert!(result.is_err(), "{resource}, dangling={dangling}");
                assert_eq!(fs::read_link(link).unwrap(), target);
                assert_eq!(
                    fs::read(outside.join("sentinel")).unwrap(),
                    b"preserve external content"
                );
                if resource != "journal" {
                    assert_eq!(fs::read(&path).unwrap(), bytes);
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn skill_recovery_rejects_existing_symlink_ancestor() {
        for ancestor in [".agent", ".agent/skills"] {
            let temporary = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            fs::create_dir(outside.path().join("skills")).unwrap();
            fs::write(outside.path().join("sentinel"), b"untouched").unwrap();
            let link = temporary.path().join(ancestor);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            let before = read_tree(outside.path()).unwrap();
            let result = install_to(temporary.path(), "local", false);
            assert!(read_tree(outside.path()).unwrap() == before, "{ancestor}");
            assert!(result.is_err());
        }
        // Aliases at the selected-base boundary are legitimate, unlike aliases below it.
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().join("base");
        fs::create_dir(&base).unwrap();
        let alias = temporary.path().join("alias");
        std::os::unix::fs::symlink(&base, &alias).unwrap();
        let result = install_to(&alias, "local", false).unwrap();
        assert_eq!(
            result["resolvedPath"],
            json!(
                dunce::canonicalize(&base)
                    .unwrap()
                    .join(".agent/skills/lspctl")
            )
        );
        assert!(base.join(".agent/skills/lspctl/SKILL.md").is_file());
    }

    #[test]
    fn resumes_after_destination_was_moved_to_backup() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("lspctl");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("custom"), b"old").unwrap();
        let digest = bundle_digest(SKILL_FILES.iter().map(|(path, bytes)| (*path, *bytes)));
        let stage = create_random_sibling(temporary.path(), ".lspctl-stage-").unwrap();
        write_embedded_skill(&stage, &digest).unwrap();
        let journal = InstallJournal {
            format_version: 1,
            destination: destination.clone(),
            backup: temporary.path().join(format!(
                ".lspctl-backup-{}",
                stage.file_name().unwrap().to_str().unwrap()
            )),
            stage,
            digest,
            outcome: "replaced".to_owned(),
            previous_skill_version: None,
            previous_digest: None,
        };
        write_journal(temporary.path(), &journal).unwrap();
        fs::rename(&destination, &journal.backup).unwrap();
        fs::remove_dir_all(&journal.stage).unwrap();

        finish_journal(
            &journal_path(temporary.path(), &journal),
            "local",
            &journal,
            true,
        )
        .unwrap();
        verify_embedded_installation(&destination, &journal.digest).unwrap();
        assert!(!journal.backup.exists());
        assert!(!journal_path(temporary.path(), &journal).exists());
    }
}
