use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn open(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    {
        let mut lock = fd_lock::RwLock::new(&file);
        let _guard = lock.write()?;
        repair_tail(&file)?;
    }
    Ok(file)
}

/// Append one record to a readable, append-mode file (as returned by `open`).
/// Failed writes are rolled back; an unrepaired tail blocks subsequent writes.
pub fn append<T: Serialize>(file: &mut File, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    append_with(file, &line, |file, line| file.write_all(line))
}

// All repair and append operations share the same exclusive inode lock. The
// caller must also serialize access to clones of the same file description.
fn append_with(
    file: &mut File,
    line: &[u8],
    write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut lock = fd_lock::RwLock::new(file);
    let mut file = lock.write()?;
    // A failed write can leave a prefix even while the daemon stays alive.
    // Repair before EVERY append, not just when opening after a restart.
    repair_tail(&file)?;
    let start = file.metadata()?.len();
    if let Err(error) = write(&mut file, line) {
        // Roll back immediately. If rollback fails, a subsequent append must
        // successfully repair the tail before it is allowed to write anything.
        if let Err(rollback) = file.set_len(start).and_then(|()| file.sync_data()) {
            return Err(io::Error::new(
                error.kind(),
                format!("JSONL append failed: {error}; rollback failed: {rollback}"),
            ));
        }
        return Err(error);
    }
    Ok(())
}

pub fn replay<T: DeserializeOwned>(path: &Path, mut apply: impl FnMut(T)) -> io::Result<()> {
    let lock = fd_lock::RwLock::new(File::open(path)?);
    let file = lock.read()?;
    let mut lines = BufReader::new(&*file).lines().peekable();
    let mut number = 0;
    while let Some(line) = lines.next() {
        number += 1;
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(value) => apply(value),
            Err(_) if lines.peek().is_none() => break,
            Err(error) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("malformed JSONL record {number}: {error}"),
                ));
            }
        }
    }
    Ok(())
}

pub fn repair(path: &Path) -> io::Result<()> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut lock = fd_lock::RwLock::new(&file);
    let _guard = lock.write()?;
    repair_tail(&file)
}

