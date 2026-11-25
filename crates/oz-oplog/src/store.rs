// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Append-only MVCC OpLog persistence under `.git/ozone/oplog/`.

use crate::types::OpLogEntry;
use fs4::FileExt;
use oz_core::{OzError, OzoneLayout, Result};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
static IN_PROCESS_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

pub(crate) fn process_oplog_mutex(oplog_dir: &Path) -> Arc<Mutex<()>> {
    let key = fs::canonicalize(oplog_dir).unwrap_or_else(|_| oplog_dir.to_path_buf());
    let map_mutex = IN_PROCESS_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map_mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(map.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
}

/// Returns `.git/ozone/oplog/ops/`.
#[must_use]
pub fn ops_dir(layout: &OzoneLayout) -> PathBuf {
    layout.oplog_dir().join("ops")
}

/// Returns `.git/ozone/oplog/HEAD`.
#[must_use]
pub fn head_path(layout: &OzoneLayout) -> PathBuf {
    layout.oplog_dir().join("HEAD")
}

/// Returns `.git/ozone/oplog/HEAD.lock`.
#[must_use]
pub fn head_lock_path(layout: &OzoneLayout) -> PathBuf {
    layout.oplog_dir().join("HEAD.lock")
}

/// Returns `.git/ozone/oplog/ops/<op_id>.json`.
pub fn entry_path(layout: &OzoneLayout, op_id: &str) -> Result<PathBuf> {
    validate_op_id(op_id)?;
    Ok(ops_dir(layout).join(format!("{op_id}.json")))
}

/// Validates that `op_id` is a safe single path component without traversal characters.
pub(crate) fn validate_op_id(op_id: &str) -> Result<()> {
    if op_id.is_empty()
        || op_id == "."
        || op_id == ".."
        || op_id.starts_with('.')
        || op_id.contains('/')
        || op_id.contains('\\')
        || op_id.contains('\0')
    {
        return Err(OzError::InvalidUsage {
            message: format!("invalid operation id '{op_id}'"),
            recovery: vec!["Pass a valid operation id such as 'op_01...'.".to_string()],
        });
    }
    Ok(())
}

/// Ensures `.git/ozone/oplog/` and `.git/ozone/oplog/ops/` exist on disk.
pub fn ensure_oplog_dirs(layout: &OzoneLayout) -> Result<()> {
    layout.ensure_initialized()?;
    fs::create_dir_all(ops_dir(layout))?;
    Ok(())
}

/// Atomically writes `bytes` to `dest` via a unique temp file in the same directory + rename.
pub(crate) fn atomic_write_bytes(dest: &Path, bytes: &[u8]) -> Result<()> {
    let parent = dest.parent().ok_or_else(|| {
        OzError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("target path {} has no parent directory", dest.display()),
        ))
    })?;
    fs::create_dir_all(parent)?;

    let pid = std::process::id();
    let mut attempt = 0u32;
    let (mut tmp_file, tmp_path) = loop {
        let seq = TMP_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let candidate = parent.join(format!(".tmp.{pid}.{seq}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => break (f, candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && attempt < 64 => {
                attempt += 1;
            }
            Err(err) => return Err(OzError::Io(err)),
        }
    };

    let write_res = (|| -> std::io::Result<()> {
        tmp_file.write_all(bytes)?;
        tmp_file.flush()?;
        drop(tmp_file);
        fs::rename(&tmp_path, dest)?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(OzError::Io(err));
    }

    Ok(())
}

pub(crate) struct HeadLockGuard<'a> {
    _proc_guard: std::sync::MutexGuard<'a, ()>,
    lock_file: File,
}

impl Drop for HeadLockGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
}

pub(crate) fn lock_head<'a>(
    layout: &OzoneLayout,
    proc_mutex: &'a Mutex<()>,
) -> Result<HeadLockGuard<'a>> {
    ensure_oplog_dirs(layout)?;
    let proc_guard = proc_mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(head_lock_path(layout))?;

    loop {
        match FileExt::lock(&lock_file) {
            Ok(()) => break,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(OzError::Io(err)),
        }
    }

    Ok(HeadLockGuard {
        _proc_guard: proc_guard,
        lock_file,
    })
}

/// Reads the current `oplog/HEAD` operation ID, or `None` if no operations exist yet.
pub fn read_head_op_id(layout: &OzoneLayout) -> Result<Option<String>> {
    let p = head_path(layout);
    match fs::read_to_string(&p) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                validate_op_id(trimmed)?;
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(OzError::Io(err)),
    }
}

/// Atomically updates or clears `.git/ozone/oplog/HEAD` (caller must hold `lock_head`).
pub(crate) fn write_head_op_id(layout: &OzoneLayout, op_id: Option<&str>) -> Result<()> {
    let p = head_path(layout);
    match op_id {
        Some(id) => {
            validate_op_id(id)?;
            let mut bytes = id.as_bytes().to_vec();
            bytes.push(b'\n');
            atomic_write_bytes(&p, &bytes)
        }
        None => match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(OzError::Io(err)),
        },
    }
}

