use std::fmt;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rusqlite::{params, Connection, OpenFlags, Transaction};

const APPLICATION_ID: i32 = 0x4343544c;
const SCHEMA_VERSION: i32 = 1;

/// A failure to validate or update a clock-scoped listing index.
#[derive(Debug)]
pub enum IndexError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    UnsupportedSchema,
    StaleResponse,
    InvalidMetadata,
    InvalidPath,
}

impl fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Sqlite(error) => error.fmt(formatter),
            Self::UnsupportedSchema => {
                formatter.write_str("unsupported transcript listing index schema")
            }
            Self::StaleResponse => formatter
                .write_str("transcript listing response belongs to an older index revision"),
            Self::InvalidMetadata => formatter.write_str("invalid transcript listing metadata"),
            Self::InvalidPath => formatter.write_str("invalid relative transcript listing path"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<io::Error> for IndexError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for IndexError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Current mtime and size from an admitted metadata owner.
#[derive(Clone, Copy, Debug)]
pub struct FileMetadata {
    pub mtime: f64,
    pub size: u64,
}

/// The source from which one listing entry receives current metadata.
#[derive(Clone, Copy, Debug)]
pub enum EntryMetadata {
    Event(FileMetadata),
    Directory,
    Filesystem,
}

/// A current entry or a deletion in a filesystem change batch.
#[derive(Debug)]
pub struct EntryChange {
    pub path: PathBuf,
    pub metadata: Option<EntryMetadata>,
}

/// A complete or incremental response from one synchronized filesystem query.
pub struct ListingBatch {
    pub clock: String,
    pub fresh_instance: bool,
    pub changes: Vec<EntryChange>,
}

/// The legacy discovery filters and output limit for one listing operation.
#[derive(Clone, Copy, Debug, Default)]
pub struct ListingFilter<'a> {
    pub project: Option<&'a str>,
    pub contains: Option<&'a str>,
    pub limit: Option<usize>,
}

/// The index revision and opaque clock to which a request is bound.
#[derive(Clone, Debug)]
pub struct ListingRequest {
    owner: Arc<()>,
    revision: i64,
    binding: Vec<u8>,
    clock: Option<String>,
}

impl ListingRequest {
    /// The opaque service clock to send unchanged in a delta query.
    pub fn clock(&self) -> Option<&str> {
        self.clock.as_deref()
    }
}

/// A selected path with the same mtime and size representation as discovery.
#[derive(Debug, PartialEq)]
pub struct ListingRow {
    pub path: PathBuf,
    pub mtime: f64,
    pub size: u64,
}

/// A limited selection and the complete matching population size.
#[derive(Debug)]
pub struct ListingSelection {
    pub total: usize,
    pub rows: Vec<ListingRow>,
}

/// A serialized, durable metadata projection for one filesystem binding.
pub struct ListingIndex {
    connection: Connection,
    _writer: File,
    owner: Arc<()>,
}

