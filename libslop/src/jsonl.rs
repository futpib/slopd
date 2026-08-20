use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn open(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    repair(path)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

pub fn append<T: Serialize>(file: &mut File, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    file.write_all(&line)
}

pub fn replay<T: DeserializeOwned>(path: &Path, mut apply: impl FnMut(T)) -> io::Result<()> {
    let file = File::open(path)?;
    let mut lines = BufReader::new(file).lines().peekable();
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
    let bytes = match std::fs::read(path) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    if end == 0 {
        return Ok(());
    }
    let start = bytes[..end]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if serde_json::from_slice::<serde_json::Value>(&bytes[start..end]).is_ok() {
        if bytes.last() != Some(&b'\n') {
            OpenOptions::new()
                .append(true)
                .open(path)?
                .write_all(b"\n")?;
        }
    } else {
        OpenOptions::new()
            .write(true)
            .open(path)?
            .set_len(start as u64)?;
    }
    Ok(())
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
