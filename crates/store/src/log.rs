//! Append-only event log: `events/YYYY-MM-DD.jsonl`, one JSON `Event` per line.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use shimmer_core::{Event, Result};

use crate::atomic::fsync_dir;

pub struct EventScan {
    pub events: Vec<Event>,
    /// Lines that did not parse: typically one torn final line after a crash.
    pub skipped: usize,
}

fn day_file(home: &Path, event: &Event) -> PathBuf {
    home.join("events").join(format!("{}.jsonl", event.at.format("%Y-%m-%d")))
}

pub fn append(home: &Path, event: &Event) -> Result<()> {
    let path = day_file(home, event);
    let mut line =
        serde_json::to_vec(event).map_err(|e| shimmer_core::Error::internal(format!("encode event: {e}")))?;
    line.push(b'\n');

    let existed = path.exists();
    let mut f = OpenOptions::new().create(true).append(true).read(true).open(&path)?;
    // A crash mid-append can leave a line with no newline; don't glue the next event to it.
    if existed && !ends_with_newline(&mut f)? {
        line.insert(0, b'\n');
    }
    f.write_all(&line)?;
    f.sync_data()?;
    if !existed {
        fsync_dir(path.parent().unwrap_or(home))?;
    }
    Ok(())
}

fn ends_with_newline(f: &mut File) -> io::Result<bool> {
    if f.metadata()?.len() == 0 {
        return Ok(true);
    }
    f.seek(SeekFrom::End(-1))?;
    let mut b = [0u8; 1];
    f.read_exact(&mut b)?;
    Ok(b[0] == b'\n')
}

/// The ids already in each day's file, each file read once: what recovery checks against
/// before appending an event a crash may have logged already. A normal commit never needs it:
/// it has just staged its events, so they can't be in the log yet (#260 finding 1).
#[derive(Default)]
pub struct Logged {
    by_day: HashMap<PathBuf, HashSet<ulid::Ulid>>,
}

impl Logged {
    /// Append `event` unless its id is already in its day's file.
    pub fn append_once(&mut self, home: &Path, event: &Event) -> Result<()> {
        let file = day_file(home, event);
        if !self.by_day.contains_key(&file) {
            let ids = ids_in(&file)?;
            self.by_day.insert(file.clone(), ids);
        }
        let ids = self.by_day.get_mut(&file).expect("just read");
        if !ids.contains(&event.id) {
            append(home, event)?;
            ids.insert(event.id);
        }
        Ok(())
    }
}

fn ids_in(file: &Path) -> Result<HashSet<ulid::Ulid>> {
    let text = match fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(text.lines().filter_map(|l| serde_json::from_str::<Event>(l).ok()).map(|e| e.id).collect())
}

pub fn read_all(home: &Path) -> Result<EventScan> {
    read_from(home, None)
}

/// Events logged at or after `since`, oldest first: only the day files from `since`'s day on
/// are read (a day file holds the events whose `at` falls on it). For a module that fell
/// behind the bus, catching up from where it was (#261 finding 1).
pub fn read_since(home: &Path, since: chrono::DateTime<chrono::Utc>) -> Result<EventScan> {
    let mut scan = read_from(home, Some(&format!("{}.jsonl", since.format("%Y-%m-%d"))))?;
    scan.events.retain(|e| e.at >= since);
    Ok(scan)
}

/// `first`: the earliest day file to read, by name (they sort by date).
fn read_from(home: &Path, first: Option<&str>) -> Result<EventScan> {
    let mut files: Vec<PathBuf> = fs::read_dir(home.join("events"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter(|p| first.is_none_or(|first| p.file_name().is_some_and(|n| n.to_string_lossy().as_ref() >= first)))
        .collect();
    files.sort();
    let mut scan = EventScan { events: Vec::new(), skipped: 0 };
    for file in files {
        for line in String::from_utf8_lossy(&fs::read(&file)?).lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(line) {
                Ok(e) => scan.events.push(e),
                Err(_) => scan.skipped += 1,
            }
        }
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    #[test]
    fn read_since_reads_from_that_day_on_and_only_what_came_since() {
        let home = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(home.path().join("events")).unwrap();
        let at = |d, h| chrono::Utc.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap();
        let ev = |d, h| Event::new("records".into(), "records.item.created", json!({"d": d, "h": h}), at(d, h));
        let (old, before, after, next_day) = (ev(18, 9), ev(19, 8), ev(19, 10), ev(20, 7));
        for e in [&old, &before, &after, &next_day] {
            append(home.path(), e).unwrap();
        }
        // A line no older file could hold is never even read: break the 18th's file.
        fs::write(home.path().join("events/2026-10-18.jsonl"), [0xff, 0xfe]).unwrap();
        let scan = read_since(home.path(), at(19, 9)).unwrap();
        assert_eq!(scan.events, [after, next_day]);
        assert_eq!(scan.skipped, 0, "the 18th's file wasn't read");
    }
}