/// Inspect only the unterminated suffix, keeping ordinary appends O(1) in the
/// journal's size. Newline-terminated corruption is reported by strict replay.
fn repair_tail(file: &File) -> io::Result<()> {
    let end = file.metadata()?.len();
    if end == 0 {
        return Ok(());
    }
    let mut last = [0];
    file.read_exact_at(&mut last, end - 1)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut start = end;
    let mut chunks = Vec::new();
    loop {
        let offset = start.saturating_sub(8192);
        let mut chunk = vec![0; (start - offset) as usize];
        file.read_exact_at(&mut chunk, offset)?;
        if let Some(index) = chunk.iter().rposition(|byte| *byte == b'\n') {
            start = offset + index as u64 + 1;
            chunks.push(chunk[index + 1..].to_vec());
            break;
        }
        chunks.push(chunk);
        start = offset;
        if start == 0 {
            break;
        }
    }
    let tail: Vec<u8> = chunks.into_iter().rev().flatten().collect();
    if serde_json::from_slice::<serde_json::Value>(&tail).is_ok() {
        // Use a positional write: repair() can open a non-O_APPEND descriptor.
        file.write_all_at(b"\n", end)?;
    } else {
        file.set_len(start)?;
    }
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Event {
        value: u8,
    }

    #[test]
    fn failed_partial_append_is_rolled_back_before_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut file = open(&path).unwrap();
        append(&mut file, &Event { value: 1 }).unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = append_with(&mut file, b"{\"value\":2}\n", |file, line| {
            file.write_all(&line[..5])?;
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        append(&mut file, &Event { value: 3 }).unwrap();
        let mut events = Vec::new();
        replay(&path, |event: Event| events.push(event)).unwrap();
        assert_eq!(events, vec![Event { value: 1 }, Event { value: 3 }]);
    }

    #[test]
    fn failed_tail_repair_prevents_another_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut file = open(&path).unwrap();
        append(&mut file, &Event { value: 1 }).unwrap();
        let mut read_only = File::open(&path).unwrap();
        // Make rollback impossible on this descriptor after injecting a partial
        // write through a separate handle. Subsequent writes must fail closed.
        let error = append_with(&mut read_only, b"unused", |_, _| {
            file.write_all(b"{\"value\":")?;
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        })
        .unwrap_err();
        assert!(error.to_string().contains("rollback failed"));
        let partial = std::fs::read(&path).unwrap();
        append_with(&mut read_only, b"must not be written", |_, _| {
            panic!("a new write must not run when tail repair fails");
        })
        .unwrap_err();
        assert_eq!(std::fs::read(&path).unwrap(), partial);
        append(&mut file, &Event { value: 2 }).unwrap();
        let mut events = Vec::new();
        replay(&path, |event: Event| events.push(event)).unwrap();
        assert_eq!(events, vec![Event { value: 1 }, Event { value: 2 }]);
    }

    #[test]
    fn append_repairs_partial_tail_without_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut file = open(&path).unwrap();
        append(&mut file, &Event { value: 1 }).unwrap();
        // A failed rollback or a killed writer may leave a prefix. The next
        // append on this still-open handle must not splice its JSON onto it.
        file.write_all(b"{\"value\":").unwrap();
        append(&mut file, &Event { value: 2 }).unwrap();
        let mut events = Vec::new();
        replay(&path, |event: Event| events.push(event)).unwrap();
        assert_eq!(events, vec![Event { value: 1 }, Event { value: 2 }]);
    }

    #[test]
    fn append_terminates_complete_tail_without_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut file = open(&path).unwrap();
        file.write_all(b"{\"value\":1}").unwrap();
        append(&mut file, &Event { value: 2 }).unwrap();
        let mut events = Vec::new();
        replay(&path, |event: Event| events.push(event)).unwrap();
        assert_eq!(events, vec![Event { value: 1 }, Event { value: 2 }]);
    }

    #[test]
    fn independent_writers_cannot_interleave_partial_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let threads: Vec<_> = (0..4)
            .map(|worker| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut file = open(&path).unwrap();
                    barrier.wait();
                    for index in 0..25 {
                        let line = format!("{{\"value\":{}}}\n", worker * 25 + index);
                        append_with(&mut file, line.as_bytes(), |file, line| {
                            file.write_all(&line[..5])?;
                            std::thread::yield_now();
                            file.write_all(&line[5..])
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let mut events = Vec::new();
        replay(&path, |event: Event| events.push(event.value)).unwrap();
        events.sort_unstable();
        assert_eq!(events, (0..100).collect::<Vec<u8>>());
    }

    #[test]
    fn repair_handles_tail_larger_than_read_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut file = open(&path).unwrap();
        append(&mut file, &Event { value: 1 }).unwrap();
        file.write_all(&vec![b'x'; 20000]).unwrap();
        append(&mut file, &Event { value: 2 }).unwrap();
        let mut events = Vec::new();
        replay(&path, |event: Event| events.push(event)).unwrap();
        assert_eq!(events, vec![Event { value: 1 }, Event { value: 2 }]);
    }

    #[test]
    fn append_replay_and_repair_partial_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut file = open(&path).unwrap();
        append(&mut file, &Event { value: 1 }).unwrap();
        file.sync_data().unwrap();
        file.write_all(b"{\"value\":").unwrap();
        drop(file);

        let mut file = open(&path).unwrap();
        append(&mut file, &Event { value: 2 }).unwrap();
        file.sync_data().unwrap();
        let mut events = Vec::new();
        replay(&path, |event: Event| events.push(event)).unwrap();
        assert_eq!(events, vec![Event { value: 1 }, Event { value: 2 }]);
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}
