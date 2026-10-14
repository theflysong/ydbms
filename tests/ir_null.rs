//! NULL、三值逻辑、左外连接、`NOT NULL` 强制、以及 CSV 里的 NULL 记法。
//!
//! 最要紧的一条是 [`csv_round_trip_distinguishes_null_empty_and_backslash_n`]：
//! CSV 里 NULL 用 `\N`、以反斜杠开头的文本要转义，于是「NULL」「空字符串」
//! 「字面文本 `\N`」三者**互不混淆**。这是可往返性的核心断言。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, TabShape, Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{run_script, Outcome, SymbolTable};
use ydbms::pers::{CsvPersister, Persister, TablePath};

fn ok(db: &mut SymbolTable, sql: &str) {
    let results = run_script(sql, db).expect("应当能解析");
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了：{:?}", r.index, r.outcome);
    }
}

fn db(sql: &str) -> SymbolTable {
    let mut db = SymbolTable::new();
    ok(&mut db, sql);
    db
}

fn q(db: &mut SymbolTable, sql: &str) -> PhysTab {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    match results.remove(0).outcome.expect("应当成功") {
        Outcome::Rows(table) => table,
        other => panic!("期望 Rows，得到 {other:?}"),
    }
}

fn failure(db: &mut SymbolTable, sql: &str) -> DbError {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    results.remove(0).outcome.expect_err("本应失败")
}

/// 取第 `c` 列的文本形式（NULL 会显示成 `NULL`）。
fn col(table: &PhysTab, c: usize) -> Vec<String> {
    (0..table.nrows())
        .map(|r| table.value(r, c).expect("格子").to_string())
        .collect()
}

// ------------------------------------------------------------ IS NULL

#[test]
fn a_truth_still_cannot_be_projected() {
    // `IS NULL` 算出来的是真值，而真值**不能作结果列**——这条边界没有因为
    // 引入 NULL 而松动。
    let mut db = SymbolTable::new();
    let err = failure(&mut db, "SELECT NULL IS NULL AS a");
    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
}

#[test]
fn comparison_with_null_is_unknown_not_false() {
    // `= NULL` 在 SQL 里是 UNKNOWN，WHERE 把它丢掉 → 0 行（而不是报错，也不是全行）
    let mut db = db("CREATE TABLE t (a INT); INSERT INTO t VALUES (1), (2)");
    let out = q(&mut db, "SELECT a FROM t WHERE a = NULL");
    assert_eq!(out.nrows(), 0);

    let out = q(&mut db, "SELECT a FROM t WHERE a <> NULL");
    assert_eq!(out.nrows(), 0);
}

#[test]
fn is_null_selects_the_null_rows() {
    let mut db = db("CREATE TABLE t (a INT); INSERT INTO t VALUES (1), (NULL), (3)");
    let out = q(&mut db, "SELECT a FROM t WHERE a IS NULL");
    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Null));
}

#[test]
fn null_literal_projects_as_text() {
    // `SELECT NULL` 在 PostgreSQL 里也是 text
    let mut db = SymbolTable::new();
    let out = q(&mut db, "SELECT NULL AS x");
    assert_eq!(out.columns()[0].ty, DataType::Text);
    assert_eq!(out.value(0, 0), Some(Value::Null));
}

#[test]
fn null_propagates_through_arithmetic() {
    let mut db = db("CREATE TABLE t (a INT); INSERT INTO t VALUES (NULL)");
    let out = q(&mut db, "SELECT a + 1 AS b FROM t");
    assert_eq!(out.value(0, 0), Some(Value::Null));
}

// ------------------------------------------------------------ LEFT JOIN

/// 两张小表：`dept` 有 3 个部门，`emp` 只覆盖其中 2 个。
const JOIN_FIXTURE: &str = "
    CREATE TABLE dept (id INT, name TEXT);
    INSERT INTO dept VALUES (1, 'A'), (2, 'B'), (3, 'C');
    CREATE TABLE emp (id INT, dept_id INT);
    INSERT INTO emp VALUES (10, 1), (11, 1), (12, 2);
";

#[test]
fn inner_join_drops_unmatched_left_rows() {
    let mut db = db(JOIN_FIXTURE);
    let out = q(&mut db, "SELECT d.name, e.id FROM dept AS d JOIN emp AS e ON e.dept_id = d.id");
    assert_eq!(out.nrows(), 3, "部门 C 没有员工，内连接里不出现");
}

#[test]
fn left_join_keeps_unmatched_left_rows_with_nulls() {
    let mut db = db(JOIN_FIXTURE);
    let out = q(
        &mut db,
        "SELECT d.name, e.id FROM dept AS d LEFT JOIN emp AS e ON e.dept_id = d.id ORDER BY d.id, e.id",
    );

    assert_eq!(out.nrows(), 4, "3 个部门：A 两人、B 一人、C 补一行 NULL");
    assert_eq!(col(&out, 0), vec!["A", "A", "B", "C"]);
    assert_eq!(col(&out, 1), vec!["10", "11", "12", "NULL"]);
}

