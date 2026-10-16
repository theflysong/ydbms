//! 六种数据类型的 testbench:`TEXT` / `CHAR(N)` / `VARCHAR(N)` / `TIMESTAMP(N)` /
//! `DATE` / `NUMERIC(N,M)`.
//!
//! 重点在三处:
//!
//! 1. **typmod 真的被强制**(长度按字符算,精度标度超了报错);
//! 2. **`NUMERIC` 的排序不是字典序**--`2 < 10` 必须成立,这一条直接钉住手写的
//!    定点比较;
//! 3. **`TIMESTAMP` 规范化成定宽文本**,于是字典序就等于时间序.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, TabShape, Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{run_script, Outcome, SymbolTable};
use ydbms::pers::{CsvPersister, Persister, TablePath};

/// 跑一段 SQL,要求全部成功.
fn ok(db: &mut SymbolTable, sql: &str) {
    let results = run_script(sql, db).expect("应当能解析");
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了:{:?}", r.index, r.outcome);
    }
}

/// 建一张表并填好数据.
fn db_with(sql: &str) -> SymbolTable {
    let mut db = SymbolTable::new();
    ok(&mut db, sql);
    db
}

/// 跑一条查询.
fn query(db: &mut SymbolTable, sql: &str) -> PhysTab {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    match results.remove(0).outcome.expect("应当成功") {
        Outcome::Rows(table) => table,
        other => panic!("期望 Rows,得到 {other:?}"),
    }
}

/// 跑一条语句并要求失败.
fn failure(db: &mut SymbolTable, sql: &str) -> DbError {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    results.remove(0).outcome.expect_err("本应失败")
}

/// 取第 `c` 列的文本形式.
fn col(db: &PhysTab, c: usize) -> Vec<String> {
    (0..db.nrows())
        .map(|r| db.value(r, c).expect("格子").to_string())
        .collect()
}

fn ty_of(db: &SymbolTable, table: &str, column: &str) -> DataType {
    db.columns(table)
        .expect("表存在")
        .iter()
        .find(|c| c.name == column)
        .expect("列存在")
        .ty
}

// ------------------------------------------------------------ 建表与 typmod

#[test]
fn create_table_accepts_all_six_types() {
    let db = db_with(
        "CREATE TABLE m (a TEXT, b CHAR(4), c VARCHAR(30), d TIMESTAMP(3), e NUMERIC(10,2), \
         f DATE, g date)",
    );

    assert_eq!(ty_of(&db, "m", "a"), DataType::Text);
    assert_eq!(ty_of(&db, "m", "b"), DataType::Char(4));
    assert_eq!(ty_of(&db, "m", "c"), DataType::VarChar(30));
    assert_eq!(ty_of(&db, "m", "d"), DataType::Timestamp(3));
    assert_eq!(ty_of(&db, "m", "e"), DataType::Numeric(10, 2));
    assert_eq!(ty_of(&db, "m", "f"), DataType::Date);
    assert_eq!(ty_of(&db, "m", "g"), DataType::Date);
}

// ---------------------------------------------------------------------- DATE

#[test]
fn date_is_canonicalised_and_orders_as_text() {
    let mut db = db_with("CREATE TABLE m (d DATE)");
    ok(&mut db, "INSERT INTO m VALUES ('2017-3-8'), ('2017-01-31'), (NULL)");

    // 规范成定宽文本(补零),于是字典序就是时间序
    assert_eq!(
        col(&query(&mut db, "SELECT d FROM m ORDER BY d"), 0),
        vec!["2017-01-31".to_string(), "2017-03-08".to_string(), "NULL".to_string()]
    );
}

#[test]
fn date_rejects_impossible_values() {
    let mut db = db_with("CREATE TABLE m (d DATE)");

    for bad in ["2017-13-01", "2017-01-32", "不是日期", "2017/03/08", "10000-01-01"] {
        let err = failure(&mut db, &format!("INSERT INTO m VALUES ('{bad}')"));
        assert!(matches!(err, DbError::BadValue { .. }), "{bad}: {err:?}");
    }
}

#[test]
fn a_date_typed_literal_fits_a_date_column() {
    let mut db = db_with("CREATE TABLE m (d DATE)");
    ok(&mut db, "INSERT INTO m VALUES (DATE '2017-03-08')");

    let out = query(&mut db, "SELECT d FROM m");
    assert_eq!(out.value(0, 0), Some(Value::Date("2017-03-08".to_string())));
}

