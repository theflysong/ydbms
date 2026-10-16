//! 索引结构层的 testbench:[`Index`] 与 [`IndexSource`] 自己.
//!
//! 这里不经过 SQL,直接观察三件事:**键 → 行号**分得对不对(重复键按行号升序),
//! 把它当 [`RowSource`] 用是不是一行一行吐,以及**建索引只读一列**--最后一条用
//! 一张会分别数 `row()` 与 `value()` 的表来盯.
//!
//! "建索引只读一列"不是风格问题:索引只存键与行号.如果建它的过程要把整行搬来
//! 搬去,"内侧不必物化"这条收益就不成立了.

use std::cell::Cell;
use std::rc::Rc;

use ydbms::db::index::{Index, IndexSlot, IndexSource};
use ydbms::db::source::RowSource;
use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, Table, TabShape, Value};
use ydbms::db::PhysTab;

/// 一张会分别数 `row()`(整行)与 `value()`(单格)的表.
///
/// 形状是 `x INT, label INT`,`label = x * 10`.
struct CountingTable {
    shape: ColumnsShape,
    rows: Vec<Vec<Value>>,
    rows_read: Cell<usize>,
    cells_read: Cell<usize>,
}

impl CountingTable {
    fn new(values: &[i32]) -> Self {
        CountingTable {
            shape: ColumnsShape::new(vec![
                Column::new("x", DataType::Int),
                Column::new("label", DataType::Int),
            ]),
            rows: values
                .iter()
                .map(|v| vec![Value::Int(*v), Value::Int(v * 10)])
                .collect(),
            rows_read: Cell::new(0),
            cells_read: Cell::new(0),
        }
    }

    /// 被整行读过几次.
    fn rows_read(&self) -> usize {
        self.rows_read.get()
    }

    /// 被单格读过几次.
    fn cells_read(&self) -> usize {
        self.cells_read.get()
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
        self.rows_read.set(self.rows_read.get() + 1);
        self.rows.get(r).cloned()
    }

    /// 覆盖默认实现(默认实现走 `row()`,整行克隆):这里只碰一格.
    fn value(&self, r: usize, c: usize) -> Option<Value> {
        self.cells_read.set(self.cells_read.get() + 1);
        self.rows.get(r).and_then(|row| row.get(c)).cloned()
    }
}

/// 一张 `nrows()` 说大话的表:说有三行,一行都给不出来.
struct ShortTable {
    shape: ColumnsShape,
}

impl TabShape for ShortTable {
    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }
}

impl Table for ShortTable {
    fn name(&self) -> &str {
        "short"
    }

    fn nrows(&self) -> usize {
        3
    }

    fn row(&self, _r: usize) -> Option<Vec<Value>> {
        None
    }
}

// ------------------------------------------------------------------ 建索引

#[test]
fn an_index_groups_rows_by_key() {
    let table = CountingTable::new(&[3, 1, 2, 1]);
    let index = Index::build(&table, 0, false, "t").unwrap();

    assert_eq!(index.column(), 0);
    // 重复键的行号按**升序**(也就是表顺序)
    assert_eq!(index.lookup(&Value::Int(1)), &[1, 3]);
    assert_eq!(index.lookup(&Value::Int(3)), &[0]);
    // 查不到的键是空切片,不是错误
    assert_eq!(index.lookup(&Value::Int(99)), &[] as &[usize]);
    assert_eq!(index.nkeys(), 3);
}

#[test]
fn building_an_index_reads_only_the_indexed_column() {
    let table = CountingTable::new(&[3, 1, 2, 1]);
    let index = Index::build(&table, 1, false, "t").unwrap();

    // 4 行 = 4 次单格读(**只读第 1 列**),一次整行读都没有
    assert_eq!(table.cells_read(), 4);
    assert_eq!(table.rows_read(), 0);
    assert_eq!(index.lookup(&Value::Int(10)), &[1, 3]);
}

#[test]
fn building_an_index_on_a_missing_column_is_an_error() {
    let table = CountingTable::new(&[1, 2]);

    assert_eq!(
        Index::build(&table, 5, false, "t"),
        Err(DbError::ColumnOutOfBounds { index: 5, arity: 2 })
    );
}

#[test]
fn a_short_read_during_the_build_is_an_error() {
    let table = ShortTable {
        shape: ColumnsShape::new(vec![Column::new("x", DataType::Int)]),
    };

    let err = Index::build(&table, 0, false, "t").unwrap_err();

    assert!(matches!(err, DbError::IrRuntime { .. }), "got {err:?}");
    assert!(err.to_string().contains("建索引时读不到"), "{err}");
}

#[test]
fn numeric_keys_use_the_canonical_text() {
    // `1.5` 与 `1.50` 是**同一个** `NUMERIC`(规范文本去掉尾随零),所以同一个桶
    let shape = ColumnsShape::new(vec![Column::new("n", DataType::NumericAny)]);
    let mut table = PhysTab::new("n", &shape);
    table.insert(vec![Value::numeric("1.5").unwrap()]).unwrap();
    table.insert(vec![Value::numeric("1.50").unwrap()]).unwrap();
    table.insert(vec![Value::numeric("2").unwrap()]).unwrap();

    let index = Index::build(&table, 0, false, "t").unwrap();

    assert_eq!(index.nkeys(), 2);
    assert_eq!(index.lookup(&Value::numeric("1.5").unwrap()), &[0, 1]);
    assert_eq!(index.lookup(&Value::numeric("1.50").unwrap()), &[0, 1]);
}

// -------------------------------------------------------------- IndexSource

#[test]
fn an_index_source_is_a_row_source_over_one_key() {
    let table = CountingTable::new(&[1, 2, 1, 3, 1]);

    // 建索引是**惰性**的:光造游标一行都不读(`LIMIT 0` 之下连索引都不建)
    let slot = Rc::new(IndexSlot::new(0, false, "t_x"));
    let mut source = IndexSource::new(&table, slot, Value::Int(1));
    assert_eq!(table.cells_read(), 0);
    assert_eq!(table.rows_read(), 0);

    assert_eq!(source.name(), "t");
    assert_eq!(source.columns(), table.columns());

    // 三行命中,按表顺序;只有**命中的行**被整行读过
    assert_eq!(source.next(), Some(vec![Value::Int(1), Value::Int(10)]));
    assert_eq!(source.next(), Some(vec![Value::Int(1), Value::Int(10)]));
    assert_eq!(source.next(), Some(vec![Value::Int(1), Value::Int(10)]));
    assert_eq!(source.next(), None);
    // 结束之后一直结束
    assert_eq!(source.next(), None);
    assert!(source.error().is_none(), "正常结束不是错误");

    assert_eq!(table.cells_read(), 5, "建索引读一列");
    assert_eq!(table.rows_read(), 3, "只有命中的 3 行被整行取出");
}

#[test]
fn an_index_source_for_a_missing_key_yields_nothing() {
    let table = CountingTable::new(&[1, 2]);
    let slot = Rc::new(IndexSlot::new(0, false, "t_x"));
    let mut source = IndexSource::new(&table, slot, Value::Int(99));

    assert_eq!(source.next(), None);
    assert!(source.error().is_none(), "查不到键不是错误");
}
