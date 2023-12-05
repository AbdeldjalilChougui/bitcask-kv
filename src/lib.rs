//! A Bitcask-style key/value store.
//!
//! - Every write is appended to the *active* data file (`NNNNNNNNNN.data`).
//! - An in-memory *keydir* maps each live key to the file/offset/length of
//!   its latest record, so a read is one positioned read.
//! - When the active file reaches `max_file_size` it is frozen and a new one starts.
//! - `merge` rewrites only the live records into fresh files, writes a *hint
//!   file* next to each (key → offset, no values) so startup does not need to
//!   read values, and deletes the old files.
//! - On open, a torn or corrupt tail (crash mid-write) is detected by length and
//!   CRC checks and truncated away.

pub mod record;

use record::DecodeError;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Options {
    /// Rotate the active file once it would exceed this many bytes.
    pub max_file_size: u64,
    /// `fsync` after every put/delete (durable but slow).
    pub sync_on_write: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { max_file_size: 64 * 1024 * 1024, sync_on_write: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Loc {
    file_id: u32,
    pos: u64,
    len: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub keys: usize,
    pub data_files: usize,
    pub total_bytes: u64,
    pub live_bytes: u64,
    /// Bytes dropped from torn/corrupt file tails when the store was opened.
    pub recovered_bytes: u64,
    /// Files loaded from hint files instead of scanning data.
    pub hinted_files: usize,
}

pub struct Bitcask {
    dir: PathBuf,
    opts: Options,
    keydir: HashMap<Vec<u8>, Loc>,
    active: File,
    active_id: u32,
    active_size: u64,
    readers: HashMap<u32, File>,
    file_sizes: HashMap<u32, u64>,
    recovered_bytes: u64,
    hinted_files: usize,
    _lock: File,
}

fn data_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("{id:010}.data"))
}

fn hint_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("{id:010}.hint"))
}