#[test]
fn date_and_timestamp_are_different_families() {
    let mut db = db_with("CREATE TABLE m (d DATE, ts TIMESTAMP)");
    ok(&mut db, "INSERT INTO m VALUES ('2017-03-08', '2017-03-08 00:00:00')");

    // 两个大类互不比较:`date = timestamp` 必须继续拒绝(PG 有隐式转换,本版没有)
    let err = failure(&mut db, "SELECT * FROM m WHERE d = ts");
    assert!(err.to_string().contains("两侧类型不同"), "{err}");
}

#[test]
fn extract_year_reads_a_date_too() {
    let mut db = db_with("CREATE TABLE m (d DATE)");
    ok(&mut db, "INSERT INTO m VALUES ('1949-05-06'), ('2015-12-31')");

    let out = query(&mut db, "SELECT EXTRACT(YEAR FROM d) AS y FROM m ORDER BY d");
    assert_eq!(out.value(0, 0), Some(Value::Int(1949)));
    assert_eq!(out.value(1, 0), Some(Value::Int(2015)));
}

// -------------------------------------------------------------------- LENGTH

#[test]
fn length_counts_characters_not_bytes() {
    let mut db = db_with("CREATE TABLE m (t TEXT)");
    let out = query(
        &mut db,
        "SELECT LENGTH('中文') AS a, LENGTH('') AS b, LENGTH(NULL) AS c, LENGTH('abc') AS d",
    );

    assert_eq!(out.value(0, 0), Some(Value::Int(2)));
    assert_eq!(out.value(0, 1), Some(Value::Int(0)));
    assert_eq!(out.value(0, 2), Some(Value::Null));
    assert_eq!(out.value(0, 3), Some(Value::Int(3)));
}

#[test]
fn length_only_takes_text() {
    let mut db = db_with("CREATE TABLE m (n INT)");
    let err = failure(&mut db, "SELECT LENGTH(n) FROM m");
    assert!(err.to_string().contains("TEXT"), "{err}");
}

#[test]
fn length_works_on_a_typed_column() {
    let mut db = db_with("CREATE TABLE m (t VARCHAR(10), n INT)");
    ok(&mut db, "INSERT INTO m VALUES ('中文', 3)");

    let out = query(&mut db, "SELECT t FROM m WHERE LENGTH(t) = 2");
    assert_eq!(out.nrows(), 1);
    let out = query(&mut db, "SELECT t FROM m WHERE LENGTH(t) = n");
    assert_eq!(out.nrows(), 0);
}

#[test]
fn bare_char_defaults_to_char_1() {
    // SQL 标准:裸 CHAR 就是 CHAR(1).注意语法树里它的实名是 bpchar.
    let db = db_with("CREATE TABLE m (a CHAR)");
    assert_eq!(ty_of(&db, "m", "a"), DataType::Char(1));
}