impl ListingIndex {
    /// Open a metadata index while holding its stable writer lock.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let path = std::path::absolute(path)?;
        let parent = path.parent().ok_or(IndexError::InvalidPath)?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let path = parent
            .canonicalize()?
            .join(path.file_name().ok_or(IndexError::InvalidPath)?);
        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.with_extension("lock"))?;
        if unsafe { libc::flock(writer.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let application: i32 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if application == 0 && version == 0 {
            let tables: i64 = connection.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [], |row| row.get(0))?;
            if tables != 0 {
                return Err(IndexError::UnsupportedSchema);
            }
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "CREATE TABLE listing_state (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    revision INTEGER NOT NULL, binding BLOB NOT NULL,
                    clock TEXT, total INTEGER NOT NULL CHECK(total>=0));
                 INSERT INTO listing_state VALUES(1,0,x'',NULL,0);
                 CREATE TABLE listing_files (
                    path BLOB PRIMARY KEY, path_order BLOB NOT NULL,
                    filename TEXT NOT NULL, project TEXT,
                    mtime REAL NOT NULL, size BLOB NOT NULL CHECK(length(size)=8));
                 CREATE INDEX listing_rank ON listing_files(mtime DESC,path_order ASC);
                 CREATE TABLE listing_refresh (
                    path BLOB PRIMARY KEY, path_order BLOB NOT NULL,
                    filename TEXT NOT NULL, project TEXT);",
            )?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            transaction.commit()?;
        } else if application != APPLICATION_ID || version != SCHEMA_VERSION {
            return Err(IndexError::UnsupportedSchema);
        }
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Ok(Self {
            connection,
            _writer: writer,
            owner: Arc::new(()),
        })
    }

    /// Bind a forthcoming query to the current revision and source identity.
    pub fn request(&self, binding: &[u8]) -> Result<ListingRequest, IndexError> {
        let (revision, stored, clock): (i64, Vec<u8>, Option<String>) = self.connection.query_row(
            "SELECT revision,binding,clock FROM listing_state WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(ListingRequest {
            owner: Arc::clone(&self.owner),
            revision,
            binding: binding.to_vec(),
            clock: if stored == binding { clock } else { None },
        })
    }

    /// Refresh matching filesystem metadata and select from the same clock response.
    pub fn refresh_and_select<F, G>(
        &mut self,
        request: &ListingRequest,
        batch: ListingBatch,
        filter: ListingFilter<'_>,
        mut follow_metadata: F,
        mut follow_size: G,
    ) -> Result<ListingSelection, IndexError>
    where
        F: FnMut(&Path) -> io::Result<FileMetadata>,
        G: FnMut(&Path) -> io::Result<u64>,
    {
        if !Arc::ptr_eq(&request.owner, &self.owner) {
            return Err(IndexError::StaleResponse);
        }
        let transaction = self.connection.transaction()?;
        let (revision, binding, stored_clock, mut total): (i64, Vec<u8>, Option<String>, i64) =
            transaction.query_row(
                "SELECT revision,binding,clock,total FROM listing_state WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let expected_clock = if binding == request.binding {
            stored_clock
        } else {
            None
        };
        if revision != request.revision || expected_clock != request.clock {
            return Err(IndexError::StaleResponse);
        }
        if request.clock.is_none() || batch.fresh_instance {
            transaction.execute("DELETE FROM listing_files", [])?;
            transaction.execute("DELETE FROM listing_refresh", [])?;
            total = 0;
        }
        for change in batch.changes {
            let key = path_order(&change.path)?;
            let bytes = change.path.as_os_str().as_bytes();
            let filename = change.path.file_name().and_then(|name| name.to_str());
            let wanted = filename.is_some_and(|name| name.ends_with(".jsonl"));
            match change.metadata.filter(|_| wanted) {
                Some(EntryMetadata::Filesystem) => {
                    transaction.execute(
                        "INSERT OR IGNORE INTO listing_refresh VALUES(?1,?2,?3,?4)",
                        params![
                            bytes,
                            key,
                            filename.expect("matched filename"),
                            project_name(&change.path)
                        ],
                    )?;
                }
                Some(EntryMetadata::Event(metadata)) => {
                    transaction.execute("DELETE FROM listing_refresh WHERE path=?1", [bytes])?;
                    store_file(&transaction, &change.path, metadata, &mut total)?;
                }
                None | Some(EntryMetadata::Directory) => {
                    transaction.execute("DELETE FROM listing_refresh WHERE path=?1", [bytes])?;
                    remove_file(&transaction, bytes, &mut total)?;
                }
            }
        }
        let mut observed = Vec::new();
        {
            let mut statement = transaction.prepare(
                "SELECT path FROM listing_refresh WHERE
                 (?1 IS NULL OR (project IS NOT NULL AND instr(project,?1)>0)) AND
                 (?2 IS NULL OR instr(filename,?2)>0) ORDER BY path_order",
            )?;
            let paths = statement.query_map(params![filter.project, filter.contains], |row| {
                row.get::<_, Vec<u8>>(0)
            })?;
            for path in paths {
                let path = PathBuf::from(std::ffi::OsString::from_vec(path?));
                match follow_metadata(&path) {
                    Ok(metadata) => {
                        if !metadata.mtime.is_finite() {
                            return Err(IndexError::InvalidMetadata);
                        }
                        observed.push((path, metadata));
                    }
                    Err(_) => remove_file(&transaction, path.as_os_str().as_bytes(), &mut total)?,
                }
            }
        }
        observed.sort_by(|left, right| {
            right
                .1
                .mtime
                .partial_cmp(&left.1.mtime)
                .expect("finite mtimes")
                .then_with(|| {
                    left.0
                        .as_os_str()
                        .as_encoded_bytes()
                        .cmp(right.0.as_os_str().as_encoded_bytes())
                })
        });
        for (path, mut metadata) in observed {
            metadata.size = follow_size(&path)?;
            store_file(&transaction, &path, metadata, &mut total)?;
        }
        transaction.execute(
            "UPDATE listing_state SET revision=?1,binding=?2,clock=?3,total=?4 WHERE singleton=1",
            params![
                revision.checked_add(1).ok_or(IndexError::InvalidMetadata)?,
                request.binding,
                batch.clock,
                total
            ],
        )?;
        transaction.commit()?;
        self.select(filter.project, filter.contains, filter.limit)
    }

    fn select(
        &self,
        project: Option<&str>,
        contains: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ListingSelection, IndexError> {
        let total: i64 = if project.is_none() && contains.is_none() {
            self.connection.query_row(
                "SELECT total FROM listing_state WHERE singleton=1",
                [],
                |row| row.get(0),
            )?
        } else {
            self.connection.query_row(
                "SELECT count(*) FROM listing_files WHERE
                 (?1 IS NULL OR (project IS NOT NULL AND instr(project,?1)>0)) AND
                 (?2 IS NULL OR instr(filename,?2)>0)",
                params![project, contains],
                |row| row.get(0),
            )?
        };
        let limit = limit
            .and_then(|limit| i64::try_from(limit).ok())
            .unwrap_or(-1);
        let mut statement = self.connection.prepare(
            "SELECT path,mtime,size FROM listing_files WHERE
             (?1 IS NULL OR (project IS NOT NULL AND instr(project,?1)>0)) AND
             (?2 IS NULL OR instr(filename,?2)>0)
             ORDER BY mtime DESC,path_order ASC LIMIT ?3",
        )?;
        let mut result = statement.query(params![project, contains, limit])?;
        let mut rows = Vec::new();
        while let Some(row) = result.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            let size: Vec<u8> = row.get(2)?;
            rows.push(ListingRow {
                path: PathBuf::from(std::ffi::OsString::from_vec(bytes)),
                mtime: row.get(1)?,
                size: u64::from_be_bytes(size.try_into().map_err(|_| IndexError::InvalidMetadata)?),
            });
        }
        Ok(ListingSelection {
            total: usize::try_from(total).map_err(|_| IndexError::InvalidMetadata)?,
            rows,
        })
    }
}

