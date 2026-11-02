use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use cc_transcript_core::listing_index::{
    EntryChange, EntryMetadata, FileMetadata, ListingBatch, ListingFilter, ListingIndex,
    ListingSelection,
};
use cc_transcript_core::snapshot::Cancellation;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_bser::bytestring::ByteString;
use serde_bser::value::Value;
use sha2::{Digest, Sha256};

struct Session {
    stream: UnixStream,
}

impl Session {
    fn request<C: Serialize, R: DeserializeOwned>(&mut self, command: &C) -> Result<R, String> {
        serde_bser::ser::serialize(&mut self.stream, command).map_err(|error| error.to_string())?;
        let value: Value =
            serde_bser::from_reader(&mut self.stream).map_err(|error| error.to_string())?;
        if let Value::Object(fields) = &value {
            if let Some(error) = fields.get("error") {
                let bytes = serde_bser::ser::serialize(Vec::new(), error)
                    .map_err(|error| error.to_string())?;
                return Err(
                    serde_bser::from_slice::<String>(&bytes).map_err(|error| error.to_string())?
                );
            }
        }
        let bytes =
            serde_bser::ser::serialize(Vec::new(), &value).map_err(|error| error.to_string())?;
        serde_bser::from_slice(&bytes).map_err(|error| error.to_string())
    }
}

#[derive(Deserialize)]
struct SocketName {
    sockname: ByteString,
}

#[derive(Deserialize)]
struct WatchRoot {
    watch: ByteString,
}

#[derive(Deserialize)]
struct RootConfig {
    config: HashMap<String, Value>,
}

#[derive(Serialize)]
struct Query<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<&'a str>,
    fields: [&'static str; 6],
    case_sensitive: bool,
    always_include_directories: bool,
}

#[derive(Deserialize)]
struct QueryResult {
    clock: String,
    #[serde(default)]
    is_fresh_instance: bool,
    files: Vec<QueryEntry>,
}

#[derive(Deserialize)]
struct QueryEntry {
    name: ByteString,
    exists: bool,
    #[serde(rename = "type")]
    kind: Option<String>,
    mtime: Option<i64>,
    mtime_f: Option<f64>,
    size: Option<u64>,
}

fn existing_socket() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("WATCHMAN_SOCK") {
        return Ok(path.into());
    }
    let cancel = Cancellation::default();
    *crate::SCAN_CANCELLATION.lock().expect("scan cancellation") = Some(cancel.clone());
    let interrupt = crate::ScanInterrupt;
    let output = Command::new("watchman")
        .args([
            "--no-spawn",
            "--no-local",
            "--output-encoding=bser-v2",
            "get-sockname",
        ])
        .output();
    if cancel.is_cancelled() {
        std::process::exit(130);
    }
    drop(interrupt);
    let output = output.map_err(|error| format!("watchman: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "watchman: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    let response: SocketName =
        serde_bser::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    Ok(PathBuf::from(OsString::from_vec(
        response.sockname.into_bytes(),
    )))
}

fn ignored_paths(config: &HashMap<String, Value>, name: &str) -> Result<Vec<PathBuf>, String> {
    let values = match config.get(name) {
        Some(Value::Array(values)) => values.clone(),
        None if name == "ignore_vcs" => [".git", ".svn", ".hg", ".jj"]
            .into_iter()
            .map(Value::from)
            .collect(),
        None => Vec::new(),
        _ => return Err(format!("watchman {name} is not an array")),
    };
    values
        .into_iter()
        .map(|value| {
            let path = match value {
                Value::ByteString(bytes) => PathBuf::from(OsString::from_vec(bytes.into_bytes())),
                Value::Utf8String(text) => PathBuf::from(text),
                _ => return Err(format!("watchman {name} contains a non-path value")),
            };
            if path.as_os_str().is_empty()
                || path
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err(format!(
                    "watchman {name} contains a path outside the transcript root"
                ));
            }
            Ok(path)
        })
        .collect()
}

