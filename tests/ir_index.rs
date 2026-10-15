//! TODO 3.6 的 testbench:索引是**目录对象**(`CREATE INDEX` / `DROP INDEX`).
//!
//! 三件事要盯住:
//!
//! 1. **只建一次**:`CREATE INDEX` 扫一遍那一列把索引建出来,之后每条查询都用它,
//!    不再重建(用一张会数数的表看得见);
//! 2. **写得对**:`INSERT` 把新键追加进去(不重建),`UPDATE` 之后索引失效,
//!    下次用时只重建一次--**绝不能**拿旧行号回答新键(那是安静的错答案);
//! 3. **该拒绝的拒绝**:唯一索引,部分索引,多列/表达式键,`CONCURRENTLY` 一律显式
//!    报错;`IF NOT EXISTS` / `IF EXISTS` 幂等.

use std::cell::Cell;
use std::rc::Rc;

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, Result as DbResult, Table, TabShape, Value};
use ydbms::ir::{compile, execute, run_script, Outcome, SymbolTable};

/// 分开数 `row()`(整行)与 `value()`(单格)的次数--建索引只该读**一列**.
#[derive(Default)]
struct Counts {
    rows: Cell<usize>,
    cells: Cell<usize>,
}

/// 一张会数数的 `t(x INT)`.
struct CountingTable {
    shape: ColumnsShape,
    rows: Vec<Vec<Value>>,
    counts: Rc<Counts>,
}

impl CountingTable {
    fn new(tag: &str, values: &[i32], counts: &Rc<Counts>) -> Self {
        let shape = ColumnsShape::new(vec![Column::new(tag, DataType::Int)]);
        CountingTable {
            shape,
            rows: values.iter().map(|v| vec![Value::Int(*v)]).collect(),
            counts: Rc::clone(counts),
        }
    }
}

impl TabShape for CountingTable {
    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }
}

impl Table for CountingTable {
    fn name(&self) -> &str {
        "t"
    }

    fn nrows(&self) -> usize {
        self.rows.len()
    }

    fn row(&self, r: usize) -> Option<Vec<Value>> {
        self.counts.rows.set(self.counts.rows.get() + 1);
        self.rows.get(r).cloned()
    }

    fn value(&self, r: usize, c: usize) -> Option<Value> {
        self.counts.cells.set(self.counts.cells.get() + 1);
        self.rows.get(r).and_then(|row| row.get(c)).cloned()
    }

    fn insert(&mut self, row: Vec<Value>) -> DbResult<usize> {
        self.rows.push(row);
        Ok(self.rows.len() - 1)
    }

    fn update_row(&mut self, r: usize, row: Vec<Value>) -> DbResult<()> {
        let nrows = self.rows.len();
        if r >= nrows {
            return Err(DbError::RowOutOfBounds { index: r, nrows });
        }
        self.rows[r] = row;
        Ok(())
    }
}

/// 装着 `t(x INT)` 的库,外加它的计数器.
fn counting(values: &[i32]) -> (SymbolTable, Rc<Counts>) {
    let counts = Rc::new(Counts::default());
    let mut symbols = SymbolTable::new();
    symbols
        .define("t", CountingTable::new("x", values, &counts))
        .unwrap();
    (symbols, counts)
}

/// 跑一段 SQL,要求全部成功.
fn ok(db: &mut SymbolTable, sql: &str) {
    let results = run_script(sql, db).expect("应当能解析");
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了:{:?}", r.index, r.outcome);
    }
}

/// 跑一条语句并要求失败.
fn failure(db: &mut SymbolTable, sql: &str) -> DbError {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    results.remove(0).outcome.expect_err("本应失败")
}

/// 一条查询的行数.
fn rows(db: &SymbolTable, sql: &str) -> usize {
    let plan = compile(sql, db).unwrap_or_else(|e| panic!("降级失败:{e}"));
    execute(&plan, db).unwrap_or_else(|e| panic!("执行失败:{e}")).nrows()
}

