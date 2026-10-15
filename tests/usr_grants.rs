//! 授权表(`<库>/grants.json`)的 testbench(6.4).
//!
//! 覆盖:往返与**规范化**(排序,去重,列级记录归并),缺文件 = 没有授权,版本不认识报错,
//! 表级 / 列级的判定(`allows` 与 `allows_any`),`REVOKE` 的两档口径(不给列只删整表那条),
//! `prune_tables` / `set_owned_away`,以及**借别人库的锁**(`with_grants`:人家正连着就不改).
//!
//! 走会话的那些在 `tests/usr_share.rs`;跨进程在 `tests/ysql_cli.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::usr::grants::{Grants, Privilege as P, TableGrant};
use ydbms::usr::ServiceDir;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 自清理的临时目录.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-grants-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn service(&self) -> ServiceDir {
        ServiceDir::resolve(Some(self.path.to_path_buf()), None).expect("服务目录")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn columns(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

// ------------------------------------------------------------ 往返与规范化

#[test]
fn grants_round_trip_and_are_normalized() {
    let dir = TempDir::new("round-trip");
    let mut grants = Grants::empty();

    grants.set_connect("carol", true);
    grants.set_connect("alice", true);
    grants.set_connect("alice", true); // 重复不算错
    grants.set_table("movies", "alice", P::Select, &[], true);
    grants.set_table("movies", "alice", P::Insert, &columns(&["title", "movie_id"]), true);
    grants.set_table("movies", "alice", P::Insert, &columns(&["title"]), true); // 并进同一条
    grants.save(dir.path()).expect("写盘");

    let back = Grants::load(dir.path()).expect("读回").expect("文件在");
    assert_eq!(back.connect, vec!["alice".to_string(), "carol".to_string()]);
    assert!(back.allows("alice", "movies", P::Select, "anything"), "整表覆盖每一列");
    assert!(back.allows("alice", "movies", P::Insert, "title"));
    assert!(back.allows("alice", "movies", P::Insert, "movie_id"));
    assert!(!back.allows("alice", "movies", P::Insert, "year"), "只授了那两列");
    assert!(!back.allows("alice", "movies", P::Update, "title"), "没授 UPDATE");
    assert!(!back.allows("bob", "movies", P::Select, "title"), "没给 bob");

    // 列级记录归并成一条,列也排好序
    let inserts: Vec<&TableGrant> = back
        .tables
        .iter()
        .filter(|grant| grant.privilege == P::Insert)
        .collect();
    assert_eq!(inserts.len(), 1, "同一种权限的列级记录只留一条");
    assert_eq!(inserts[0].columns, columns(&["movie_id", "title"]));

    // 写回去是稳定的(同样的内容,同样的顺序)
    let text = std::fs::read_to_string(dir.path().join("grants.json")).expect("读文件");
    let mut again = back.clone();
    again.save(dir.path()).expect("再写一遍");
    assert_eq!(
        text,
        std::fs::read_to_string(dir.path().join("grants.json")).unwrap()
    );
}

#[test]
fn a_missing_grants_file_means_no_grants_and_a_bad_one_is_refused() {
    let dir = TempDir::new("missing");
    assert!(Grants::load(dir.path()).expect("缺文件不算错").is_none());
    assert_eq!(Grants::load_or_empty(dir.path()).expect("空授权"), Grants::empty());

    std::fs::write(dir.path().join("grants.json"), "{ 这不是 JSON }").unwrap();
    let err = Grants::load(dir.path()).expect_err("坏文件要报错");
    assert!(err.to_string().contains("grants.json"), "{err}");

    std::fs::write(
        dir.path().join("grants.json"),
        r#"{"version": 99, "connect": [], "tables": []}"#,
    )
    .unwrap();
    let err = Grants::load(dir.path()).expect_err("版本不认识要报错");
    assert!(err.to_string().contains("99"), "{err}");
}

// ------------------------------------------------------------------ 判定

#[test]
fn allows_any_is_the_rule_for_queries_that_read_no_column() {
    let mut grants = Grants::empty();
    grants.set_table("movies", "alice", P::Select, &columns(&["title"]), true);

    assert!(grants.allows_any("alice", "movies", P::Select), "有一列的 SELECT 就够");
    assert!(!grants.allows_any("alice", "movies", P::Insert), "别的权限不算");
    assert!(!grants.allows_any("alice", "other", P::Select), "别的表不算");
}

#[test]
fn mentions_covers_both_sides_of_the_file() {
    let mut grants = Grants::empty();
    grants.set_connect("alice", true);
    grants.set_table("movies", "bob", P::Select, &[], true);

    assert!(grants.mentions("alice"));
    assert!(grants.mentions("bob"));
    assert!(!grants.mentions("carol"));
}

// -------------------------------------------------------------- 收权限的两档

#[test]
fn revoking_without_a_column_list_only_drops_the_table_level_record() {
    let mut grants = Grants::empty();
    grants.set_table("movies", "alice", P::Select, &[], true);
    grants.set_table("movies", "alice", P::Select, &columns(&["title"]), true);

    grants.set_table("movies", "alice", P::Select, &[], false);
    assert!(!grants.allows("alice", "movies", P::Select, "year"), "整表那条没了");
    assert!(grants.allows("alice", "movies", P::Select, "title"), "列级那条还在");
    assert_eq!(grants.tables.len(), 1);

    // 反过来:按列收,只摘那一列
    grants.set_table("movies", "alice", P::Select, &columns(&["title"]), false);
    assert!(!grants.allows("alice", "movies", P::Select, "title"));
    assert!(grants.tables.is_empty(), "摘空了连记录一起删");
}

#[test]
fn pruning_and_dropping_owned_clear_the_right_records() {
    let mut grants = Grants::empty();
    grants.set_connect("alice", true);
    grants.set_table("movies", "alice", P::Select, &[], true);
    grants.set_table("credits", "bob", P::Select, &[], true);

    assert!(grants.prune_tables(&["movies".to_string()]), "动过了");
    assert!(!grants.prune_tables(&["movies".to_string()]), "再删没什么可删");
    assert!(grants.tables.iter().all(|grant| grant.table != "movies"));

    grants.set_owned_away("alice");
    assert!(!grants.may_connect("alice"));
    assert!(grants.tables.iter().all(|grant| grant.grantee != "alice"));
    assert_eq!(grants.tables.len(), 1, "bob 的那条留着");
}

// -------------------------------------------------------------- 借别人库的锁

#[test]
fn a_borrowed_lock_refuses_to_write_while_someone_else_holds_it() {
    let dir = TempDir::new("borrowed-lock");
    let service = dir.service();
    service.create_database("other", "owner").expect("建库");

    // 另一个进程(这里用手里的句柄)正连着:授权表**不许改**
    let held = service.open("other", &ydbms::usr::User {
        name: "owner".to_string(),
        salt: String::new(),
        password_hash: String::new(),
        is_admin: true,
        created_unix: 0,
        can_login: true,
        can_create_db: true,
        can_create_role: true,
        inherit: true,
        member_of: Vec::new(),
    }).expect("打开 other");

    let err = service
        .with_grants("other", |grants| {
            grants.set_connect("alice", true);
            Ok(())
        })
        .expect_err("别人正连着就不该改");
    assert!(err.to_string().contains("另一个进程"), "{err}");

    // 放手之后就能改(而且是读-改-写)
    drop(held);
    service
        .with_grants("other", |grants| {
            grants.set_connect("alice", true);
            Ok(())
        })
        .expect("放手之后可以改");
    let back = Grants::load(&dir.path().join("other")).unwrap().unwrap();
    assert!(back.may_connect("alice"));

    // 不存在的库:报"没有这个库"
    let err = service
        .with_grants("nope", |_| Ok(()))
        .expect_err("没有的库");
    assert!(err.to_string().contains("unknown database"), "{err}");
}
