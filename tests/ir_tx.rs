//! 事务的撤销记录(TODO 3.17):**追加式写**不必克隆整表.
//!
//! `INSERT` / `COPY` 只往表尾加行,所以撤销只需要"把表截断回事务开始时的行数";
//! `UPDATE` 改的是已有行的内容,它那条撤销记录仍然是一份整表克隆.
//!
//! 用一张**会数快照**的表看得见这件事:整表克隆走 `Table::snapshot`,追加式的
//! 记录连一次都不该调它.

use std::cell::Cell;
use std::rc::Rc;

use ydbms::db::table::{
    Column, ColumnsShape, DataType, DbError, Result as DbResult, Row, TabShape, Table, Value,
};
use ydbms::ir::{run_script, SymbolTable};

/// 一张会数 `snapshot()` 的 `t(k INT, v INT)`.
#[derive(Clone)]
struct CountingTable {
    shape: ColumnsShape,
    rows: Vec<Row>,
    snapshots: Rc<Cell<usize>>,
}

impl CountingTable {
    fn new(n: i32, snapshots: &Rc<Cell<usize>>) -> Self {
        CountingTable {
            shape: ColumnsShape::new(vec![
                Column::new("k", DataType::Int),
                Column::new("v", DataType::Int),
            ]),
            rows: (0..n)
                .map(|k| vec![Value::Int(k), Value::Int(k * 10)])
                .collect(),
            snapshots: Rc::clone(snapshots),
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
        self.rows.get(r).cloned()
    }

    fn row_ref(&self, r: usize) -> Option<&[Value]> {
        self.rows.get(r).map(|row| row.as_slice())
    }

    fn insert(&mut self, row: Row) -> DbResult<usize> {
        self.rows.push(row);
        Ok(self.rows.len() - 1)
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

    /// 整表快照:数一次,然后克隆自己(物理表也是克隆).
    fn snapshot(&self) -> Box<dyn Table> {
        self.snapshots.set(self.snapshots.get() + 1);
        Box::new(self.clone())
    }
}

/// 一张 `n` 行的 `t`,外加它的快照计数器.
fn counting(n: i32) -> (SymbolTable, Rc<Cell<usize>>) {
    let snapshots = Rc::new(Cell::new(0));
    let mut symbols = SymbolTable::new();
    symbols
        .define("t", CountingTable::new(n, &snapshots))
        .expect("建 t");
    (symbols, snapshots)
}

/// 跑一段脚本,断言每一条都成功.
fn ok(db: &mut SymbolTable, sql: &str) {
    let results = run_script(sql, db).expect("应当能解析");
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了:{:?}", r.index, r.outcome);
    }
}

/// `t` 的行数.
fn nrows(db: &SymbolTable) -> usize {
    db.relation("t").expect("表在").nrows()
}

/// `t` 第 `r` 行的第 `c` 列(INT).
fn cell(db: &SymbolTable, r: usize, c: usize) -> i32 {
    db.relation("t")
        .expect("表在")
        .value(r, c)
        .and_then(|value| value.as_int())
        .expect("INT 列")
}

#[test]
fn a_rolled_back_insert_never_clones_the_table() {
    let (mut db, snapshots) = counting(1000);

    ok(
        &mut db,
        "BEGIN;\n\
         INSERT INTO t VALUES (1000, 1), (1001, 2);\n\
         ROLLBACK;",
    );

    assert_eq!(nrows(&db), 1000, "插进去的行要跟着回滚");
    assert_eq!(cell(&db, 999, 1), 9990, "原来的行一个都没动");
    assert_eq!(
        snapshots.get(),
        0,
        "追加式写的撤销记录只是'截断回原来的行数',不必克隆整表"
    );
}

#[test]
fn a_committed_insert_keeps_its_rows() {
    let (mut db, snapshots) = counting(2);

    ok(&mut db, "BEGIN; INSERT INTO t VALUES (2, 20); COMMIT;");

    assert_eq!(nrows(&db), 3);
    assert_eq!(cell(&db, 2, 1), 20);
    assert_eq!(snapshots.get(), 0, "提交更不该克隆");
}

#[test]
fn an_update_still_clones_the_table_and_rolls_back() {
    let (mut db, snapshots) = counting(5);

    ok(&mut db, "BEGIN; UPDATE t SET v = -1 WHERE k = 2; ROLLBACK;");

    assert_eq!(nrows(&db), 5);
    assert_eq!(cell(&db, 2, 1), 20, "改之前的值要回来");
    assert!(snapshots.get() >= 1, "原地改写要一份整表克隆");
}

#[test]
fn an_insert_then_an_update_both_roll_back() {
    // 两种撤销记录叠在同一张表上:先追加(`appended`),再原地改写(`full`).
    // 撤销的顺序是"先恢复整表克隆,再截断到追加之前的行数".
    let (mut db, snapshots) = counting(5);

    ok(
        &mut db,
        "BEGIN;\n\
         INSERT INTO t VALUES (5, 50);\n\
         UPDATE t SET v = -1 WHERE k = 2;\n\
         ROLLBACK;",
    );

    assert_eq!(nrows(&db), 5, "插进去的那一行不该留下");
    assert_eq!(cell(&db, 2, 1), 20, "改过的那一行要回来");
    assert!(snapshots.get() >= 1, "改写那一步要克隆");
}

#[test]
fn an_insert_then_a_drop_schema_rolls_back_to_the_original_rows() {
    // 反过来叠:`DROP SCHEMA ... CASCADE` 记一份整表克隆(里面已经有刚插的那一行),
    // 撤销时截断再把它们去掉.
    let (mut db, _) = counting(3);
    ok(&mut db, "CREATE SCHEMA s; SET search_path TO s; CREATE TABLE u (k INT, v INT);");
    ok(&mut db, "INSERT INTO u VALUES (1, 10), (2, 20);");

    ok(
        &mut db,
        "BEGIN;\n\
         INSERT INTO u VALUES (3, 30);\n\
         DROP SCHEMA s CASCADE;\n\
         ROLLBACK;",
    );

    assert_eq!(db.schemas(), ["s"], "schema 名要回来");
    let u = db.relation("s.u").expect("表要回来");
    assert_eq!(u.nrows(), 2, "刚插的那一行不该留在里面");
    assert_eq!(
        u.value(1, 1).and_then(|value| value.as_int()),
        Some(20),
        "原来的行逐值还原"
    );
}

#[test]
fn a_copy_is_an_append_too() {
    let (mut db, snapshots) = counting(2);

    // `COPY` 与 `INSERT` 走同一条写路径(逐行追加)
    let path = std::env::temp_dir().join(format!("ydbms-ir-tx-{}.tsv", std::process::id()));
    std::fs::write(&path, "2\t20\n3\t30\n").expect("写数据文件");
    let sql = format!("BEGIN;\nCOPY t FROM '{}';\nROLLBACK;", path.display());

    ok(&mut db, &sql);
    let _ = std::fs::remove_file(&path);

    assert_eq!(nrows(&db), 2, "装进去的行要跟着回滚");
    assert_eq!(snapshots.get(), 0, "`COPY` 也是追加式写");
}