/// 计划文本.
fn explain(db: &SymbolTable, sql: &str) -> String {
    compile(sql, db)
        .and_then(|plan| plan.explain(db))
        .unwrap_or_else(|e| panic!("降级失败:{e}"))
}

// -------------------------------------------------------------- 只建一次

#[test]
fn an_index_is_built_once_and_reused_by_later_queries() {
    let (mut db, counts) = counting(&[1, 2, 3, 4, 5]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");

    // `CREATE INDEX` 当场扫一遍这一列
    assert_eq!(counts.cells.get(), 5, "CREATE INDEX 读一列");
    let after_create = counts.cells.get();

    // 连着两条查询:都走索引,都**不**重建
    for round in 1..=2 {
        assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 3"), 1, "第 {round} 条");
        assert_eq!(
            counts.cells.get(),
            after_create,
            "第 {round} 条不该重新建索引"
        );
    }
    // 只有命中的那两行被整行取出
    assert_eq!(counts.rows.get(), 2);
}

#[test]
fn explain_only_shows_an_index_path_when_the_catalogue_has_one() {
    let (mut db, _) = counting(&[1, 2, 3]);

    // 没建索引:顺序扫描(本版不为"也许有用"先扫一遍表)
    assert_eq!(explain(&db, "SELECT x FROM t WHERE x = 2"), "Scan(t, filter=[x = 2])\n");

    ok(&mut db, "CREATE INDEX t_x ON t (x)");
    assert_eq!(
        explain(&db, "SELECT x FROM t WHERE x = 2"),
        "Scan(t, index=[x], filter=[x = 2])\n"
    );
}

// ---------------------------------------------------------------- 写得对

#[test]
fn insert_maintains_the_index_without_a_rescan() {
    let (mut db, counts) = counting(&[1, 2, 3]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");
    let after_create = counts.cells.get();

    ok(&mut db, "INSERT INTO t VALUES (4)");
    assert_eq!(counts.cells.get(), after_create, "维护索引不扫表");

    // 新行查得到,而且索引仍然有效(不再重建)
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 4"), 1);
    assert_eq!(counts.cells.get(), after_create);
}

#[test]
fn update_invalidates_the_index_and_it_is_rebuilt_once() {
    let (mut db, counts) = counting(&[1, 2, 3]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");

    ok(&mut db, "UPDATE t SET x = 30 WHERE x = 3");
    let after_update = counts.cells.get();

    // **绝不能用旧行号回答新键**:旧键查不到,新键查得到
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 3"), 0, "旧键不该命中");
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 30"), 1, "新键要命中");
    assert_eq!(
        counts.cells.get(),
        after_update + 3,
        "第一次用到时重建一遍(只读一列 3 格)"
    );

    // 第二条查询不再重建
    let rebuilt = counts.cells.get();
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 30"), 1);
    assert_eq!(counts.cells.get(), rebuilt);
}

#[test]
fn dropping_an_index_takes_the_access_path_away() {
    let (mut db, counts) = counting(&[1, 2, 3]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");
    let after_create = counts.cells.get();

    ok(&mut db, "DROP INDEX t_x");
    assert!(!explain(&db, "SELECT x FROM t WHERE x = 2").contains("index="));

    // 结果不变,但现在是顺序扫描(不再读那一列去建索引)
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 2"), 1);
    assert_eq!(counts.cells.get(), after_create, "没有索引就不该再读那一列");
}

// -------------------------------------------------------------- 目录的账

#[test]
fn create_index_is_recorded_in_the_catalogue() {
    let (mut db, _) = counting(&[1, 2, 3]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");

    assert_eq!(
        db.indexes(),
        vec![("t_x".to_string(), "t".to_string(), 0)],
        "索引名,表名,列都要记下来"
    );

    // 名字与表/别名在**同一个名单**里:重名报错(PostgreSQL 也是这么定的)
    let err = failure(&mut db, "CREATE INDEX t ON t (x)");
    assert!(matches!(err, DbError::DuplicateSymbol { .. }), "got {err:?}");

    // `IF NOT EXISTS` 命中时视为满足要求
    ok(&mut db, "CREATE INDEX IF NOT EXISTS t_x ON t (x)");
    assert_eq!(db.indexes().len(), 1);

    // 但表名/列名仍然要先解出来才谈得上"命中"(与 PostgreSQL 的一处记录在案的差异:
    // PG 命中名字之后连表列都不看,我们宁可把错字报出来)
    let err = failure(&mut db, "CREATE INDEX IF NOT EXISTS t_x ON t (zzz)");
    assert!(matches!(err, DbError::UnknownColumn { .. }), "got {err:?}");

    // 表 / 列必须存在
    let err = failure(&mut db, "CREATE INDEX i ON nope (x)");
    assert!(matches!(err, DbError::UnknownSymbol { .. }), "got {err:?}");
    let err = failure(&mut db, "CREATE INDEX i ON t (zzz)");
    assert!(matches!(err, DbError::UnknownColumn { .. }), "got {err:?}");
}

#[test]
fn drop_index_if_exists_is_idempotent() {
    let (mut db, _) = counting(&[1]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");

    let err = failure(&mut db, "DROP INDEX nope");
    assert!(matches!(err, DbError::UnknownSymbol { .. }), "got {err:?}");

    ok(&mut db, "DROP INDEX t_x");
    assert!(db.indexes().is_empty());
    ok(&mut db, "DROP INDEX IF EXISTS t_x");
}

#[test]
fn create_index_rejects_what_it_cannot_do() {
    let (mut db, _) = counting(&[1]);
    ok(&mut db, "CREATE TABLE c (a INT, b INT)");

    for (sql, needle) in [
        ("CREATE UNIQUE INDEX i ON t (x)", "唯一"),
        ("CREATE INDEX CONCURRENTLY i ON t (x)", "CONCURRENTLY"),
        ("CREATE INDEX i ON t (x) WHERE x > 1", "部分索引"),
        ("CREATE INDEX i ON c (a, b)", "索引键的个数"),
        ("CREATE INDEX i ON c ((a + b))", "裸列"),
        ("CREATE INDEX i ON c USING hash (a)", "USING"),
        ("CREATE INDEX i ON c (a) INCLUDE (b)", "INCLUDE"),
    ] {
        let err = failure(&mut db, sql);
        let text = err.to_string();
        assert!(text.contains(needle), "{sql}: {text}");
        assert!(db.indexes().is_empty(), "{sql}: 不该留下索引");
    }
}

#[test]
fn drop_schema_cascade_also_drops_the_indexes() {
    let (mut db, _) = counting(&[1]);
    ok(
        &mut db,
        "CREATE SCHEMA study; SET search_path TO study; CREATE TABLE m (a INT); \
         CREATE INDEX m_a ON m (a)",
    );
    // 索引的生效名跟着 `search_path` 走,与建表同一套规则
    assert_eq!(db.indexes(), vec![("study.m_a".to_string(), "study.m".to_string(), 0)]);

    // 不写 CASCADE 时如实报出"名下还有东西"
    let err = failure(&mut db, "DROP SCHEMA study");
    assert!(matches!(err, DbError::Unsupported { .. }), "got {err:?}");

    ok(&mut db, "DROP SCHEMA study CASCADE");
    assert!(db.indexes().is_empty(), "schema 下的索引也要清掉");
}

// ---------------------------------------------------------- 语句层的输出

#[test]
fn the_create_index_message_reports_the_column_and_the_keys() {
    let (mut db, _) = counting(&[1, 2, 3]);
    let results = run_script("CREATE INDEX t_x ON t (x)", &mut db).unwrap();

    match &results[0].outcome {
        Ok(Outcome::NoOp { message }) => {
            assert!(message.contains("t(x)"), "{message}");
            assert!(message.contains("3 个不同的键"), "{message}");
        }
        other => panic!("expected NoOp, got {other:?}"),
    }
}
