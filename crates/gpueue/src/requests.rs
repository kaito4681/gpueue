//! Mapping parsed command line arguments to API requests (pure, so it can be
//! tested without a daemon).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use gpueue_core::model::{DEFAULT_GROUP, GpuIndex, JobId, JobSpec};
use gpueue_protocol::api::{AddJobRequest, JobSelector};

/// Which jobs `-a`, `-g` and ids select, as pueue's `selection_from_params`:
/// `--all` wins over `--group`, which wins over ids; nothing at all means the
/// default group.
pub fn selection(ids: &[usize], group: Option<&str>, all: bool) -> JobSelector {
    explicit_selection(ids, group, all).unwrap_or_else(|| JobSelector::Group {
        group: DEFAULT_GROUP.to_owned(),
    })
}

/// Like [`selection`], but `None` when nothing was given (for commands that
/// must not silently default to the default group).
pub fn explicit_selection(ids: &[usize], group: Option<&str>, all: bool) -> Option<JobSelector> {
    if all {
        Some(JobSelector::All)
    } else if let Some(group) = group {
        Some(JobSelector::Group {
            group: group.to_owned(),
        })
    } else if !ids.is_empty() {
        Some(JobSelector::Ids { ids: job_ids(ids) })
    } else {
        None
    }
}

pub fn job_ids(ids: &[usize]) -> Vec<JobId> {
    ids.iter().map(|&id| JobId(id as u64)).collect()
}

pub fn gpu_indices(gpus: &[u32]) -> Vec<GpuIndex> {
    gpus.iter().map(|&index| GpuIndex(index)).collect()
}

pub fn timestamp(at: &DateTime<Local>) -> i64 {
    at.timestamp_millis()
}

/// The arguments of `add` that end up in the request.
#[derive(Debug, Clone, Default)]
pub struct AddArgs<'a> {
    pub command: &'a [String],
    pub escape: bool,
    pub group: Option<&'a str>,
    pub after: &'a [usize],
    pub priority: Option<i32>,
    pub nice: Option<i32>,
    pub label: Option<&'a str>,
    pub gpu: &'a [u32],
    pub no_checkpoint: bool,
    pub stashed: bool,
    pub delay: Option<DateTime<Local>>,
    pub immediate: bool,
}

/// Builds the `add` request. `cwd` and `envs` are the client's working
/// directory (or `--working-directory`) and its whole environment.
pub fn add_request(
    args: &AddArgs<'_>,
    cwd: PathBuf,
    envs: BTreeMap<String, String>,
) -> AddJobRequest {
    let command = if args.escape {
        args.command
            .iter()
            .map(|word| shell_escape(word))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        args.command.join(" ")
    };
    AddJobRequest {
        spec: JobSpec {
            command,
            cwd,
            envs,
            group: args.group.unwrap_or(DEFAULT_GROUP).to_owned(),
            gpus: gpu_indices(args.gpu),
            dependencies: job_ids(args.after),
            priority: args.priority.unwrap_or(0),
            label: args.label.map(str::to_owned),
            no_checkpoint: args.no_checkpoint,
            nice: args.nice.unwrap_or(0),
        },
        // As in pueue, a delayed job is stashed until its time comes.
        stashed: args.stashed || args.delay.is_some(),
        enqueue_at: args.delay.as_ref().map(timestamp),
        immediate: args.immediate,
    }
}

/// Quotes `word` for `sh` unless it only contains safe characters (the same
/// rule as the `shell-escape` crate pueue uses).
pub fn shell_escape(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_=/,.+".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Rejects a working directory the JSON request cannot carry: paths travel
/// as strings, so they must be valid UTF-8.
pub fn check_working_directory(cwd: &Path) -> Result<(), String> {
    if cwd.to_str().is_some() {
        Ok(())
    } else {
        Err(format!(
            "the working directory {} is not valid UTF-8; gpueue can only run jobs in \
             directories whose path is valid UTF-8 (cd elsewhere or pass --working-directory)",
            cwd.display()
        ))
    }
}

/// The client's environment, skipping variables that are not valid UTF-8
/// (they cannot be represented in the JSON request).
pub fn client_environment() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn selection_prefers_all_then_group_then_ids() {
        assert_eq!(selection(&[1], Some("g"), true), JobSelector::All);
        assert_eq!(
            selection(&[1], Some("g"), false),
            JobSelector::Group { group: "g".into() }
        );
        assert_eq!(
            selection(&[1, 2], None, false),
            JobSelector::Ids {
                ids: vec![JobId(1), JobId(2)]
            }
        );
        assert_eq!(
            selection(&[], None, false),
            JobSelector::Group {
                group: "default".into()
            }
        );
        assert_eq!(explicit_selection(&[], None, false), None);
    }

    #[test]
    fn add_request_maps_every_option() {
        let command = words(&["python", "train.py", "--lr", "0.1"]);
        let delay = Local.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap();
        let args = AddArgs {
            command: &command,
            group: Some("gpu"),
            after: &[3, 4],
            priority: Some(-2),
            label: Some("exp"),
            gpu: &[0, 1],
            no_checkpoint: true,
            delay: Some(delay),
            ..AddArgs::default()
        };
        let envs = BTreeMap::from([("HOME".to_owned(), "/home/a".to_owned())]);
        let request = add_request(&args, PathBuf::from("/work"), envs.clone());
        assert_eq!(request.spec.command, "python train.py --lr 0.1");
        assert_eq!(request.spec.cwd, PathBuf::from("/work"));
        assert_eq!(request.spec.envs, envs);
        assert_eq!(request.spec.group, "gpu");
        assert_eq!(request.spec.gpus, vec![GpuIndex(0), GpuIndex(1)]);
        assert_eq!(request.spec.dependencies, vec![JobId(3), JobId(4)]);
        assert_eq!(request.spec.priority, -2);
        assert_eq!(request.spec.label.as_deref(), Some("exp"));
        assert!(request.spec.no_checkpoint);
        assert!(request.stashed, "a delay implies stashed");
        assert_eq!(request.enqueue_at, Some(delay.timestamp_millis()));
        assert!(!request.immediate);
    }

    #[test]
    fn add_request_defaults() {
        let command = words(&["ls"]);
        let args = AddArgs {
            command: &command,
            immediate: true,
            ..AddArgs::default()
        };
        let request = add_request(&args, PathBuf::from("/"), BTreeMap::new());
        assert_eq!(request.spec.group, "default");
        assert!(request.spec.gpus.is_empty());
        assert_eq!(request.spec.priority, 0);
        assert!(!request.stashed);
        assert_eq!(request.enqueue_at, None);
        assert!(request.immediate);
    }

    #[test]
    fn non_utf8_working_directory_is_rejected() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        assert!(check_working_directory(Path::new("/work/é")).is_ok());
        let bad = Path::new(OsStr::from_bytes(b"/tmp/\xff"));
        let err = check_working_directory(bad).unwrap_err();
        assert!(err.contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn escape_quotes_special_words() {
        let command = words(&["echo", "a b", "it's", "$HOME", "plain-word"]);
        let args = AddArgs {
            command: &command,
            escape: true,
            ..AddArgs::default()
        };
        let request = add_request(&args, PathBuf::from("/"), BTreeMap::new());
        assert_eq!(
            request.spec.command,
            r#"echo 'a b' 'it'\''s' '$HOME' plain-word"#
        );
        assert_eq!(shell_escape(""), "''");
    }
}