#[test]
fn left_join_plus_is_null_finds_the_unmatched_rows() {
    // 这就是 query2 [2-4] 的形状
    let mut db = db(JOIN_FIXTURE);
    let out = q(
        &mut db,
        "SELECT d.id, d.name FROM dept AS d LEFT JOIN emp AS e ON e.dept_id = d.id \
         WHERE e.id IS NULL ORDER BY d.id",
    );

    assert_eq!(out.nrows(), 1);
    assert_eq!(col(&out, 1), vec!["C"]);
}

#[test]
fn left_join_with_a_condition_that_matches_nothing_still_keeps_left_rows() {
    let mut db = db(JOIN_FIXTURE);
    let out = q(
        &mut db,
        "SELECT d.id, e.id FROM dept AS d LEFT JOIN emp AS e ON e.dept_id = d.id AND e.id > 100",
    );
    assert_eq!(out.nrows(), 3, "一个都没匹配，但左行全在");
    assert_eq!(col(&out, 1), vec!["NULL", "NULL", "NULL"]);
}

#[test]
fn a_left_join_makes_the_right_side_nullable() {
    // 右支的列声明了 NOT NULL，但左外连接没匹配上的行上它们**就是 NULL**。
    // NOT NULL 不能从右支带进外连接的输出 schema——否则真正产生外连接 NULL 时
    // 会报"列声明了 NOT NULL"，把合法结果判成错。这是引入外连接时踩到的坑。
    let mut db = db(
        "CREATE TABLE d (id INT NOT NULL);
         INSERT INTO d VALUES (1), (2);
         CREATE TABLE e (id INT NOT NULL, dept_id INT NOT NULL);
         INSERT INTO e VALUES (10, 1);",
    );

    let out = q(
        &mut db,
        "SELECT d.id, e.id FROM d AS d LEFT JOIN e AS e ON e.dept_id = d.id ORDER BY d.id",
    );

    assert_eq!(out.nrows(), 2);
    assert_eq!(col(&out, 1), vec!["10", "NULL"]);
}

#[test]
fn an_inner_join_keeps_the_not_null_flag() {
    // 内连接里右支的每个值都来自真实行，所以 NOT NULL 要保留
    let mut db = db(
        "CREATE TABLE d (id INT NOT NULL);
         INSERT INTO d VALUES (1);
         CREATE TABLE e (id INT NOT NULL, dept_id INT NOT NULL);
         INSERT INTO e VALUES (10, 1);",
    );

    let out = q(
        &mut db,
        "SELECT d.id, e.id FROM d AS d JOIN e AS e ON e.dept_id = d.id",
    );
    assert!(!out.columns()[1].not_null, "Join 节点自己的 schema 不该带 NOT NULL");

    // 但底层表的列定义仍然带着它
    let columns = db.columns("e").unwrap();
    assert!(columns[0].not_null);
}

// ------------------------------------------------------------ ORDER BY

#[test]
fn order_by_asc_puts_nulls_last_by_default() {
    let mut db = db("CREATE TABLE t (a INT); INSERT INTO t VALUES (2), (NULL), (1)");
    let out = q(&mut db, "SELECT a FROM t ORDER BY a");

    assert_eq!(col(&out, 0), vec!["1", "2", "NULL"]);
}

#[test]
fn order_by_desc_puts_nulls_first_by_default() {
    // PostgreSQL 的默认：DESC → NULLS FIRST。**NULL 的位置不跟着 desc 翻转。**
    let mut db = db("CREATE TABLE t (a INT); INSERT INTO t VALUES (2), (NULL), (1)");
    let out = q(&mut db, "SELECT a FROM t ORDER BY a DESC");

    assert_eq!(col(&out, 0), vec!["NULL", "2", "1"]);
}

#[test]
fn explicit_nulls_first_and_last_override_the_default() {
    let mut db = db("CREATE TABLE t (a INT); INSERT INTO t VALUES (2), (NULL), (1)");

    let out = q(&mut db, "SELECT a FROM t ORDER BY a ASC NULLS FIRST");
    assert_eq!(col(&out, 0), vec!["NULL", "1", "2"]);

    let out = q(&mut db, "SELECT a FROM t ORDER BY a DESC NULLS LAST");
    assert_eq!(col(&out, 0), vec!["2", "1", "NULL"]);
}

// ------------------------------------------------------------ NOT NULL

#[test]
fn not_null_is_now_really_enforced() {
    let mut db = SymbolTable::new();
    ok(&mut db, "CREATE TABLE t (a INT NOT NULL)");

    // 建表时 NOT NULL 不再进"未强制"名单
    let results = run_script("CREATE TABLE u (b INT NOT NULL, c INT PRIMARY KEY)", &mut db).unwrap();
    match &results[0].outcome {
        Ok(Outcome::Created { unenforced, .. }) => {
            assert!(
                !unenforced.iter().any(|s| s.contains("NOT NULL")),
                "NOT NULL 现在真的强制了：{unenforced:?}"
            );
            assert!(unenforced.iter().any(|s| s.contains("PRIMARY KEY")), "{unenforced:?}");
        }
        other => panic!("expected Created, got {other:?}"),
    }

    let err = failure(&mut db, "INSERT INTO t VALUES (NULL)");
    match err {
        DbError::BadValue { ref reason, .. } => {
            assert!(reason.contains("NOT NULL"), "{reason}");
        }
        other => panic!("expected BadValue, got {other:?}"),
    }
}

