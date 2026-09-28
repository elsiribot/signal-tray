//! Where each account's files live in the data directory:
//!
//! ```text
//! accounts/<id>/presage.sqlite          presage's keys, sessions, contacts, groups
//! accounts/<id>/messages.sqlite         the app's message history
//! accounts/<id>/history-import-pending  while Link & Sync is unfinished
//! accounts/<id>/attachments/            downloaded and sent images
//! accounts/<id>/avatars/                pictures of contacts and groups
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::config::Config;
use crate::model::AccountId;

const ACCOUNTS_DIR: &str = "accounts";
const PRESAGE_DB: &str = "presage.sqlite";
const MESSAGES_DB: &str = "messages.sqlite";
const HISTORY_MARKER: &str = "history-import-pending";
const ATTACHMENTS_DIR: &str = "attachments";
const AVATARS_DIR: &str = "avatars";

/// The account the files of the single-account layout move to.
const MIGRATED_ID: &str = "1";

pub struct AccountPaths {
    pub dir: PathBuf,
    pub presage: PathBuf,
    pub messages: PathBuf,
    pub history_marker: PathBuf,
    pub attachments: PathBuf,
    pub avatars: PathBuf,
}

pub fn paths(data_dir: &Path, id: &AccountId) -> AccountPaths {
    let dir = data_dir.join(ACCOUNTS_DIR).join(id.as_str());
    AccountPaths {
        presage: dir.join(PRESAGE_DB),
        messages: dir.join(MESSAGES_DB),
        history_marker: dir.join(HISTORY_MARKER),
        attachments: dir.join(ATTACHMENTS_DIR),
        avatars: dir.join(AVATARS_DIR),
        dir,
    }
}

/// The smallest positive number that is neither configured nor has a
/// directory, so a new account never picks up leftover files.
pub fn next_id(data_dir: &Path, configured: &[AccountId]) -> AccountId {
    (1u32..)
        .map(|n| AccountId::try_from(n.to_string()).expect("numbers are valid account ids"))
        .find(|id| !configured.contains(id) && !paths(data_dir, id).dir.exists())
        .expect("some number is free")
}

/// Moves the files of the single-account layout into their own account.
/// The account is added to the config and `save`d first, so an interrupted
/// move can never leave an unlisted directory that [`remove_unlisted`]
/// would delete; the remaining files move on the next start.
pub fn migrate_single_account(
    data_dir: &Path,
    config: &mut Config,
    save: impl FnOnce(&Config) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let legacy: Vec<PathBuf> = std::fs::read_dir(data_dir)
        .with_context(|| format!("reading {}", data_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| {
                        // Including SQLite's -wal and -shm files.
                        name.starts_with(PRESAGE_DB)
                            || name.starts_with(MESSAGES_DB)
                            || name == HISTORY_MARKER
                    })
        })
        .collect();
    if legacy.is_empty() {
        return Ok(());
    }
    let id = AccountId::try_from(MIGRATED_ID.to_owned()).expect("valid account id");
    if config.account(&id).is_none() {
        config.add_account(id.clone());
        save(config)?;
    }
    let dir = paths(data_dir, &id).dir;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for from in legacy {
        let to = dir.join(from.file_name().expect("listed files have names"));
        if to.exists() {
            tracing::warn!(from = %from.display(), to = %to.display(), "not migrating over an existing file");
            continue;
        }
        std::fs::rename(&from, &to).with_context(|| format!("moving {}", from.display()))?;
    }
    tracing::info!(account = %id, "moved the single-account files into their own account");
    Ok(())
}

/// Deletes account directories that aren't configured: accounts are
/// configured once linked, so these are abandoned link attempts.
pub fn remove_unlisted(data_dir: &Path, configured: &[AccountId]) {
    let Ok(entries) = std::fs::read_dir(data_dir.join(ACCOUNTS_DIR)) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let listed = AccountId::try_from(name.clone()).is_ok_and(|id| configured.contains(&id));
        if !listed {
            tracing::info!(account = %name, "removing an abandoned link attempt");
            remove_dir(&entry.path());
        }
    }
}

/// Deletes an account's files.
pub fn remove(data_dir: &Path, id: &AccountId) {
    remove_dir(&paths(data_dir, id).dir);
}

fn remove_dir(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(%err, dir = %dir.display(), "removing account files failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AccountId {
        AccountId::try_from(s.to_owned()).expect("valid account id")
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("signal-tray-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir is writable");
        dir
    }

    #[test]
    fn next_id_skips_configured_ids_and_leftover_directories() {
        let dir = temp_dir("next-id");
        assert_eq!(next_id(&dir, &[]), id("1"));
        std::fs::create_dir_all(paths(&dir, &id("2")).dir).expect("dir created");
        assert_eq!(next_id(&dir, &[id("1")]), id("3"));
        assert_eq!(next_id(&dir, &[id("3")]), id("1"));
        std::fs::remove_dir_all(&dir).expect("temp dir is removable");
    }

    #[test]
    fn single_account_files_move_into_account_1() {
        let dir = temp_dir("migrate");
        let files = [
            "presage.sqlite",
            "presage.sqlite-wal",
            "messages.sqlite",
            "messages.sqlite-shm",
            "history-import-pending",
        ];
        for file in files {
            std::fs::write(dir.join(file), file).expect("file written");
        }
        std::fs::write(dir.join("unrelated"), "").expect("file written");

        let mut config = Config::default();
        let mut saved = None;
        migrate_single_account(&dir, &mut config, |config| {
            assert!(
                dir.join("messages.sqlite").exists(),
                "the account is saved before any file moves"
            );
            saved = Some(config.accounts.clone());
            Ok(())
        })
        .expect("migration succeeds");
        assert_eq!(saved.as_ref(), Some(&config.accounts));
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].id, id("1"));
        let account = paths(&dir, &id("1")).dir;
        for file in files {
            assert!(!dir.join(file).exists(), "{file} moved away");
            assert_eq!(
                std::fs::read_to_string(account.join(file)).expect("file moved"),
                file
            );
        }
        assert!(dir.join("unrelated").exists());

        migrate_single_account(&dir, &mut config, |_| panic!("nothing left to migrate"))
            .expect("second run succeeds");
        assert_eq!(config.accounts.len(), 1);
        std::fs::remove_dir_all(&dir).expect("temp dir is removable");
    }

    #[test]
    fn interrupted_migration_resumes_without_duplicating_the_account() {
        let dir = temp_dir("migrate-resume");
        let mut config = Config::default();
        config.add_account(id("1"));
        std::fs::write(dir.join("messages.sqlite"), "").expect("file written");
        migrate_single_account(&dir, &mut config, |_| {
            panic!("the account was already configured")
        })
        .expect("migration succeeds");
        assert_eq!(config.accounts.len(), 1);
        assert!(paths(&dir, &id("1")).messages.exists());
        std::fs::remove_dir_all(&dir).expect("temp dir is removable");
    }

    #[test]
    fn unlisted_account_directories_are_removed() {
        let dir = temp_dir("unlisted");
        for account in ["1", "2"] {
            std::fs::create_dir_all(paths(&dir, &id(account)).dir).expect("dir created");
        }
        remove_unlisted(&dir, &[id("1")]);
        assert!(paths(&dir, &id("1")).dir.exists());
        assert!(!paths(&dir, &id("2")).dir.exists());
        std::fs::remove_dir_all(&dir).expect("temp dir is removable");
    }
}