fn corrupt(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn list_ids(dir: &Path) -> io::Result<Vec<u32>> {
    let mut ids = Vec::new();
    for e in fs::read_dir(dir)? {
        let name = e?.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".data") {
            if let Ok(id) = stem.parse() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

impl Bitcask {
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Bitcask> {
        Bitcask::open_with(dir, Options::default())
    }

    pub fn open_with(dir: impl AsRef<Path>, opts: Options) -> io::Result<Bitcask> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("LOCK"))?;
        acquire_lock(&lock)?;

        let ids = list_ids(&dir)?;
        let mut keydir = HashMap::new();
        let mut readers = HashMap::new();
        let mut file_sizes = HashMap::new();
        let mut recovered_bytes = 0;
        let mut hinted_files = 0;
        for &id in &ids {
            let path = data_path(&dir, id);
            let hint = hint_path(&dir, id);
            let size = if hint.exists() && load_hint(&hint, id, &mut keydir).is_ok() {
                hinted_files += 1;
                fs::metadata(&path)?.len()
            } else {
                let (valid, file_len) = scan_data(&path, id, &mut keydir)?;
                if valid < file_len {
                    // Torn write or corruption at the tail: cut it off.
                    OpenOptions::new().write(true).open(&path)?.set_len(valid)?;
                    recovered_bytes += file_len - valid;
                }
                valid
            };
            readers.insert(id, File::open(&path)?);
            file_sizes.insert(id, size);
        }

        // Keep appending to the last file unless it is full or immutable (hinted).
        let (active_id, active_size) = match ids.last() {
            Some(&id) if !hint_path(&dir, id).exists() && file_sizes[&id] < opts.max_file_size => (id, file_sizes[&id]),
            Some(&id) => (id + 1, 0),
            None => (0, 0),
        };
        let active = OpenOptions::new().create(true).append(true).open(data_path(&dir, active_id))?;
        readers.entry(active_id).or_insert(File::open(data_path(&dir, active_id))?);
        file_sizes.insert(active_id, active_size);

        Ok(Bitcask {
            dir,
            opts,
            keydir,
            active,
            active_id,
            active_size,
            readers,
            file_sizes,
            recovered_bytes,
            hinted_files,
            _lock: lock,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.active.sync_all()?;
        self.active_id += 1;
        let path = data_path(&self.dir, self.active_id);
        self.active = OpenOptions::new().create(true).append(true).open(&path)?;
        self.readers.insert(self.active_id, File::open(&path)?);
        self.active_size = 0;
        self.file_sizes.insert(self.active_id, 0);
        Ok(())
    }

    fn append(&mut self, rec: &[u8]) -> io::Result<Loc> {
        if self.active_size > 0 && self.active_size + rec.len() as u64 > self.opts.max_file_size {
            self.rotate()?;
        }
        self.active.write_all(rec)?;
        if self.opts.sync_on_write {
            self.active.sync_data()?;
        }
        let loc = Loc { file_id: self.active_id, pos: self.active_size, len: rec.len() as u32 };
        self.active_size += rec.len() as u64;
        self.file_sizes.insert(self.active_id, self.active_size);
        Ok(loc)
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        if key.len() > u16::MAX as usize || value.len() >= (u32::MAX / 2) as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "key or value too large"));
        }
        let loc = self.append(&record::encode(key, Some(value)))?;
        self.keydir.insert(key.to_vec(), loc);
        Ok(())
    }

    /// Delete a key. Returns whether it existed.
    pub fn delete(&mut self, key: &[u8]) -> io::Result<bool> {
        if !self.keydir.contains_key(key) {
            return Ok(false);
        }
        self.append(&record::encode(key, None))?;
        self.keydir.remove(key);
        Ok(true)
    }

    fn read_record(&self, loc: Loc) -> io::Result<Vec<u8>> {
        let f = self.readers.get(&loc.file_id).ok_or_else(|| corrupt("missing data file"))?;
        let mut buf = vec![0; loc.len as usize];
        f.read_exact_at(&mut buf, loc.pos)?;
        Ok(buf)
    }

    pub fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let Some(&loc) = self.keydir.get(key) else { return Ok(None) };
        let buf = self.read_record(loc)?;
        match record::decode(&buf) {
            Ok(d) if d.key == key => Ok(d.value.map(<[u8]>::to_vec)),
            Ok(_) => Err(corrupt("keydir points at a different key")),
            Err(_) => Err(corrupt(format!("checksum mismatch in file {} at {}", loc.file_id, loc.pos))),
        }
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        self.keydir.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.keydir.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keydir.is_empty()
    }

    /// All live keys, sorted.
    pub fn keys(&self) -> Vec<Vec<u8>> {
        let mut k: Vec<_> = self.keydir.keys().cloned().collect();
        k.sort();
        k
    }

    pub fn sync(&self) -> io::Result<()> {
        self.active.sync_all()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            keys: self.keydir.len(),
            data_files: self.file_sizes.len(),
            total_bytes: self.file_sizes.values().sum(),
            live_bytes: self.keydir.values().map(|l| l.len as u64).sum(),
            recovered_bytes: self.recovered_bytes,
            hinted_files: self.hinted_files,
        }
    }

    /// Compact: copy every live record into new files (with hint files) and
    /// delete all previous files. Crash-safe at any point: merged files get
    /// the highest ids and hold only the latest values, and old files are
    /// deleted oldest-first, so a replay of whatever survives is correct.
    pub fn merge(&mut self) -> io::Result<()> {
        self.active.sync_all()?;
        let old_ids: Vec<u32> = {
            let mut v: Vec<u32> = self.file_sizes.keys().copied().collect();
            v.sort_unstable();
            v
        };
        let mut next_id = self.active_id + 1;
        let mut out: Option<(u32, File, Vec<u8>, u64)> = None; // (id, data file, hint buffer, size)
        let mut new_keydir = HashMap::with_capacity(self.keydir.len());
        let mut new_sizes = HashMap::new();
        let mut merged_files = Vec::new();

        let finish = |o: (u32, File, Vec<u8>, u64), dir: &Path, sizes: &mut HashMap<u32, u64>| -> io::Result<()> {
            let (id, f, hint, size) = o;
            f.sync_all()?;
            let tmp = dir.join(format!("{id:010}.hint.tmp"));
            fs::write(&tmp, &hint)?;
            File::open(&tmp)?.sync_all()?;
            fs::rename(&tmp, hint_path(dir, id))?;
            sizes.insert(id, size);
            Ok(())
        };

        let mut entries: Vec<(&Vec<u8>, &Loc)> = self.keydir.iter().collect();
        entries.sort_by_key(|(_, l)| (l.file_id, l.pos));
        for (key, &loc) in entries {
            let rec = self.read_record(loc)?;
            record::decode(&rec).map_err(|_| corrupt("corrupt record during merge"))?;
            let need_new = match &out {
                None => true,
                Some((_, _, _, size)) => *size > 0 && size + rec.len() as u64 > self.opts.max_file_size,
            };
            if need_new {
                if let Some(o) = out.take() {
                    finish(o, &self.dir, &mut new_sizes)?;
                }
                let f = OpenOptions::new().create(true).write(true).truncate(true).open(data_path(&self.dir, next_id))?;
                merged_files.push(next_id);
                out = Some((next_id, f, Vec::new(), 0));
                next_id += 1;
            }
            let (id, f, hint, size) = out.as_mut().unwrap();
            f.write_all(&rec)?;
            hint.extend_from_slice(&record::encode_hint(key, rec.len() as u32, *size));
            new_keydir.insert(key.clone(), Loc { file_id: *id, pos: *size, len: rec.len() as u32 });
            *size += rec.len() as u64;
        }
        if let Some(o) = out.take() {
            finish(o, &self.dir, &mut new_sizes)?;
        }

        // Switch to a fresh active file after the merged ones, then drop the old files.
        let active_path = data_path(&self.dir, next_id);
        self.active = OpenOptions::new().create(true).append(true).open(&active_path)?;
        self.active_id = next_id;
        self.active_size = 0;
        new_sizes.insert(next_id, 0);
        for id in &old_ids {
            self.readers.remove(id);
            fs::remove_file(data_path(&self.dir, *id))?;
            let _ = fs::remove_file(hint_path(&self.dir, *id));
        }
        for id in merged_files.iter().chain(std::iter::once(&next_id)) {
            self.readers.insert(*id, File::open(data_path(&self.dir, *id))?);
        }
        self.keydir = new_keydir;
        self.file_sizes = new_sizes;
        Ok(())
    }
}