fn admit_coverage(root: &Path, config: &RootConfig) -> Result<(), String> {
    for name in ["ignore_dirs", "ignore_vcs"] {
        for relative in ignored_paths(&config.config, name)? {
            let path = root.join(relative);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if name == "ignore_dirs" || metadata.is_dir() => {
                    return Err(format!(
                        "watchman excludes transcript paths under {}",
                        path.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
    }
    Ok(())
}

fn entry_change(entry: QueryEntry) -> Result<Option<EntryChange>, String> {
    let path = PathBuf::from(OsString::from_vec(entry.name.into_bytes()));
    let wanted = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl"));
    if !wanted {
        return Ok(None);
    }
    if !entry.exists {
        return Ok(Some(EntryChange {
            path,
            metadata: None,
        }));
    }
    let metadata = match entry.kind.as_deref() {
        Some("d") => EntryMetadata::Directory,
        Some("l") => EntryMetadata::Filesystem,
        Some("f" | "b" | "c" | "p" | "s" | "D" | "?") => {
            if entry.mtime.ok_or("watchman omitted transcript mtime")? < 0 {
                EntryMetadata::Filesystem
            } else {
                EntryMetadata::Event(FileMetadata {
                    mtime: entry
                        .mtime_f
                        .ok_or("watchman omitted transcript fractional mtime")?,
                    size: entry.size.ok_or("watchman omitted transcript size")?,
                })
            }
        }
        _ => return Err("watchman omitted the transcript entry type".into()),
    };
    Ok(Some(EntryChange {
        path,
        metadata: Some(metadata),
    }))
}

pub(crate) fn select(root: &Path, filter: ListingFilter<'_>) -> Result<ListingSelection, String> {
    let canonical = root
        .canonicalize()
        .map_err(|error| format!("{}: {error}", root.display()))?;
    let before =
        fs::metadata(&canonical).map_err(|error| format!("{}: {error}", root.display()))?;
    let socket = existing_socket()?;
    let socket_metadata =
        fs::metadata(&socket).map_err(|error| format!("{}: {error}", socket.display()))?;
    let mut session = Session {
        stream: UnixStream::connect(&socket)
            .map_err(|error| format!("{}: {error}", socket.display()))?,
    };
    let wire_root = ByteString::from(canonical.as_os_str().as_bytes().to_vec());
    let watched: WatchRoot = session.request(&("watch", &wire_root))?;
    if watched.watch.as_bytes() != canonical.as_os_str().as_bytes() {
        return Err("watchman returned a different transcript root".into());
    }
    let config: RootConfig = session.request(&("get-config", &wire_root))?;
    admit_coverage(&canonical, &config)?;
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(".cache")
        });
    let identity = format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()));
    let mut index = ListingIndex::open(
        &cache
            .join("cc-transcript/list-v1")
            .join(identity)
            .join("listing.sqlite3"),
    )
    .map_err(|error| error.to_string())?;
    let mut binding = canonical.as_os_str().as_bytes().to_vec();
    for identity in [
        before.dev(),
        before.ino(),
        socket_metadata.dev(),
        socket_metadata.ino(),
    ] {
        binding.extend_from_slice(&identity.to_be_bytes());
    }
    let request = index.request(&binding).map_err(|error| error.to_string())?;
    let response: QueryResult = session.request(&(
        "query",
        &wire_root,
        Query {
            since: request.clock(),
            fields: ["name", "exists", "type", "mtime", "mtime_f", "size"],
            case_sensitive: true,
            always_include_directories: true,
        },
    ))?;
    let after = fs::metadata(&canonical).map_err(|error| format!("{}: {error}", root.display()))?;
    if before.dev() != after.dev() || before.ino() != after.ino() {
        return Err("transcript root changed during listing".into());
    }
    let config: RootConfig = session.request(&("get-config", &wire_root))?;
    admit_coverage(&canonical, &config)?;
    let changes = response
        .files
        .into_iter()
        .filter_map(|entry| entry_change(entry).transpose())
        .collect::<Result<Vec<_>, _>>()?;
    index
        .refresh_and_select(
            &request,
            ListingBatch {
                clock: response.clock,
                fresh_instance: response.is_fresh_instance,
                changes,
            },
            filter,
            |relative| {
                let metadata = fs::metadata(root.join(relative))?;
                Ok(FileMetadata {
                    mtime: cc_transcript_core::discovery::mtime_secs(&metadata),
                    size: metadata.len(),
                })
            },
            |relative| {
                let path = root.join(relative);
                fs::metadata(&path)
                    .map(|metadata| metadata.len())
                    .map_err(|error| {
                        std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
                    })
            },
        )
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: Vec<u8>, seconds: i64, fraction: f64) -> QueryEntry {
        QueryEntry {
            name: name.into(),
            exists: true,
            kind: Some("f".into()),
            mtime: Some(seconds),
            mtime_f: Some(fraction),
            size: Some(17),
        }
    }

    #[test]
    fn positive_fractional_mtimes_keep_all_float_bits_and_negative_times_refresh() {
        for mtime in [0.0, 0.1, 1_735_689_600.123_456_7, 15_000_000_000.25] {
            let change = entry_change(entry(b"session.jsonl".to_vec(), mtime as i64, mtime))
                .unwrap()
                .unwrap();
            let Some(EntryMetadata::Event(metadata)) = change.metadata else {
                panic!("event metadata")
            };
            assert_eq!(metadata.mtime.to_bits(), mtime.to_bits());
            assert_eq!(metadata.size, 17);
        }
        let change = entry_change(entry(b"old.jsonl".to_vec(), -1, -0.099_999_999_999_999_98))
            .unwrap()
            .unwrap();
        assert!(matches!(change.metadata, Some(EntryMetadata::Filesystem)));
    }

    #[test]
    fn raw_parent_names_survive_and_non_utf8_filenames_are_not_discovered() {
        let name = b"project-\xff/session.jsonl".to_vec();
        let change = entry_change(entry(name.clone(), 1, 1.0)).unwrap().unwrap();
        assert_eq!(change.path.as_os_str().as_bytes(), name);
        assert!(entry_change(entry(b"session-\xff.jsonl".to_vec(), 1, 1.0))
            .unwrap()
            .is_none());
        assert!(
            entry_change(entry(b"not-a-transcript.txt".to_vec(), 1, 1.0))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn deleted_and_directory_entries_need_no_file_metadata() {
        let mut deleted = entry(b"gone.jsonl".to_vec(), 1, 1.0);
        deleted.exists = false;
        deleted.kind = None;
        deleted.mtime = None;
        deleted.mtime_f = None;
        deleted.size = None;
        assert!(entry_change(deleted).unwrap().unwrap().metadata.is_none());
        let mut directory = entry(b"directory.jsonl".to_vec(), 1, 1.0);
        directory.kind = Some("d".into());
        directory.mtime = None;
        directory.mtime_f = None;
        directory.size = None;
        assert!(matches!(
            entry_change(directory).unwrap().unwrap().metadata,
            Some(EntryMetadata::Directory)
        ));
    }

    #[test]
    fn server_errors_are_returned_before_success_shape_decoding() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let command: Vec<String> = serde_bser::from_reader(&mut server).unwrap();
            assert_eq!(command, ["query"]);
            serde_bser::ser::serialize(server, HashMap::from([("error", "permission denied")]))
                .unwrap();
        });
        let result: Result<QueryResult, _> = Session { stream: client }.request(&["query"]);
        assert_eq!(result.err().as_deref(), Some("permission denied"));
        worker.join().unwrap();
    }

    #[test]
    fn present_ignored_prefix_is_explicitly_not_a_complete_projection() {
        let root = tempfile::tempdir().unwrap();
        let config = RootConfig {
            config: HashMap::new(),
        };
        admit_coverage(root.path(), &config).unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        assert!(admit_coverage(root.path(), &config).is_err());
        let complete = RootConfig {
            config: HashMap::from([
                ("ignore_dirs".into(), Value::Array(vec![])),
                ("ignore_vcs".into(), Value::Array(vec![])),
            ]),
        };
        admit_coverage(root.path(), &complete).unwrap();
    }
}
