use bitcask_kv::{Bitcask, Options};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

fn small() -> Options {
    Options { max_file_size: 4096, sync_on_write: false }
}

fn data_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "data"))
        .collect();
    v.sort();
    v
}

#[test]
fn put_get_delete_and_reopen() {
    let t = tempfile::tempdir().unwrap();
    {
        let mut db = Bitcask::open(t.path()).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        db.put(b"a", b"3").unwrap();
        assert!(db.delete(b"b").unwrap());
        assert!(!db.delete(b"missing").unwrap());
        db.put(b"empty", b"").unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"3".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), None);
    }
    let db = Bitcask::open(t.path()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"3".to_vec()));
    assert_eq!(db.get(b"b").unwrap(), None);
    assert_eq!(db.get(b"empty").unwrap(), Some(Vec::new()));
    assert_eq!(db.keys(), vec![b"a".to_vec(), b"empty".to_vec()]);
}

#[test]
fn second_open_is_locked() {
    let t = tempfile::tempdir().unwrap();
    let _db = Bitcask::open(t.path()).unwrap();
    assert!(Bitcask::open(t.path()).is_err());
}

#[test]
fn rotation_and_model_check_across_reopens() {
    let t = tempfile::tempdir().unwrap();
    let mut model = BTreeMap::new();
    // Deterministic pseudo-random workload.
    let mut x: u64 = 0x2545F4914F6CDD1D;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..4 {
        let mut db = Bitcask::open_with(t.path(), small()).unwrap();
        for _ in 0..500 {
            let r = next();
            let key = format!("k{}", r % 60).into_bytes();
            if r % 5 == 0 {
                db.delete(&key).unwrap();
                model.remove(&key);
            } else {
                let val = format!("v{round}-{}", "x".repeat((r % 100) as usize)).into_bytes();
                db.put(&key, &val).unwrap();
                model.insert(key, val);
            }
        }
        if round == 2 {
            db.merge().unwrap();
        }
        for (k, v) in &model {
            assert_eq!(db.get(k).unwrap().as_ref(), Some(v));
        }
        assert_eq!(db.len(), model.len());
    }
    assert!(data_files(t.path()).len() > 1, "expected rotation");
    let db = Bitcask::open_with(t.path(), small()).unwrap();
    assert_eq!(db.keys(), model.keys().cloned().collect::<Vec<_>>());
}

#[test]
fn merge_reclaims_space_and_writes_hints() {
    let t = tempfile::tempdir().unwrap();
    {
        let mut db = Bitcask::open_with(t.path(), small()).unwrap();
        for i in 0..200 {
            db.put(format!("key{}", i % 10).as_bytes(), format!("value-{i:04}").as_bytes()).unwrap();
        }
        db.delete(b"key3").unwrap();
        let before = db.stats();
        db.merge().unwrap();
        let after = db.stats();
        assert!(after.total_bytes < before.total_bytes / 5, "{before:?} -> {after:?}");
        assert_eq!(after.total_bytes, after.live_bytes);
        assert_eq!(after.keys, 9);
        // Writes after merge go to the new active file.
        db.put(b"key0", b"after-merge").unwrap();
    }
    let hints: Vec<_> = fs::read_dir(t.path())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "hint"))
        .collect();
    assert!(!hints.is_empty());
    let db = Bitcask::open_with(t.path(), small()).unwrap();
    assert!(db.stats().hinted_files >= 1);
    assert_eq!(db.get(b"key0").unwrap(), Some(b"after-merge".to_vec()));
    assert_eq!(db.get(b"key9").unwrap(), Some(b"value-0199".to_vec()));
    assert_eq!(db.get(b"key3").unwrap(), None);
}

#[test]
fn crash_recovery_truncated_tail() {
    let t = tempfile::tempdir().unwrap();
    {
        let mut db = Bitcask::open(t.path()).unwrap();
        db.put(b"one", b"1").unwrap();
        db.put(b"two", b"2").unwrap();
        db.put(b"three", b"3333333333").unwrap();
        db.sync().unwrap();
    }
    // Simulate a crash mid-write: chop the last record in half.
    let file = data_files(t.path()).pop().unwrap();
    let len = fs::metadata(&file).unwrap().len();
    OpenOptions::new().write(true).open(&file).unwrap().set_len(len - 7).unwrap();
    {
        let mut db = Bitcask::open(t.path()).unwrap();
        assert_eq!(db.get(b"one").unwrap(), Some(b"1".to_vec()));
        assert_eq!(db.get(b"two").unwrap(), Some(b"2".to_vec()));
        assert_eq!(db.get(b"three").unwrap(), None);
        assert!(db.stats().recovered_bytes > 0);
        // The store keeps working, and new writes land after the repaired tail.
        db.put(b"four", b"4").unwrap();
    }
    let db = Bitcask::open(t.path()).unwrap();
    assert_eq!(db.stats().recovered_bytes, 0);
    assert_eq!(db.get(b"four").unwrap(), Some(b"4".to_vec()));
    assert_eq!(db.len(), 3);
}

#[test]
fn crash_recovery_garbage_tail() {
    let t = tempfile::tempdir().unwrap();
    {
        let mut db = Bitcask::open(t.path()).unwrap();
        db.put(b"k", b"v").unwrap();
    }
    let file = data_files(t.path()).pop().unwrap();
    let mut f = OpenOptions::new().append(true).open(&file).unwrap();
    f.write_all(&[0xAB; 40]).unwrap();
    drop(f);
    let db = Bitcask::open(t.path()).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(db.stats().recovered_bytes, 40);
}

#[test]
fn detects_corrupted_value_on_read() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Bitcask::open(t.path()).unwrap();
    db.put(b"k", b"value").unwrap();
    db.sync().unwrap();
    let file = data_files(t.path()).pop().unwrap();
    let mut bytes = fs::read(&file).unwrap();
    *bytes.last_mut().unwrap() ^= 0xFF;
    fs::write(&file, bytes).unwrap();
    assert!(db.get(b"k").is_err());
}

#[test]
fn cli_roundtrip() {
    let t = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_bitcask");
    let run = |args: &[&str]| std::process::Command::new(bin).arg(t.path()).args(args).output().unwrap();
    assert!(run(&["put", "name", "bitcask"]).status.success());
    assert_eq!(run(&["get", "name"]).stdout, b"bitcask\n");
    assert_eq!(run(&["get", "nope"]).status.code(), Some(1));
    assert!(run(&["del", "name"]).status.success());
    assert!(run(&["merge"]).status.success());
    assert!(String::from_utf8(run(&["stats"]).stdout).unwrap().contains("keys:            0"));
}