/// Loads a single immutable [`OpLogEntry`] from `.git/ozone/oplog/ops/<op_id>.json`.
pub fn load_entry(layout: &OzoneLayout, op_id: &str) -> Result<OpLogEntry> {
    let p = entry_path(layout, op_id)?;
    let raw = match fs::read_to_string(&p) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(OzError::InvalidUsage {
                message: format!("operation '{op_id}' not found in oplog"),
                recovery: vec![
                    "Inspect available operations in .git/ozone/oplog/ops/.".to_string(),
                ],
            });
        }
        Err(err) => return Err(OzError::Io(err)),
    };
    let entry: OpLogEntry = serde_json::from_str(&raw)?;
    Ok(entry)
}

/// Appends an [`OpLogEntry`] atomically to `.git/ozone/oplog/ops/<op_id>.json` and advances
/// `oplog/HEAD` under micro-scoped `fs4` coordination with compare-and-merge parent linking.
///
/// Concurrent writers serialize cleanly without lock failures and never orphan entries from `HEAD`.
pub fn append_entry(layout: &OzoneLayout, mut entry: OpLogEntry) -> Result<OpLogEntry> {
    validate_op_id(&entry.op_id)?;
    ensure_oplog_dirs(layout)?;
    let proc_mutex = process_oplog_mutex(&layout.oplog_dir());
    let _lock = lock_head(layout, &proc_mutex)?;

    let current_head = read_head_op_id(layout)?;
    if let Some(ref head_id) = current_head
        && head_id != &entry.op_id
        && !entry.parents.contains(head_id)
    {
        entry.parents.push(head_id.clone());
    }

    let op_file = entry_path(layout, &entry.op_id)?;
    let json = serde_json::to_vec_pretty(&entry)?;
    atomic_write_bytes(&op_file, &json)?;

    let mut head_bytes = entry.op_id.as_bytes().to_vec();
    head_bytes.push(b'\n');
    atomic_write_bytes(&head_path(layout), &head_bytes)?;

    Ok(entry)
}

#[derive(Clone, Eq, PartialEq)]
struct HeapEntry(OpLogEntry);

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .timestamp
            .cmp(&other.0.timestamp)
            .then_with(|| self.0.op_id.cmp(&other.0.op_id))
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Lazily walks the OpLog backwards from `oplog/HEAD` in reverse causal/chronological order,
/// invoking `visitor` for each entry until `visitor` returns `Ok(false)` (stop) or all reachable
/// entries are visited.
pub(crate) fn walk_entries_rev<F>(layout: &OzoneLayout, mut visitor: F) -> Result<()>
where
    F: FnMut(&OpLogEntry) -> Result<bool>,
{
    let Some(head_id) = read_head_op_id(layout)? else {
        return Ok(());
    };
    let head_entry = load_entry(layout, &head_id)?;
    let mut visited = HashSet::new();
    visited.insert(head_entry.op_id.clone());
    let mut heap = BinaryHeap::new();
    heap.push(HeapEntry(head_entry));

    while let Some(HeapEntry(entry)) = heap.pop() {
        let should_continue = visitor(&entry)?;
        if !should_continue {
            break;
        }
        for parent_id in &entry.parents {
            if visited.insert(parent_id.clone())
                && let Ok(parent_entry) = load_entry(layout, parent_id)
            {
                heap.push(HeapEntry(parent_entry));
            }
        }
    }

    Ok(())
}

/// Lists all OpLog entries in chronological order (oldest to newest), reconstructing the causal
/// DAG from `oplog/HEAD` and including any unlinked `ops/*.json` entries if present.
pub fn list_entries(layout: &OzoneLayout) -> Result<Vec<OpLogEntry>> {
    let ops = ops_dir(layout);
    if !ops.is_dir() {
        return Ok(Vec::new());
    }

    let mut ordered_rev = Vec::new();
    let mut seen = HashSet::new();
    walk_entries_rev(layout, |entry| {
        seen.insert(entry.op_id.clone());
        ordered_rev.push(entry.clone());
        Ok(true)
    })?;
    ordered_rev.reverse();

    // Pick up any standalone op files not linked from HEAD (e.g., manual/legacy fixtures).
    let mut unlinked = Vec::new();
    for item in fs::read_dir(&ops)? {
        let item = item?;
        let file_name = item.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        if name_str.starts_with('.') || !name_str.ends_with(".json") {
            continue;
        }
        let op_id = &name_str[..name_str.len() - ".json".len()];
        if seen.contains(op_id) {
            continue;
        }
        if let Ok(entry) = load_entry(layout, op_id) {
            unlinked.push(entry);
        }
    }

    if !unlinked.is_empty() {
        unlinked.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.op_id.cmp(&b.op_id))
        });
        // Merge unlinked before/after cleanly by keeping causal HEAD entries in order and prepending unlinked.
        unlinked.extend(ordered_rev);
        return Ok(unlinked);
    }

    Ok(ordered_rev)
}
