# bitcask-kv

A log-structured key/value store following the [Bitcask paper](https://riak.com/assets/bitcask-intro.pdf):
append-only data files, an in-memory hash index ("keydir"), merge/compaction, hint files
for fast startup, and crash recovery of torn writes.

## Quick start

```sh
cargo build --release
B=target/release/bitcask
$B ./db put greeting "hello"
echo -n "from stdin" | $B ./db put note
$B ./db get greeting
$B ./db keys
$B ./db del greeting
$B ./db stats
$B ./db merge
```

As a library:

```rust
let mut db = bitcask_kv::Bitcask::open("./db")?;
db.put(b"k", b"v")?;
assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
db.delete(b"k")?;
db.merge()?;
```

## How it works

- **Data files** `0000000000.data`, … hold records
  `crc32 | key_len | val_len | key | value` (little endian). A delete appends a
  *tombstone* (`val_len = u32::MAX`). Only the newest file is written to; it rotates at
  `max_file_size` (64 MiB by default).
- **Keydir**: `HashMap<key, (file_id, offset, len)>` for every live key. `get` is one
  `pread` plus a CRC check of the record.
- **Startup** replays files oldest to newest. A file that has a **hint file**
  (`key_len | rec_len | rec_pos | key` entries, CRC-protected) is indexed from the hint
  without reading values.
- **Crash recovery**: replay stops at the first record that is truncated or fails its
  CRC, and the file is truncated there, so a torn write at the tail is discarded and
  new writes continue after the last good record. `stats` reports the bytes dropped.
- **Merge** copies live records into new files (with the highest ids) plus their hint
  files, then deletes the old files oldest-first. A crash at any point during merge leaves
  a set of files whose replay gives the correct state.
- A `LOCK` file with an OS advisory lock keeps two processes from opening the same directory.

## Tests and benchmarks

```sh
cargo test     # record codec + store tests: model check across reopens/merges/rotation,
               # truncated-tail and garbage-tail recovery, CRC corruption, CLI
cargo bench    # criterion
```

Example numbers on a laptop (100-byte values, no fsync): put ~7 µs, get ~7 µs
(one syscall each), open by scanning 10k records ~10 ms, merge of 10k writes / 1k keys ~12 ms.

## Status / limitations

Learning project.
- Single writer per process, not thread-safe (`&mut self` for writes); no concurrent merge.
- The whole keydir lives in memory (as in Bitcask), and a scan reads each data file
  fully into memory.
- Corruption in the *middle* of an older file is treated like a torn tail: everything
  after it in that file is dropped.
- `sync_on_write` is off by default: the CLI fsyncs after each command, the library
  only on `sync()`, rotation and merge. No key expiry, no range scans.
- Unix only (uses `FileExt::read_exact_at`).

## License

MIT, see [LICENSE](LICENSE).