fn path_order(path: &Path) -> Result<Vec<u8>, IndexError> {
    let mut key = Vec::with_capacity(path.as_os_str().len() + 1);
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(IndexError::InvalidPath);
        };
        if name.as_bytes().contains(&0) {
            return Err(IndexError::InvalidPath);
        }
        key.extend_from_slice(name.as_bytes());
        key.push(0);
    }
    if key.is_empty() {
        return Err(IndexError::InvalidPath);
    }
    Ok(key)
}

fn project_name(path: &Path) -> Option<String> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())?;
    Some(
        path.components()
            .next()?
            .as_os_str()
            .to_string_lossy()
            .into_owned(),
    )
}

fn remove_file(
    transaction: &Transaction<'_>,
    path: &[u8],
    total: &mut i64,
) -> Result<(), IndexError> {
    *total -= transaction.execute("DELETE FROM listing_files WHERE path=?1", [path])? as i64;
    Ok(())
}

fn store_file(
    transaction: &Transaction<'_>,
    path: &Path,
    metadata: FileMetadata,
    total: &mut i64,
) -> Result<(), IndexError> {
    if !metadata.mtime.is_finite() {
        return Err(IndexError::InvalidMetadata);
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(IndexError::InvalidPath)?;
    let size = metadata.size.to_be_bytes();
    *total += transaction.execute(
        "INSERT OR IGNORE INTO listing_files VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            path.as_os_str().as_bytes(),
            path_order(path)?,
            filename,
            project_name(path),
            metadata.mtime,
            size.as_slice()
        ],
    )? as i64;
    transaction.execute(
        "UPDATE listing_files SET mtime=?2,size=?3 WHERE path=?1 AND (mtime IS NOT ?2 OR size IS NOT ?3)",
        params![path.as_os_str().as_bytes(), metadata.mtime, size.as_slice()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, ListingIndex) {
        let directory = tempfile::tempdir().unwrap();
        let index = ListingIndex::open(&directory.path().join("listing.sqlite3")).unwrap();
        (directory, index)
    }

    #[test]
    fn an_aliased_parent_opens_the_same_index() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let index = ListingIndex::open(&alias.join("listing.sqlite3")).unwrap();
        assert!(index.request(b"alias").unwrap().clock().is_none());
        drop(index);
        assert!(real.join("listing.sqlite3").is_file());
        let index = ListingIndex::open(&real.join("listing.sqlite3")).unwrap();
        assert!(index.request(b"alias").unwrap().clock().is_none());
    }

    #[test]
    fn a_database_leaf_symlink_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().canonicalize().unwrap();
        let target = parent.join("target.sqlite3");
        drop(ListingIndex::open(&target).unwrap());
        let link = parent.join("link.sqlite3");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            ListingIndex::open(&link),
            Err(IndexError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
                if error.extended_code == rusqlite::ffi::SQLITE_CANTOPEN_SYMLINK
        ));
    }

    #[test]
    fn a_writer_leaf_symlink_is_refused_before_opening_the_database() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().canonicalize().unwrap();
        let target = parent.join("target.lock");
        File::create(&target).unwrap();
        let database = parent.join("listing.sqlite3");
        std::os::unix::fs::symlink(&target, database.with_extension("lock")).unwrap();
        assert!(matches!(
            ListingIndex::open(&database),
            Err(IndexError::Io(error)) if error.raw_os_error() == Some(libc::ELOOP)
        ));
        assert!(!database.exists());
    }

    fn file(path: impl Into<PathBuf>, mtime: f64) -> EntryChange {
        EntryChange {
            path: path.into(),
            metadata: Some(EntryMetadata::Event(FileMetadata { mtime, size: 17 })),
        }
    }

    fn no_links(_: &Path) -> io::Result<FileMetadata> {
        panic!("regular projections must not request filesystem metadata")
    }

    fn no_link_sizes(_: &Path) -> io::Result<u64> {
        panic!("regular projections must not request filesystem sizes")
    }

    fn apply<F, G>(
        index: &mut ListingIndex,
        request: &ListingRequest,
        clock: &str,
        fresh_instance: bool,
        changes: impl IntoIterator<Item = EntryChange>,
        follow_metadata: F,
        follow_size: G,
    ) -> Result<(), IndexError>
    where
        F: FnMut(&Path) -> io::Result<FileMetadata>,
        G: FnMut(&Path) -> io::Result<u64>,
    {
        index
            .refresh_and_select(
                request,
                ListingBatch {
                    clock: clock.into(),
                    fresh_instance,
                    changes: changes.into_iter().collect(),
                },
                ListingFilter::default(),
                follow_metadata,
                follow_size,
            )
            .map(|_| ())
    }

    fn measured_turn(index: &mut ListingIndex) -> (ListingSelection, usize) {
        let steps = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&steps);
        index.connection.progress_handler(
            1,
            Some(move || {
                count.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let request = index.request(b"root-a").unwrap();
        let result = index
            .refresh_and_select(
                &request,
                ListingBatch {
                    clock: "c:1:1:2".into(),
                    fresh_instance: false,
                    changes: vec![],
                },
                ListingFilter {
                    limit: Some(25),
                    ..Default::default()
                },
                no_links,
                no_link_sizes,
            )
            .unwrap();
        index.connection.progress_handler(0, None::<fn() -> bool>);
        (result, steps.load(Ordering::Relaxed))
    }

    #[test]
    fn warm_top25_work_does_not_scan_the_regular_population() {
        let mut measured = Vec::new();
        for population in [32, 10_000] {
            let (_directory, mut index) = fixture();
            let request = index.request(b"root-a").unwrap();
            let mut expected = Vec::new();
            let changes = (0..population).map(|position| {
                let path = PathBuf::from(format!(
                    "project-{group:03}/session-{position:05}.jsonl",
                    group = position / 100
                ));
                let mtime = (position / 3) as f64;
                expected.push(ListingRow {
                    path: path.clone(),
                    mtime,
                    size: 17,
                });
                file(path, mtime)
            });
            apply(
                &mut index,
                &request,
                "c:1:1:1",
                false,
                changes,
                no_links,
                no_link_sizes,
            )
            .unwrap();
            expected.sort_by(|left, right| {
                right
                    .mtime
                    .partial_cmp(&left.mtime)
                    .unwrap()
                    .then_with(|| left.path.cmp(&right.path))
            });
            expected.truncate(25);
            let (selected, steps) = measured_turn(&mut index);
            assert_eq!(selected.total, population);
            assert_eq!(selected.rows, expected);
            println!("LIST_INDEX_PRIVATE_POPULATION={population} INDEX_TURN_VM_STEPS={steps}");
            measured.push(steps);
        }
        assert!(
            measured[1] <= measured[0] + 64,
            "top25 must not scan all regular rows: {measured:?}"
        );
    }

    #[test]
    fn changed_old_entry_new_entry_and_unselected_delete_update_rank_and_count() {
        let (_directory, mut index) = fixture();
        let request = index.request(b"root-a").unwrap();
        apply(
            &mut index,
            &request,
            "c:1:1:1",
            false,
            (0..32).map(|position| {
                file(
                    format!("project/session-{position:02}.jsonl"),
                    position as f64,
                )
            }),
            no_links,
            no_link_sizes,
        )
        .unwrap();
        let request = index.request(b"root-a").unwrap();
        apply(
            &mut index,
            &request,
            "c:1:1:2",
            false,
            [
                file("project/session-00.jsonl", 100.0),
                file("new-project/new.jsonl", 101.0),
                EntryChange {
                    path: "project/session-01.jsonl".into(),
                    metadata: None,
                },
            ],
            no_links,
            no_link_sizes,
        )
        .unwrap();
        let selected = index.select(None, None, Some(25)).unwrap();
        assert_eq!(selected.total, 32);
        assert_eq!(selected.rows.len(), 25);
        assert_eq!(selected.rows[0].path, Path::new("new-project/new.jsonl"));
        assert_eq!(selected.rows[1].path, Path::new("project/session-00.jsonl"));
    }

    #[test]
    fn replay_refuses_without_mutation_and_fresh_instance_replaces_the_domain() {
        let (_directory, mut index) = fixture();
        let old_request = index.request(b"root-a").unwrap();
        apply(
            &mut index,
            &old_request,
            "c:1:1:1",
            false,
            [file("old.jsonl", 1.0)],
            no_links,
            no_link_sizes,
        )
        .unwrap();
        assert!(matches!(
            apply(
                &mut index,
                &old_request,
                "c:1:1:1",
                false,
                [file("replayed.jsonl", 2.0)],
                no_links,
                no_link_sizes
            ),
            Err(IndexError::StaleResponse)
        ));
        assert_eq!(
            index.select(None, None, None).unwrap().rows[0].path,
            Path::new("old.jsonl")
        );
        let request = index.request(b"root-a").unwrap();
        apply(
            &mut index,
            &request,
            "c:2:1:1",
            true,
            [file("current.jsonl", 3.0)],
            no_links,
            no_link_sizes,
        )
        .unwrap();
        let selected = index.select(None, None, None).unwrap();
        assert_eq!(selected.total, 1);
        assert_eq!(selected.rows[0].path, Path::new("current.jsonl"));
        assert_eq!(index.request(b"root-a").unwrap().clock(), Some("c:2:1:1"));
    }

    #[test]
    fn path_component_ties_filters_default_zero_and_uncapped_selection() {
        let (_directory, mut index) = fixture();
        let request = index.request(b"root-a").unwrap();
        apply(
            &mut index,
            &request,
            "c:1:1:1",
            false,
            [
                file("a.b/z.jsonl", 2.0),
                file("a/z.jsonl", 2.0),
                file("root.jsonl", 2.0),
                file("a/older.jsonl", 1.0),
            ],
            no_links,
            no_link_sizes,
        )
        .unwrap();
        let selected = index.select(None, None, None).unwrap();
        assert_eq!(selected.total, 4);
        assert_eq!(
            selected
                .rows
                .iter()
                .map(|row| row.path.as_path())
                .collect::<Vec<_>>(),
            [
                Path::new("a/z.jsonl"),
                Path::new("a.b/z.jsonl"),
                Path::new("root.jsonl"),
                Path::new("a/older.jsonl")
            ]
        );
        let zero = index.select(None, None, Some(0)).unwrap();
        assert_eq!(zero.total, 4);
        assert!(zero.rows.is_empty());
        assert_eq!(
            index.select(None, None, Some(usize::MAX)).unwrap().rows,
            selected.rows
        );
        assert_eq!(index.select(Some(""), None, None).unwrap().total, 3);
        assert_eq!(index.select(Some("a.b"), Some("z"), None).unwrap().total, 1);
        assert_eq!(index.select(Some("A"), None, None).unwrap().total, 0);
        assert_eq!(
            index.select(None, Some("older"), None).unwrap().rows[0].path,
            Path::new("a/older.jsonl")
        );
    }

    #[test]
    fn unselected_and_initially_dangling_links_are_refreshed_without_root_deltas() {
        let (_directory, mut index) = fixture();
        let request = index.request(b"root-a").unwrap();
        let link = EntryChange {
            path: "old-link.jsonl".into(),
            metadata: Some(EntryMetadata::Filesystem),
        };
        apply(
            &mut index,
            &request,
            "c:1:1:1",
            false,
            [file("regular.jsonl", 50.0), link],
            |_| Err(io::ErrorKind::NotFound.into()),
            no_link_sizes,
        )
        .unwrap();
        assert_eq!(index.select(None, None, Some(1)).unwrap().total, 1);
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        for (clock, mtime, expected) in [
            ("c:1:1:2", 1.0, "regular.jsonl"),
            ("c:1:1:3", 100.0, "old-link.jsonl"),
        ] {
            let request = index.request(b"root-a").unwrap();
            apply(
                &mut index,
                &request,
                clock,
                false,
                [],
                |_| {
                    first.fetch_add(1, Ordering::Relaxed);
                    Ok(FileMetadata { mtime, size: 7 })
                },
                |_| {
                    second.fetch_add(1, Ordering::Relaxed);
                    Ok(8)
                },
            )
            .unwrap();
            let selected = index.select(None, None, Some(1)).unwrap();
            assert_eq!(selected.total, 2);
            assert_eq!(selected.rows[0].path, Path::new(expected));
        }
        assert_eq!(first.load(Ordering::Relaxed), 2);
        assert_eq!(second.load(Ordering::Relaxed), 2);
        let before = index.select(None, None, None).unwrap().rows;
        let request = index.request(b"root-a").unwrap();
        assert!(matches!(
            apply(
                &mut index,
                &request,
                "c:1:1:4",
                false,
                [file("not-committed.jsonl", 200.0)],
                |_| Ok(FileMetadata {
                    mtime: 101.0,
                    size: 9
                }),
                |_| Err(io::ErrorKind::PermissionDenied.into())
            ),
            Err(IndexError::Io(_))
        ));
        assert_eq!(index.select(None, None, None).unwrap().rows, before);
        assert_eq!(index.request(b"root-a").unwrap().clock(), Some("c:1:1:3"));
    }

    #[test]
    fn link_metadata_errors_follow_the_requested_filter_before_selection() {
        let (_directory, mut index) = fixture();
        let request = index.request(b"root-a").unwrap();
        let link = EntryChange {
            path: "other/broken.jsonl".into(),
            metadata: Some(EntryMetadata::Filesystem),
        };
        let selected = index
            .refresh_and_select(
                &request,
                ListingBatch {
                    clock: "c:1:1:1".into(),
                    fresh_instance: false,
                    changes: vec![file("wanted/good.jsonl", 10.0), link],
                },
                ListingFilter {
                    project: Some("wanted"),
                    contains: None,
                    limit: Some(25),
                },
                no_links,
                no_link_sizes,
            )
            .unwrap();
        assert_eq!(selected.total, 1);
        assert_eq!(selected.rows[0].path, Path::new("wanted/good.jsonl"));
        let request = index.request(b"root-a").unwrap();
        let selected = index
            .refresh_and_select(
                &request,
                ListingBatch {
                    clock: "c:1:1:2".into(),
                    fresh_instance: false,
                    changes: vec![],
                },
                ListingFilter {
                    project: None,
                    contains: Some("good"),
                    limit: Some(0),
                },
                no_links,
                no_link_sizes,
            )
            .unwrap();
        assert_eq!(selected.total, 1);
        assert!(selected.rows.is_empty());
        let request = index.request(b"root-a").unwrap();
        assert!(matches!(
            index.refresh_and_select(
                &request,
                ListingBatch {
                    clock: "c:1:1:3".into(),
                    fresh_instance: false,
                    changes: vec![],
                },
                ListingFilter::default(),
                |_| Ok(FileMetadata {
                    mtime: 20.0,
                    size: 0,
                }),
                |_| Err(io::ErrorKind::PermissionDenied.into())
            ),
            Err(IndexError::Io(_))
        ));
        assert_eq!(index.request(b"root-a").unwrap().clock(), Some("c:1:1:2"));
    }

    #[test]
    fn reopen_scope_change_and_eviction_do_not_reuse_an_unrelated_cursor() {
        use std::os::unix::fs::MetadataExt;
        let (directory, mut index) = fixture();
        let path = directory.path().join("listing.sqlite3");
        let lock_inode = std::fs::metadata(path.with_extension("lock"))
            .unwrap()
            .ino();
        let request = index.request(b"root-a").unwrap();
        apply(
            &mut index,
            &request,
            "c:1:1:1",
            false,
            [file("old.jsonl", 1.0)],
            no_links,
            no_link_sizes,
        )
        .unwrap();
        drop(index);
        let mut index = ListingIndex::open(&path).unwrap();
        assert_eq!(index.request(b"root-a").unwrap().clock(), Some("c:1:1:1"));
        let request = index.request(b"root-b").unwrap();
        assert_eq!(request.clock(), None);
        apply(
            &mut index,
            &request,
            "c:2:1:1",
            false,
            [file("new.jsonl", 2.0)],
            no_links,
            no_link_sizes,
        )
        .unwrap();
        assert_eq!(
            index.select(None, None, None).unwrap().rows[0].path,
            Path::new("new.jsonl")
        );
        assert_eq!(index.select(None, None, None).unwrap().total, 1);
        drop(index);
        std::fs::remove_file(&path).unwrap();
        let mut index = ListingIndex::open(&path).unwrap();
        assert_eq!(index.request(b"root-b").unwrap().clock(), None);
        assert!(matches!(
            apply(
                &mut index,
                &request,
                "c:2:1:2",
                false,
                [file("stale.jsonl", 9.0)],
                no_links,
                no_link_sizes
            ),
            Err(IndexError::StaleResponse)
        ));
        assert_eq!(
            std::fs::metadata(path.with_extension("lock"))
                .unwrap()
                .ino(),
            lock_inode
        );
    }

    #[test]
    fn an_empty_evicted_database_cannot_accept_an_old_revision_zero_request() {
        let (directory, index) = fixture();
        let request = index.request(b"root-a").unwrap();
        let path = directory.path().join("listing.sqlite3");
        drop(index);
        std::fs::remove_file(&path).unwrap();
        let mut index = ListingIndex::open(&path).unwrap();
        assert!(matches!(
            apply(
                &mut index,
                &request,
                "c:1:1:1",
                false,
                [file("stale.jsonl", 1.0)],
                no_links,
                no_link_sizes
            ),
            Err(IndexError::StaleResponse)
        ));
        assert_eq!(index.select(None, None, None).unwrap().total, 0);
        assert_eq!(index.request(b"root-a").unwrap().clock(), None);
    }
}
