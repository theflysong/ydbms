//! 用户表与口令认证的 testbench（阶段 6.1）。
//!
//! 这里只碰 `src/usr/`：加盐哈希、`users.json` 的往返、bootstrap 的口径。
//! "谁能连哪个库"在 `tests/usr_service.rs`，真进程的端到端在 `tests/ysql_cli.rs`。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::usr::{hash, AuthError, UserStore};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 自清理的临时目录。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-auth-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        TempDir { path }
    }

    /// `users.json` 的路径。
    fn users(&self) -> PathBuf {
        self.path.join("users.json")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ------------------------------------------------------------------ 哈希

#[test]
fn a_salted_hash_round_trips_and_rejects_a_wrong_password() {
    let salt = hash::random_salt().expect("取盐");
    let stored = hash::hash_password(&salt, "s3cret");

    assert!(hash::verify(&salt, "s3cret", &stored), "对的口令应当过");
    assert!(!hash::verify(&salt, "s3cre", &stored), "少一个字符就不行");
    assert!(!hash::verify(&salt, "S3CRET", &stored), "大小写要区分");

    // 同一个口令、两份不同的盐 → 哈希不同（这正是盐的用处）
    let other = hash::random_salt().expect("取盐");
    assert_ne!(other, salt, "两份盐不该撞上");
    assert_ne!(hash::hash_password(&other, "s3cret"), stored);
}

#[test]
fn hex_round_trips_and_rejects_junk() {
    let bytes: Vec<u8> = (0u8..=255).collect();
    let text = hash::to_hex(&bytes);
    assert_eq!(text.len(), 512);
    assert_eq!(hash::from_hex(&text).as_deref(), Some(&bytes[..]));

    // 大写也认，空白忽略
    assert_eq!(hash::from_hex(" ABCD \n").as_deref(), Some(&[0xab, 0xcd][..]));

    assert_eq!(hash::from_hex("abc"), None, "奇数长度不是十六进制");
    assert_eq!(hash::from_hex("zz"), None, "非十六进制字符");
}

#[test]
fn the_hash_is_sha256_of_salt_then_password() {
    // 空盐 + "abc" 就是 FIPS 180-4 里 SHA-256("abc") 的公开测试向量。
    // 用它钉住"算的到底是哪个算法、盐在前还是口令在前"。
    assert_eq!(
        hash::hash_password(&[], "abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hash::hash_password(&[], ""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

// ------------------------------------------------------------ users.json

#[test]
fn users_json_holds_no_plaintext_and_round_trips() {
    let dir = TempDir::new("plaintext");
    let path = dir.users();

    let mut store = UserStore::empty();
    store.add("bob", "hunter2", true).expect("加用户");
    store.save(&path).expect("写 users.json");

    let raw = std::fs::read_to_string(&path).expect("读回");
    assert!(!raw.contains("hunter2"), "口令不能明文落盘：{raw}");

    let back = UserStore::load(&path).expect("读回用户表");
    assert_eq!(back.users.len(), 1);
    let bob = back.find("bob").expect("bob 在");
    assert!(bob.is_admin);
    assert!(!bob.salt.is_empty() && !bob.password_hash.is_empty());
    assert!(bob.created_unix > 0, "时间戳要是 unix 秒");

    // 读回来的用户表还能认证
    assert!(back.authenticate("bob", "hunter2").is_ok());
    assert!(matches!(
        back.authenticate("bob", "hunter3"),
        Err(AuthError::BadPassword { .. })
    ));
}

#[test]
fn a_missing_users_json_is_not_the_same_as_an_empty_one() {
    let dir = TempDir::new("missing");
    let path = dir.users();

    assert!(!UserStore::exists(&path));
    assert!(
        UserStore::load(&path).is_err(),
        "不存在的用户表要报错——bootstrap 靠 exists 判断，不能靠 load 空手回来"
    );

    // 空的用户表存下去之后就是"存在且为空"
    UserStore::empty().save(&path).expect("写空表");
    assert!(UserStore::exists(&path));
    assert!(UserStore::load(&path).expect("读回").users.is_empty());
}

#[test]
fn an_unknown_user_is_refused() {
    let mut store = UserStore::empty();
    store.add("bob", "hunter2", true).unwrap();

    assert!(matches!(
        store.authenticate("alice", "hunter2"),
        Err(AuthError::UnknownUser { ref name }) if name == "alice"
    ));
}

#[test]
fn a_corrupt_salt_is_reported_as_corrupt() {
    let mut store = UserStore::empty();
    store.add("bob", "hunter2", true).unwrap();
    store.users[0].salt = "not-hex".to_string();

    assert!(matches!(
        store.authenticate("bob", "hunter2"),
        Err(AuthError::Corrupt { .. })
    ));
}

#[test]
fn a_second_user_can_be_added_but_not_a_duplicate_or_an_empty_password() {
    let mut store = UserStore::empty();
    store.add("bob", "hunter2", true).expect("第一个用户");

    // 第二个用户：不是管理员也能加（本版没有 CREATE USER，靠库 API / 手工编辑）
    let alice = store.add("alice", "correct horse", false).expect("第二个用户");
    assert!(!alice.is_admin);
    assert!(store.authenticate("alice", "correct horse").is_ok());

    assert!(store.add("bob", "other", false).is_err(), "重名要拒绝");
    assert!(store.add("carol", "", false).is_err(), "空口令要拒绝");
    assert!(store.add("  ", "x", false).is_err(), "空用户名要拒绝");
}

#[test]
fn an_unknown_version_is_refused() {
    let dir = TempDir::new("version");
    let path = dir.users();
    std::fs::write(
        &path,
        r#"{"version": 99, "users": []}"#,
    )
    .expect("写文件");

    let err = UserStore::load(&path).expect_err("版本不认识要报错");
    assert!(err.to_string().contains("99"), "{err}");
}
