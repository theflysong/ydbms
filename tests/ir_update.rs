//! `UPDATE` 的两项改动(TODO 3.15 与 3.16),外加它们与索引的相处方式.
//!
//! 三件事要盯住:
//!
//! 1. **借用行**(3.15):`WHERE` 在借来的旧行上求值,只有命中的行才克隆整行
//!    (名字也只解一次,用一张会数数的表看得见);
//! 2. **索引路径**(3.16):`UPDATE ... WHERE k = v` 有索引时只取命中那几行,
//!    没有索引时行为与过去一样(顺序扫,逐行复核);
//! 3. **选择性作废**(3.15):只作废**写到的列**上的索引--写无关列时,那一份
//!    "键 → 行号"仍然有效,下次用到不必整表重建.
//!
//! 期望值都是**手算的**(表小到能一眼看出该剩哪几行).

use std::cell::Cell;
use std::rc::Rc;

use ydbms::db::table::{
    Column, ColumnsShape, DataType, DbError, Result as DbResult, Row, TabShape, Table, Value,
};
use ydbms::ir::{run_script, SymbolTable};

/// 一张会数数的 `t(k INT, v INT)` 的读数.
///
/// 三个分开数:`refs` 是**谓词看过几行**(借来的值,零拷贝),`rows` 是**整行克隆了
/// 几次**,`cells` 是单格读了几次(建索引用).
#[derive(Default)]
struct Counts {
    refs: Cell<usize>,
    rows: Cell<usize>,
    cells: Cell<usize>,
}

/// 会数数的 `t(k INT, v INT)`;`row_ref` 也实现了(物理表那条快路径).
#[derive(Clone)]
struct CountingTable {
    shape: ColumnsShape,
    rows: Vec<Row>,
    counts: Rc<Counts>,
}