/// Take the exclusive directory lock. Retries briefly: a process that forks
/// (e.g. to spawn a child) shares our open file descriptions until `exec`,
/// which can make a just-released lock look held for a moment.
fn acquire_lock(lock: &File) -> io::Result<()> {
    for _ in 0..50 {
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(fs::TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::WouldBlock, "database is locked by another process"))
}

/// Replay a data file into the keydir. Returns `(valid_prefix_len, file_len)`.
fn scan_data(path: &Path, id: u32, keydir: &mut HashMap<Vec<u8>, Loc>) -> io::Result<(u64, u64)> {
    let buf = fs::read(path)?;
    let mut pos = 0usize;
    while pos < buf.len() {
        match record::decode(&buf[pos..]) {
            Ok(d) => {
                match d.value {
                    Some(_) => {
                        keydir.insert(d.key.to_vec(), Loc { file_id: id, pos: pos as u64, len: d.len as u32 });
                    }
                    None => {
                        keydir.remove(d.key);
                    }
                }
                pos += d.len;
            }
            Err(DecodeError::Truncated | DecodeError::Corrupt) => break,
        }
    }
    Ok((pos as u64, buf.len() as u64))
}

fn load_hint(path: &Path, id: u32, keydir: &mut HashMap<Vec<u8>, Loc>) -> io::Result<()> {
    let buf = fs::read(path)?;
    // Parse fully before touching the keydir so a bad hint falls back cleanly.
    let mut entries = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let (key, len, rec_pos, used) = record::decode_hint(&buf[pos..]).map_err(|_| corrupt("bad hint file"))?;
        entries.push((key.to_vec(), Loc { file_id: id, pos: rec_pos, len }));
        pos += used;
    }
    keydir.extend(entries);
    Ok(())
}