#[test]
fn a_nullable_column_still_accepts_null() {
    let mut db = db("CREATE TABLE t (a INT)");
    ok(&mut db, "INSERT INTO t VALUES (NULL)");
    assert_eq!(q(&mut db, "SELECT a FROM t").value(0, 0), Some(Value::Null));
}

#[test]
fn not_null_shows_up_in_the_column_display() {
    let db = db("CREATE TABLE t (a INT NOT NULL, b INT)");
    let shown: Vec<String> = db
        .columns("t")
        .unwrap()
        .iter()
        .map(|c| c.to_string())
        .collect();
    assert_eq!(shown, vec!["a INT NOT NULL", "b INT"]);
}

// ---------------------------------------------------------------- CSV

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-null-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn csv_round_trip_distinguishes_null_empty_and_backslash_n() {
    let dir = TempDir::new("triple");
    let at = TablePath::new(dir.path(), "t");

    let shape = ColumnsShape::new(vec![Column::new("a", DataType::Text)]);
    let mut saved = PhysTab::new("t", &shape);
    saved.insert(vec![Value::Null]).expect("NULL");
    saved.insert(vec![Value::Text(String::new())]).expect("空字符串");
    saved
        .insert(vec![Value::Text("\\N".to_string())])
        .expect("字面文本 \\N");
    saved
        .insert(vec![Value::Text("\\x".to_string())])
        .expect("以反斜杠开头");

    let persister = CsvPersister::new();
    persister.save(&saved, &at).expect("保存");
    let loaded = persister.load(&at).expect("读回");

    assert_eq!(loaded.nrows(), 4);
    for r in 0..4 {
        assert_eq!(loaded.row(r), saved.row(r), "第 {r} 行必须原样回来");
    }
}

#[test]
fn csv_null_in_an_int_column() {
    let dir = TempDir::new("intnull");
    let at = TablePath::new(dir.path(), "t");

    let shape = ColumnsShape::new(vec![Column::new("a", DataType::Int)]);
    let mut saved = PhysTab::new("t", &shape);
    saved.insert(vec![Value::Int(7)]).unwrap();
    saved.insert(vec![Value::Null]).unwrap();

    let persister = CsvPersister::new();
    persister.save(&saved, &at).unwrap();
    let loaded = persister.load(&at).unwrap();

    assert_eq!(loaded.row(0), Some(vec![Value::Int(7)]));
    assert_eq!(loaded.row(1), Some(vec![Value::Null]));
}

#[test]
fn csv_schema_records_the_default_and_old_files_still_load() {
    let dir = TempDir::new("default");
    let at = TablePath::new(dir.path(), "t");

    // `DEFAULT` 是**列定义的一部分**，跟着模式文件走；存的是 CSV 字段形式
    let shape = ColumnsShape::new(vec![
        Column::new("a", DataType::Int).with_default("7"),
        Column::new("b", DataType::Text).with_default("\\N"),
    ]);
    let saved = PhysTab::new("t", &shape);
    CsvPersister::new().save(&saved, &at).unwrap();

    let loaded = CsvPersister::new().load(&at).unwrap();
    assert_eq!(loaded.columns()[0].default_text(), Some("7"));
    assert_eq!(loaded.columns()[1].default_text(), Some("\\N"), "NULL 记作 \\N");

    // 老文件（没有 default 键）照常读
    let old = TablePath::new(dir.path(), "old");
    std::fs::write(
        old.schema_path(),
        r#"{"version":1,"name":"old","columns":[{"name":"x","ty":"INT"}]}"#,
    )
    .unwrap();
    std::fs::write(old.data_path(), "x\n1\n").unwrap();

    let loaded = CsvPersister::new().load(&old).unwrap();
    assert_eq!(loaded.columns()[0].default_text(), None, "缺键应当读成 None");
}

#[test]
fn csv_schema_records_not_null_and_old_files_still_load() {
    let dir = TempDir::new("schema");
    let at = TablePath::new(dir.path(), "t");

    let shape = ColumnsShape::new(vec![Column::not_null("a", DataType::Int)]);
    let saved = PhysTab::new("t", &shape);
    CsvPersister::new().save(&saved, &at).unwrap();

    let raw = std::fs::read_to_string(at.schema_path()).unwrap();
    assert!(raw.contains("not_null"), "schema 里应当记下 NOT NULL：{raw}");

    // 老文件（没有 not_null 键）照常读
    let old = TablePath::new(dir.path(), "old");
    std::fs::write(
        old.schema_path(),
        r#"{"version":1,"name":"old","columns":[{"name":"x","ty":"INT"}]}"#,
    )
    .unwrap();
    std::fs::write(old.data_path(), "x\n1\n").unwrap();

    let loaded = CsvPersister::new().load(&old).unwrap();
    assert!(!loaded.columns()[0].not_null, "缺键应当读成 false");
}