impl CountingTable {
    fn new(n: i32, counts: &Rc<Counts>) -> Self {
        CountingTable {
            shape: ColumnsShape::new(vec![
                Column::new("k", DataType::Int),
                Column::new("v", DataType::Int),
            ]),
            rows: (0..n)
                .map(|k| vec![Value::Int(k), Value::Int(k * 10)])
                .collect(),
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

    fn row(&self, r: usize) -> Option<Row> {
        self.counts.rows.set(self.counts.rows.get() + 1);
        self.rows.get(r).cloned()
    }

    fn row_ref(&self, r: usize) -> Option<&[Value]> {
        self.counts.refs.set(self.counts.refs.get() + 1);
        self.rows.get(r).map(|row| row.as_slice())
    }

    fn value(&self, r: usize, c: usize) -> Option<Value> {
        self.counts.cells.set(self.counts.cells.get() + 1);
        self.rows.get(r).and_then(|row| row.get(c)).cloned()
    }

    fn update_row(&mut self, r: usize, row: Row) -> DbResult<()> {
        let nrows = self.rows.len();
        if r >= nrows {
            return Err(DbError::RowOutOfBounds { index: r, nrows });
        }
        self.rows[r] = row;
        Ok(())
    }

    fn truncate_to(&mut self, n: usize) -> DbResult<()> {
        self.rows.truncate(n);
        Ok(())
    }

    /// 快照就是克隆自己(物理表也这么做):事务的整表克隆**不该**走逐行 `row()`,
    /// 那不是 `UPDATE` 自己的读数.
    fn snapshot(&self) -> Box<dyn Table> {
        Box::new(self.clone())
    }
}

/// 一张 100 行的 `t`,外加它的计数器.
fn counting() -> (SymbolTable, Rc<Counts>) {
    let counts = Rc::new(Counts::default());
    let mut symbols = SymbolTable::new();
    symbols
        .define("t", CountingTable::new(100, &counts))
        .expect("建 t");
    (symbols, counts)
}

/// 跑一条语句,断言成功.
fn ok(db: &mut SymbolTable, sql: &str) {
    let results = run_script(sql, db).expect("应当能解析");
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了:{:?}", r.index, r.outcome);
    }
}

/// `t` 第 `r` 行的第 `c` 列(INT).
fn cell(db: &SymbolTable, r: usize, c: usize) -> i32 {
    db.relation("t")
        .expect("表在")
        .value(r, c)
        .and_then(|value| value.as_int())
        .expect("INT 列")
}

/// 一条查询的行数.
fn rows(db: &mut SymbolTable, sql: &str) -> usize {
    let results = run_script(sql, db).expect("解析");
    match results
        .into_iter()
        .next()
        .expect("一条")
        .outcome
        .expect("成功")
    {
        ydbms::ir::Outcome::Rows(table) => table.nrows(),
        other => panic!("期望 Rows,得到 {other:?}"),
    }
}

#[test]
fn an_update_filters_on_borrowed_rows() {
    let (mut db, counts) = counting();

    // 100 行里只有 k = 1 那一行该被克隆:`WHERE` 在**借来的**行上求值
    ok(&mut db, "UPDATE t SET v = 99 WHERE k = 1");

    assert_eq!(cell(&db, 1, 1), 99, "命中的那一行改了");
    assert_eq!(cell(&db, 0, 1), 0, "别的行不动");
    assert_eq!(counts.refs.get(), 100, "谓词把 100 行都看过一遍(零拷贝)");
    assert_eq!(counts.rows.get(), 1, "只有命中的那一行才克隆整行");
}

#[test]
fn an_update_that_hits_nothing_clones_nothing() {
    let (mut db, counts) = counting();

    // 谓词要在每一行上算一次(得读行),但**被过滤掉的行一次克隆都不做**
    ok(&mut db, "UPDATE t SET v = 1 WHERE k < 0");

    assert_eq!(cell(&db, 0, 1), 0, "一行都不该改");
    assert_eq!(counts.refs.get(), 100, "100 行都看过");
    assert_eq!(counts.rows.get(), 0, "一次整行克隆都没有");
}

#[test]
fn an_update_without_a_where_touches_every_row() {
    let (mut db, counts) = counting();

    ok(&mut db, "UPDATE t SET v = v + 1");

    assert_eq!(cell(&db, 0, 1), 1);
    assert_eq!(cell(&db, 99, 1), 991);
    // 没有 `WHERE`:100 行全命中,于是每一行都克隆一次(那正是要改的那些)
    assert_eq!(counts.rows.get(), 100, "100 行都改了");
}

#[test]
fn an_update_uses_the_index_access_path() {
    let (mut db, counts) = counting();
    ok(&mut db, "CREATE INDEX t_k ON t (k)");

    let before = counts.refs.get();
    ok(&mut db, "UPDATE t SET v = 42 WHERE k = 7");

    assert_eq!(cell(&db, 7, 1), 42);
    assert_eq!(
        counts.refs.get() - before,
        1,
        "索引路径:谓词只看命中那一行(顺序扫描要看满 100 行)"
    );

    // 对照:没有索引的列上照样是顺序扫(行为与过去一致)
    let before = counts.refs.get();
    ok(&mut db, "UPDATE t SET v = 43 WHERE v = 42");
    assert_eq!(
        counts.refs.get() - before,
        100,
        "没索引的那一列只能逐行复核"
    );
    assert_eq!(cell(&db, 7, 1), 43);
}

#[test]
fn an_update_only_invalidates_the_indexes_of_the_columns_it_writes() {
    let (mut db, _) = counting();
    ok(&mut db, "CREATE INDEX t_k ON t (k)");
    ok(&mut db, "CREATE INDEX t_v ON t (v)");

    let k_index = db.index_of("t", 0).expect("k 上的索引");
    let v_index = db.index_of("t", 1).expect("v 上的索引");
    assert!(k_index.is_built() && v_index.is_built(), "刚建好");

    // 写 `v`:只有 `v` 上的那份失效,`k` 那份"键 → 行号"仍然完全有效
    // (这就是"写无关列时不必整表重建"那条)
    ok(&mut db, "UPDATE t SET v = v + 1 WHERE k = 3");
    assert!(!v_index.is_built(), "写到的列:那份索引失效了");
    assert!(k_index.is_built(), "没写到的列:那份索引不该被作废");

    // 再写 `k`:这回轮到它失效(`-1` 是表里不可能已有的值,免得撞上别的行)
    ok(&mut db, "UPDATE t SET k = -1 WHERE k = 3");
    assert!(!k_index.is_built(), "写到的列:那份索引失效了");

    // 失效只是"下次用到时重建一次":结果照旧对
    assert_eq!(rows(&mut db, "SELECT k FROM t WHERE k = -1"), 1);
    assert!(k_index.is_built(), "用到时重建一次");
}

#[test]
fn an_update_of_the_indexed_column_keeps_answers_correct() {
    // 索引作废之后**绝不能**拿旧行号回答新键:改完立刻查旧键与新键
    let (mut db, _) = counting();
    ok(&mut db, "CREATE INDEX t_k ON t (k)");

    ok(&mut db, "UPDATE t SET k = 1000 WHERE k = 7");

    assert_eq!(rows(&mut db, "SELECT k FROM t WHERE k = 7"), 0, "旧键不该命中");
    assert_eq!(
        rows(&mut db, "SELECT k FROM t WHERE k = 1000"),
        1,
        "新键要命中"
    );
}