#[test]
fn bare_varchar_is_rejected_with_a_hint() {
    let mut db = SymbolTable::new();
    let err = failure(&mut db, "CREATE TABLE m (a VARCHAR)");

    match err {
        DbError::Unsupported { ref feature } => {
            assert!(feature.contains("VARCHAR(N)"), "{feature}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn bare_numeric_is_now_unconstrained() {
    // 放开精度之后裸 `NUMERIC` 是合法的,精度与标度都不限
    let db = db_with("CREATE TABLE m (a NUMERIC)");
    assert_eq!(ty_of(&db, "m", "a"), DataType::NumericAny);
}

#[test]
fn bare_timestamp_defaults_to_six_digits() {
    // 裸 TIMESTAMP 是合法的,默认 6 位(与 PostgreSQL 一致)
    let db = db_with("CREATE TABLE m (a TIMESTAMP)");
    assert_eq!(ty_of(&db, "m", "a"), DataType::Timestamp(6));
}

#[test]
fn timestamp_precision_over_six_is_rejected() {
    let mut db = SymbolTable::new();
    let err = failure(&mut db, "CREATE TABLE m (a TIMESTAMP(9))");
    assert!(matches!(err, DbError::Unsupported { .. }), "got {err:?}");
}

// ---------------------------------------------------------------- 存与取

#[test]
fn text_round_trips_including_awkward_characters() {
    let mut db = db_with("CREATE TABLE m (a TEXT)");
    ok(
        &mut db,
        "INSERT INTO m VALUES ('带,逗号'), ('带\"引号'), ('带''单引号')",
    );

    let table = query(&mut db, "SELECT a FROM m");
    assert_eq!(
        col(&table, 0),
        vec!["带,逗号".to_string(), "带\"引号".to_string(), "带'单引号".to_string()]
    );
}

#[test]
fn varchar_length_counts_characters_not_bytes() {
    // '中文' 是 2 个字符,6 个字节.按字节算会误判超长.
    let mut db = db_with("CREATE TABLE m (a VARCHAR(2))");
    ok(&mut db, "INSERT INTO m VALUES ('中文')");

    let err = failure(&mut db, "INSERT INTO m VALUES ('中文三')");
    match err {
        DbError::BadValue { ref reason, .. } => assert!(reason.contains("3 超过上限 2"), "{reason}"),
        other => panic!("expected BadValue, got {other:?}"),
    }
}

#[test]
fn char_length_is_enforced() {
    let mut db = db_with("CREATE TABLE m (a CHAR(2))");
    ok(&mut db, "INSERT INTO m VALUES ('us')");

    let err = failure(&mut db, "INSERT INTO m VALUES ('usa')");
    assert!(matches!(err, DbError::BadValue { .. }), "got {err:?}");
}

#[test]
fn char_is_not_blank_padded() {
    // 已知偏差:PostgreSQL 会把 char(4) 的值补到 4 位,本版存原值.
    let mut db = db_with("CREATE TABLE m (a CHAR(4))");
    ok(&mut db, "INSERT INTO m VALUES ('ab')");

    let table = query(&mut db, "SELECT a FROM m");
    assert_eq!(col(&table, 0), vec!["ab".to_string()]);
}

#[test]
fn timestamp_is_canonicalized_to_fixed_width() {
    let mut db = db_with("CREATE TABLE m (a TIMESTAMP)");
    ok(&mut db, "INSERT INTO m VALUES ('2024-1-5')");

    let table = query(&mut db, "SELECT a FROM m");
    assert_eq!(col(&table, 0), vec!["2024-01-05 00:00:00.000000".to_string()]);
}

#[test]
fn timestamp_accepts_t_separator_and_fraction() {
    let mut db = db_with("CREATE TABLE m (a TIMESTAMP)");
    ok(&mut db, "INSERT INTO m VALUES ('2024-01-05T08:30:45.5')");

    let table = query(&mut db, "SELECT a FROM m");
    assert_eq!(col(&table, 0), vec!["2024-01-05 08:30:45.500000".to_string()]);
}

#[test]
fn timestamp_rejects_nonsense() {
    let mut db = db_with("CREATE TABLE m (a TIMESTAMP)");
    let err = failure(&mut db, "INSERT INTO m VALUES ('not a time')");
    assert!(matches!(err, DbError::BadValue { .. }), "got {err:?}");
}

#[test]
fn numeric_is_canonicalized_to_strip_trailing_zeros() {
    // 规范化是必需的:不这么做,`Ord`(按值)与 `PartialEq`(按文本)会打架.
    let mut db = db_with("CREATE TABLE m (a NUMERIC(6,2))");
    ok(&mut db, "INSERT INTO m VALUES (1.50)");

    let table = query(&mut db, "SELECT a FROM m");
    assert_eq!(col(&table, 0), vec!["1.5".to_string()]);
}

#[test]
fn integer_literal_fits_a_numeric_column() {
    // 这条曾经是 bug:整数常量走的是 INT 路径,塞进 NUMERIC 列会报类型不符.
    let mut db = db_with("CREATE TABLE m (a NUMERIC(6,2))");
    ok(&mut db, "INSERT INTO m VALUES (100)");

    let table = query(&mut db, "SELECT a FROM m");
    assert_eq!(col(&table, 0), vec!["100".to_string()]);
}

// ---------------------------------------------------- NUMERIC 的排序(关键)

#[test]
fn numeric_order_is_not_lexicographic() {
    // 字典序会给 10, 2, 9 -- 所以这条直接钉死"必须定点比较"
    let mut db = db_with("CREATE TABLE m (a NUMERIC(6,2))");
    ok(&mut db, "INSERT INTO m VALUES (2), (10), (9)");

    let table = query(&mut db, "SELECT a FROM m ORDER BY a");
    assert_eq!(col(&table, 0), vec!["2", "9", "10"]);
}

#[test]
fn numeric_order_handles_negatives_and_fractions() {
    let mut db = db_with("CREATE TABLE m (a NUMERIC(8,2))");
    ok(&mut db, "INSERT INTO m VALUES (-1.5), (2), (-10), (0.25)");

    let table = query(&mut db, "SELECT a FROM m ORDER BY a");
    assert_eq!(col(&table, 0), vec!["-10", "-1.5", "0.25", "2"]);
}

#[test]
fn numeric_equality_follows_the_value() {
    let mut db = db_with("CREATE TABLE m (a NUMERIC(6,2))");
    ok(&mut db, "INSERT INTO m VALUES (1.5)");

    // 整数 1.50 与存进去的 1.5 数值相等
    let table = query(&mut db, "SELECT a FROM m WHERE a = 1.50");
    assert_eq!(table.nrows(), 1);
}

#[test]
fn numeric_different_magnitudes_compare_correctly() {
    let mut db = db_with("CREATE TABLE m (a NUMERIC(8,2))");
    ok(&mut db, "INSERT INTO m VALUES (9.99), (10.0), (0.01)");

    let table = query(&mut db, "SELECT a FROM m WHERE a > 9.99");
    assert_eq!(col(&table, 0), vec!["10".to_string()]);
}

// ------------------------------------------------------------- TIMESTAMP 排序

#[test]
fn timestamp_order_is_chronological() {
    let mut db = db_with("CREATE TABLE m (a TIMESTAMP)");
    ok(
        &mut db,
        "INSERT INTO m VALUES ('2024-06-01'), ('1999-12-31'), ('2024-01-15 08:30:00')",
    );

    let table = query(&mut db, "SELECT a FROM m ORDER BY a");
    assert_eq!(
        col(&table, 0),
        vec![
            "1999-12-31 00:00:00.000000".to_string(),
            "2024-01-15 08:30:00.000000".to_string(),
            "2024-06-01 00:00:00.000000".to_string(),
        ]
    );
}

#[test]
fn timestamp_comparison_uses_the_column_type_to_type_the_literal() {
    // 字面量本身是字符串,靠**列的类型**定成 TIMESTAMP,否则会拿 TEXT 去比
    let mut db = db_with("CREATE TABLE m (a TIMESTAMP)");
    ok(
        &mut db,
        "INSERT INTO m VALUES ('1999-12-31'), ('2024-06-01')",
    );

    let table = query(&mut db, "SELECT a FROM m WHERE a > '2000-01-01'");
    assert_eq!(table.nrows(), 1);
    assert_eq!(col(&table, 0), vec!["2024-06-01 00:00:00.000000".to_string()]);
}

// ---------------------------------------------------------------- 比较

#[test]
fn text_compares_and_orders() {
    let mut db = db_with("CREATE TABLE m (a TEXT)");
    ok(&mut db, "INSERT INTO m VALUES ('bob'), ('alice'), ('carl')");

    let table = query(&mut db, "SELECT a FROM m ORDER BY a");
    assert_eq!(col(&table, 0), vec!["alice", "bob", "carl"]);

    let one = query(&mut db, "SELECT a FROM m WHERE a = 'bob'");
    assert_eq!(one.nrows(), 1);
}

#[test]
fn int_and_numeric_compare_numerically() {
    // PostgreSQL 里**唯一**的隐式放大:int → numeric.
    // 注意 `3 > 1.5` 这一条:如果只放宽类型检查,不在运行时真的把 INT 转成
    // NUMERIC,`Value::cmp` 会退到"按大类排名比"的兜底,把 `3 > 1.5` 算成**假**.
    let mut db = db_with("CREATE TABLE t (x INT); INSERT INTO t VALUES (1)");

    let hits = |db: &mut SymbolTable, sql: &str| query(db, sql).nrows();
    assert_eq!(hits(&mut db, "SELECT x FROM t WHERE 3 > 1.5"), 1);
    assert_eq!(hits(&mut db, "SELECT x FROM t WHERE 10 > 9.5"), 1);
    assert_eq!(hits(&mut db, "SELECT x FROM t WHERE -5 < -1.5"), 1);
    assert_eq!(hits(&mut db, "SELECT x FROM t WHERE 2.5 <= 3"), 1);
    assert_eq!(hits(&mut db, "SELECT x FROM t WHERE 1 < 1.5"), 1);
    assert_eq!(hits(&mut db, "SELECT x FROM t WHERE 1.0 = 1"), 1);

    // 不相等的仍然是假,不是报错
    assert_eq!(hits(&mut db, "SELECT x FROM t WHERE 1 = 1.5"), 0);
    assert_eq!(hits(&mut db, "SELECT x FROM t WHERE 1.5 = 1"), 0);
}

#[test]
fn int_and_numeric_columns_compare() {
    let mut db = db_with(
        "CREATE TABLE t (i INT, n NUMERIC);
         INSERT INTO t VALUES (1, 1.0), (2, 1.5), (3, 3.0)",
    );

    let hits = |db: &mut SymbolTable, sql: &str| query(db, sql).nrows();
    assert_eq!(hits(&mut db, "SELECT i FROM t WHERE i = n"), 2);
    assert_eq!(hits(&mut db, "SELECT i FROM t WHERE i > n"), 1);
    assert_eq!(hits(&mut db, "SELECT i FROM t WHERE i < n"), 0);
    assert_eq!(hits(&mut db, "SELECT i FROM t WHERE n > 1"), 2);
    assert_eq!(hits(&mut db, "SELECT i FROM t WHERE i = 1.0"), 1);
}

#[test]
fn only_int_and_numeric_get_the_implicit_widening() {
    // 别的跨族比较仍然拒绝--与 PostgreSQL 的 `operator does not exist` 一致
    let mut db = db_with(
        "CREATE TABLE t (s TEXT, ts TIMESTAMP, n NUMERIC, i INT);
         INSERT INTO t VALUES ('a', '2024-01-01', 1.5, 1)",
    );

    for sql in [
        "SELECT s FROM t WHERE s = n",
        "SELECT s FROM t WHERE ts = s",
        "SELECT s FROM t WHERE s = i",
    ] {
        let err = failure(&mut db, sql);
        assert!(err.to_string().contains("类型不同"), "{sql} -> {err}");
    }
}

#[test]
fn cross_family_comparison_is_rejected() {
    let mut db = db_with("CREATE TABLE m (a TEXT)");
    let err = failure(&mut db, "SELECT a FROM m WHERE a = 5");
    assert!(err.to_string().contains("两侧类型不同"), "{err}");
}

#[test]
fn arithmetic_is_still_int_only() {
    let mut db = db_with("CREATE TABLE m (a TEXT)");
    let err = failure(&mut db, "SELECT a + 1 AS x FROM m");
    assert!(err.to_string().contains("INT"), "{err}");
}

#[test]
fn typed_literal_uses_the_written_type() {
    let mut db = db_with("CREATE TABLE m (a TIMESTAMP)");
    ok(&mut db, "INSERT INTO m VALUES (TIMESTAMP '2024-01-15 08:30:00')");

    let table = query(&mut db, "SELECT a FROM m");
    assert_eq!(col(&table, 0), vec!["2024-01-15 08:30:00.000000".to_string()]);
}

#[test]
fn cast_literal_to_int() {
    let table = query(&mut SymbolTable::new(), "SELECT '5'::int AS n");
    assert_eq!(table.value(0, 0), Some(Value::Int(5)));
}

// ------------------------------------------------------------ typmod 强制

#[test]
fn numeric_scale_is_enforced() {
    let mut db = db_with("CREATE TABLE m (a NUMERIC(4,2))");
    let err = failure(&mut db, "INSERT INTO m VALUES (1.234)");

    match err {
        DbError::BadValue { ref reason, .. } => {
            assert!(reason.contains("小数位 3"), "{reason}");
        }
        other => panic!("expected BadValue, got {other:?}"),
    }
}

#[test]
fn numeric_precision_is_enforced() {
    let mut db = db_with("CREATE TABLE m (a NUMERIC(4,2))");
    let err = failure(&mut db, "INSERT INTO m VALUES (1234.56)");
    assert!(matches!(err, DbError::BadValue { .. }), "got {err:?}");
}

#[test]
fn numeric_within_bounds_is_accepted() {
    let mut db = db_with("CREATE TABLE m (a NUMERIC(4,2))");
    ok(&mut db, "INSERT INTO m VALUES (12.34), (-12.34), (0)");

    let table = query(&mut db, "SELECT a FROM m");
    assert_eq!(col(&table, 0), vec!["12.34", "-12.34", "0"]);
}

#[test]
fn a_bare_decimal_literal_is_numeric() {
    // 与 PostgreSQL 一致:`SELECT 1.5` 的类型就是 numeric
    let mut db = SymbolTable::new();
    let table = query(&mut db, "SELECT 1.5 AS x");

    assert_eq!(table.columns()[0].ty, DataType::NumericAny);
    assert_eq!(table.value(0, 0), Some(Value::Numeric("1.5".to_string())));
}

// ---------------------------------------------------- 投影保留列类型

#[test]
fn projection_keeps_the_source_column_type() {
    let mut db = db_with("CREATE TABLE m (a VARCHAR(8), b INT)");
    ok(&mut db, "INSERT INTO m VALUES ('hi', 1)");

    let table = query(&mut db, "SELECT a FROM m");
    assert_eq!(
        table.columns()[0].ty,
        DataType::VarChar(8),
        "透传列要保留原类型,而不是退化成 TEXT"
    );
}

#[test]
fn computed_column_falls_back_to_the_family_default() {
    let mut db = db_with("CREATE TABLE m (a NUMERIC(6,2))");
    ok(&mut db, "INSERT INTO m VALUES (1.5)");

    // 没有算术,就取同一列做两次投影的别名--仍是透传,类型保留
    let table = query(&mut db, "SELECT a AS x FROM m");
    assert_eq!(table.columns()[0].ty, DataType::Numeric(6, 2));
}

// -------------------------------------------------- CASE 两分支的类型统一

/// `CASE` 一侧 `INT`,一侧 `NUMERIC` 时统一成 `NUMERIC`--**照 PostgreSQL 办**:
///
/// ```text
/// select pg_typeof(case when true then 1 else 1.5 end);  →  numeric
/// ```
#[test]
fn case_unifies_int_and_numeric_into_numeric() {
    let mut db = SymbolTable::new();
    let table = query(&mut db, "SELECT CASE WHEN true THEN 1 ELSE 1.5 END AS a");

    assert_eq!(
        table.columns()[0].ty,
        DataType::NumericAny,
        "两分支 INT / NUMERIC 的公共类型是 NUMERIC"
    );
    assert_eq!(
        table.value(0, 0),
        Some(Value::Numeric("1".to_string())),
        "取到的是 INT 那一侧,但必须已经真的放大成 NUMERIC"
    );
}

/// 取到 `NUMERIC` 那一侧,以及 `THEN` / `ELSE` 互换--四个方向都要对.
#[test]
fn case_widening_is_symmetric() {
    let mut db = SymbolTable::new();

    let table = query(&mut db, "SELECT CASE WHEN false THEN 1 ELSE 1.5 END AS a");
    assert_eq!(table.value(0, 0), Some(Value::Numeric("1.5".to_string())));

    let table = query(&mut db, "SELECT CASE WHEN true THEN 1.5 ELSE 2 END AS a");
    assert_eq!(table.value(0, 0), Some(Value::Numeric("1.5".to_string())));

    let table = query(&mut db, "SELECT CASE WHEN false THEN 1.5 ELSE 2 END AS a");
    assert_eq!(table.value(0, 0), Some(Value::Numeric("2".to_string())));
}

/// 一侧是 `NULL` 时**不该**被放大:公共类型就是另一侧自己的类型.
#[test]
fn case_with_null_does_not_widen() {
    let mut db = db_with("CREATE TABLE m (x INT)");
    ok(&mut db, "INSERT INTO m VALUES (7)");

    let table = query(&mut db, "SELECT CASE WHEN true THEN x ELSE NULL END AS a FROM m");
    assert_eq!(table.columns()[0].ty, DataType::Int);
    assert_eq!(table.value(0, 0), Some(Value::Int(7)));
}

/// 放大之后要能真的落进 `NUMERIC` 列--物化时的类型校验会盯住这件事.
#[test]
fn widened_case_fits_a_numeric_column() {
    let mut db = db_with("CREATE TABLE m (a INT, b NUMERIC(6,2))");
    ok(&mut db, "INSERT INTO m VALUES (1, 2.5)");

    let table = query(&mut db, "SELECT CASE WHEN a = 1 THEN a ELSE b END AS c FROM m");
    assert_eq!(table.value(0, 0), Some(Value::Numeric("1".to_string())));
}

/// 真的没法统一(`INT` 与 `TEXT`)时仍然拒绝,别把放大当成"什么都行".
#[test]
fn case_still_rejects_unrelated_branches() {
    let mut db = SymbolTable::new();
    let err = failure(&mut db, "SELECT CASE WHEN true THEN 1 ELSE 'x' END AS a");
    assert!(err.to_string().contains("两个分支类型不同"), "{err}");
}

// ---------------------------------------------------------------- 持久化

/// 自清理的临时目录.
static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-types-{}-{}-{}", std::process::id(), tag, n));
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
fn csv_round_trips_every_new_type() {
    let dir = TempDir::new("csv");
    let at = TablePath::new(dir.path(), "m");

    let shape = ColumnsShape::new(vec![
        Column::new("t", DataType::Text),
        Column::new("c", DataType::Char(4)),
        Column::new("v", DataType::VarChar(8)),
        Column::new("ts", DataType::Timestamp(6)),
        Column::new("n", DataType::Numeric(10, 2)),
        Column::new("d", DataType::Date),
    ]);
    let mut saved = PhysTab::new("m", &shape);
    saved
        .insert(vec![
            Value::Text("带,逗号的文本".to_string()),
            Value::Text("ab".to_string()),
            Value::Text("中文".to_string()),
            Value::Timestamp("2024-01-05 08:30:00.000000".to_string()),
            Value::Numeric("-12.34".to_string()),
            Value::Date("2017-03-08".to_string()),
        ])
        .expect("插入");

    let persister = CsvPersister::new();
    persister.save(&saved, &at).expect("保存");
    let loaded = persister.load(&at).expect("读回");

    assert_eq!(loaded.columns(), saved.columns(), "含 typmod 的形状要原样回来");
    for r in 0..saved.nrows() {
        assert_eq!(loaded.row(r), saved.row(r), "第 {r} 行");
    }
}

#[test]
fn csv_round_trips_an_unconstrained_numeric() {
    let dir = TempDir::new("bignum");
    let at = TablePath::new(dir.path(), "t");

    let shape = ColumnsShape::new(vec![Column::new("n", DataType::NumericAny)]);
    let mut saved = PhysTab::new("t", &shape);
    // 60 位有效数字:旧的 i128 实现根本存不下
    let big = "123456789012345678901234567890123456789012345678901234567890.5";
    saved
        .insert(vec![Value::Numeric(big.to_string())])
        .expect("插入");

    let persister = CsvPersister::new();
    persister.save(&saved, &at).expect("保存");
    let loaded = persister.load(&at).expect("读回");

    assert_eq!(loaded.columns()[0].ty, DataType::NumericAny);
    assert_eq!(loaded.row(0), Some(vec![Value::Numeric(big.to_string())]));

    // schema 里是一个**新增**的标记,不是改老格式
    let raw = std::fs::read_to_string(at.schema_path()).unwrap();
    assert!(raw.contains("NUMERIC_ANY"), "{raw}");
}

#[test]
fn old_schema_files_with_a_constrained_numeric_still_load() {
    // 向后兼容:老文件里是 `{"NUMERIC":[10,2]}`,仍然要读得回来
    let dir = TempDir::new("oldnum");
    let at = TablePath::new(dir.path(), "old");
    std::fs::write(
        at.schema_path(),
        r#"{"version":1,"name":"old","columns":[{"name":"n","ty":{"NUMERIC":[10,2]}}]}"#,
    )
    .unwrap();
    std::fs::write(at.data_path(), "n
12.34
").unwrap();

    let loaded = CsvPersister::new().load(&at).expect("应当能读回");
    assert_eq!(loaded.columns()[0].ty, DataType::Numeric(10, 2));
    assert_eq!(loaded.row(0), Some(vec![Value::Numeric("12.34".to_string())]));
}

#[test]
fn old_schema_files_with_plain_int_still_load() {
    // 向后兼容:老的 `"ty": "INT"` 是单元变体,仍然能解析
    let dir = TempDir::new("compat");
    let at = TablePath::new(dir.path(), "old");
    std::fs::write(
        at.schema_path(),
        r#"{"version":1,"name":"old","columns":[{"name":"x","ty":"INT"}]}"#,
    )
    .expect("写 schema");
    std::fs::write(at.data_path(), "x\n1\n2\n").expect("写数据");

    let loaded = CsvPersister::new().load(&at).expect("应当能读回");
    assert_eq!(loaded.nrows(), 2);
    assert_eq!(loaded.columns()[0].ty, DataType::Int);
}

// ------------------------------------------------------------ COPY text 字段

/// COPY text 的反转义:控制字符,八/十六进制字节,以及"认不出来的转义就是它自己".
#[test]
fn copy_fields_unescape_the_way_postgresql_does() {
    let text = DataType::Text;
    let field = |s: &str| text.from_copy_field(s).expect("应当读得出来");
    let value = |s: &str| field(s);

    assert_eq!(value("plain"), Value::Text("plain".to_string()));
    // 五个有专用写法的控制字符
    assert_eq!(value(r"\t"), Value::Text("\t".to_string()));
    assert_eq!(value(r"\n"), Value::Text("\n".to_string()));
    assert_eq!(value(r"\r"), Value::Text("\r".to_string()));
    assert_eq!(value(r"\b"), Value::Text("\u{8}".to_string()));
    assert_eq!(value(r"\f"), Value::Text("\u{c}".to_string()));
    assert_eq!(value(r"\v"), Value::Text("\u{b}".to_string()));
    // `\\` 落在"认不出来的转义"那一支,正好就是反斜杠自己
    assert_eq!(value(r"\\"), Value::Text("\\".to_string()));
    assert_eq!(value(r"a\\b"), Value::Text("a\\b".to_string()));
    assert_eq!(value(r"a\tb"), Value::Text("a\tb".to_string()));
    // 八进制 / 十六进制给的是**字节**
    assert_eq!(value(r"\101"), Value::Text("A".to_string()));
    assert_eq!(value(r"\x41"), Value::Text("A".to_string()));
    assert_eq!(value(r"\x4"), Value::Text("\u{4}".to_string()));
    // 分开写的两个字节要能拼回一个字符(`é` 的 UTF-8 是 C3 A9)
    assert_eq!(value(r"\xC3\xA9"), Value::Text("é".to_string()));
    // `\x` 后面没有十六进制数字时,`x` 就是它自己
    assert_eq!(value(r"\x"), Value::Text("x".to_string()));
    assert_eq!(value(r"\xg"), Value::Text("xg".to_string()));
    // 认不出来的转义 = 丢掉反斜杠(`\q` 就是 `q`,PostgreSQL 的文档口径)
    assert_eq!(value(r"\q"), Value::Text("q".to_string()));
    // 引号与逗号在 COPY text 里**没有**特殊含义(那是 CSV 的事),原样留着
    assert_eq!(
        value(r#"He said "hi", twice"#),
        Value::Text(r#"He said "hi", twice"#.to_string())
    );
}

/// NULL / 空字符串 / 字面文本 `\N` 三者必须始终可区分.
#[test]
fn copy_fields_keep_null_empty_and_a_literal_backslash_n_apart() {
    let text = DataType::Text;

    assert_eq!(text.from_copy_field(r"\N").unwrap(), Value::Null);
    assert_eq!(text.from_copy_field("").unwrap(), Value::Text(String::new()));
    assert_eq!(
        text.from_copy_field(r"\\N").unwrap(),
        Value::Text(r"\N".to_string())
    );

    // 非文本族:`\N` 也是 NULL,别的照旧按类型解析
    assert_eq!(DataType::Int.from_copy_field(r"\N").unwrap(), Value::Null);
    assert_eq!(DataType::Int.from_copy_field("42").unwrap(), Value::Int(42));
    assert!(matches!(
        DataType::Int.from_copy_field("abc"),
        Err(DbError::BadValue { .. })
    ));
    // 长度限制与 CSV 那条路共用 `DataType::parse`,不另写一套
    assert!(matches!(
        DataType::VarChar(2).from_copy_field("abc"),
        Err(DbError::BadValue { .. })
    ));
    // 转义拼不成合法 UTF-8:照实报错,不是安静地塞进去
    assert!(matches!(
        DataType::Text.from_copy_field(r"\xC3"),
        Err(DbError::BadValue { .. })
    ));
}
