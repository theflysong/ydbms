//! 拉取式执行模型:**惰性**,错误传播,共享节点.
//!
//! 这些测试不从内部结构出发,而是从**外面观察**"一行一行拉"这件事:
//!
//! * 用一张会数自己被读了几次的表,看 `LIMIT` 是否真的少读了行;
//! * 看满足 `LIMIT` 之后的行是否**根本没被求值**(那里的除零不该冒出来);
//! * 看深层源里的错误是否仍然能被报出来;
//! * 看同一个节点被两个父节点引用时只物化一次.

use std::cell::Cell;
use std::rc::Rc;

use ydbms::db::source::{drain, materialize, RowSource, TableCursor};
use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, TabShape, Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{
    execute, execute_source, BinOp, JoinKind, JoinStrategy, PlanBuilder, ProgBuilder, SortKey,
    SymbolTable,
};

/// 一张会数自己 `row()` 被调过几次的表.
///
/// 读数放在 `Rc<Cell<_>>` 里,测试才拿得到--符号表把表的所有权收走了.
struct CountingTable {
    name: String,
    shape: ColumnsShape,
    rows: Vec<Vec<Value>>,
    reads: Rc<Cell<usize>>,
}

impl CountingTable {
    /// 一张 `x INT` 的表,`x` 取 `values`.
    fn new(values: &[i32], reads: Rc<Cell<usize>>) -> Self {
        let shape = ColumnsShape::new(vec![Column::new("x", DataType::Int)]);
        let rows = values.iter().map(|v| vec![Value::Int(*v)]).collect();
        CountingTable {
            name: "t".to_string(),
            shape,
            rows,
            reads,
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
        &self.name
    }

    fn nrows(&self) -> usize {
        self.rows.len()
    }

    fn row(&self, r: usize) -> Option<Vec<Value>> {
        self.reads.set(self.reads.get() + 1);
        self.rows.get(r).cloned()
    }
}

/// 一张装了 `t(x INT)` 的符号表,外加它的读数.
fn counting(values: &[i32]) -> (SymbolTable, Rc<Cell<usize>>) {
    let reads = Rc::new(Cell::new(0));
    let mut symbols = SymbolTable::new();
    symbols
        .define("t", CountingTable::new(values, Rc::clone(&reads)))
        .expect("注册 t");
    (symbols, reads)
}

/// `1..=n`.
fn series(n: i32) -> Vec<i32> {
    (1..=n).collect()
}

/// 造一个 `1 / x` 的程序:`x = 0` 那一行会除以零.
fn one_over_x() -> ydbms::ir::Prog {
    let mut p = ProgBuilder::new();
    let one = p.int(1);
    let x = p.col(0);
    let q = p.binary(BinOp::Div, one, x);
    p.finish(q)
}

// ------------------------------------------------------------ 惰性:拉多少,读多少

#[test]
fn limit_pulls_only_what_it_needs() {
    let (symbols, reads) = counting(&series(100));

    let mut b = PlanBuilder::new();
    let s = b.scan("t");
    let root = b.limit(s, Some(1), 0).unwrap();
    let plan = b.finish(root).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Int(1)));
    assert_eq!(reads.get(), 1, "LIMIT 之下的行不该被读(全物化的老模型会读 100 行)");
}

#[test]
fn offset_and_limit_pull_exactly_that_many() {
    let (symbols, reads) = counting(&series(100));

    let mut b = PlanBuilder::new();
    let s = b.scan("t");
    let root = b.limit(s, Some(2), 2).unwrap();
    let plan = b.finish(root).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    // 跳 1,2,取 3,4
    assert_eq!(out.value(0, 0), Some(Value::Int(3)));
    assert_eq!(out.value(1, 0), Some(Value::Int(4)));
    assert_eq!(out.nrows(), 2);
    assert_eq!(reads.get(), 4, "offset + limit 就是实际要读的行数");
}

#[test]
fn limit_zero_pulls_nothing_not_even_the_offset() {
    let (symbols, reads) = counting(&series(100));

    let mut b = PlanBuilder::new();
    let s = b.scan("t");
    let root = b.limit(s, Some(0), 5).unwrap();
    let plan = b.finish(root).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 0);
    assert_eq!(reads.get(), 0, "LIMIT 0 的结果是空,连 offset 都不用跳");
}

#[test]
fn a_caller_can_stop_pulling() {
    let (symbols, reads) = counting(&series(100));

    let mut b = PlanBuilder::new();
    let s = b.scan("t");
    let plan = b.finish(s).unwrap();

    // `execute_source` 把"拉多少"交给调用方
    let mut source = execute_source(&plan, &symbols).unwrap();
    assert_eq!(source.next(), Some(vec![Value::Int(1)]));
    assert_eq!(source.next(), Some(vec![Value::Int(2)]));
    assert_eq!(reads.get(), 2, "拉了 2 行就只读 2 行");

    drop(source);
    assert_eq!(reads.get(), 2, "不再拉就不再读");
}

#[test]
fn sort_must_see_every_row_even_under_a_limit() {
    let (symbols, reads) = counting(&[3, 1, 2]);

    let mut b = PlanBuilder::new();
    let s = b.scan("t");
    let mut p = ProgBuilder::new();
    let x = p.col(0);
    let key = SortKey {
        prog: p.finish(x),
        desc: false,
        nulls_first: false,
    };
    let sorted = b.sort(s, vec![key]).unwrap();
    let root = b.limit(sorted, Some(1), 0).unwrap();
    let plan = b.finish(root).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.value(0, 0), Some(Value::Int(1)));
    assert_eq!(reads.get(), 3, "排序要随机访问,必须看完全部输入--LIMIT 省不掉它");
}

// ---------------------------------------------------------------- 错误传播

#[test]
fn an_error_behind_a_satisfied_limit_is_not_evaluated() {
    let (symbols, _) = counting(&[1, 0]);

    let mut b = PlanBuilder::new();
    let s = b.scan("t");
    let pr = b
        .project(s, vec![one_over_x()], vec!["q".to_string()])
        .unwrap();
    let root = b.limit(pr, Some(1), 0).unwrap();
    let plan = b.finish(root).unwrap();

    // 第一行算得出来;第二行 `x = 0` 落在 LIMIT 之后,**根本不会被求值**
    // (PostgreSQL 也是这样:`LIMIT` 拉够就不再往下要行了)
    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Int(1)));
}

#[test]
fn an_error_in_the_stream_surfaces() {
    let (symbols, _) = counting(&[1, 0]);

    let mut b = PlanBuilder::new();
    let s = b.scan("t");
    let pr = b
        .project(s, vec![one_over_x()], vec!["q".to_string()])
        .unwrap();
    let plan = b.finish(pr).unwrap();

    // 没有 LIMIT 挡住,第二行就会被求值,错误必须冒出来
    let err = execute(&plan, &symbols).unwrap_err();

    assert!(matches!(err, DbError::IrRuntime { .. }), "got {err:?}");
    assert!(err.to_string().contains("除以零"), "{err}");
}

#[test]
fn an_error_inside_a_scan_filter_surfaces() {
    let (symbols, _) = counting(&[1, 2, 3]);

    // 谓词是 `(x / 0) = 0`:类型上没问题(两边都是 INT),取值时除零
    let mut p = ProgBuilder::new();
    let x = p.col(0);
    let zero = p.int(0);
    let quotient = p.binary(BinOp::Div, x, zero);
    let predicate = p.binary(BinOp::Eq, quotient, zero);

    let mut b = PlanBuilder::new();
    let s = b.scan_with_filter("t", p.finish(predicate));
    let plan = b.finish(s).unwrap();

    let err = execute(&plan, &symbols).unwrap_err();

    assert!(matches!(err, DbError::IrRuntime { .. }), "got {err:?}");
    assert!(err.to_string().contains("除以零"), "{err}");
}

// ------------------------------------------------------------ 共享节点

#[test]
fn a_node_with_two_consumers_is_materialized_once() {
    let (symbols, reads) = counting(&[1, 2, 3]);

    let mut b = PlanBuilder::new();
    let s = b.scan("t");
    // **同一个**节点当左右两支:它有两个消费者
    let j = b.join(s, s, JoinKind::Inner, None, JoinStrategy::NestedLoop).unwrap();
    let plan = b.finish(j).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 9, "3 × 3 的笛卡尔积");
    assert_eq!(reads.get(), 3, "共享节点只物化一次,不是每个消费者各扫一遍");
}

#[test]
fn two_separate_scans_read_the_table_twice() {
    let (symbols, reads) = counting(&[1, 2, 3]);

    let mut b = PlanBuilder::new();
    // 两个**各自独立**的 Scan 节点:没有共享,各自拉各自的
    let left = b.scan("t");
    let right = b.scan("t");
    let j = b.join(left, right, JoinKind::Inner, None, JoinStrategy::NestedLoop).unwrap();
    let plan = b.finish(j).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 9);
    assert_eq!(reads.get(), 6, "两个节点就扫两遍");
}

// ------------------------------------------------------------ 抽象本身

#[test]
fn table_cursor_walks_a_base_table() {
    let shape = ColumnsShape::new(vec![Column::new("x", DataType::Int)]);
    let mut tab = PhysTab::new("t", &shape);
    tab.insert(vec![Value::Int(1)]).unwrap();
    tab.insert(vec![Value::Int(2)]).unwrap();

    let mut cursor = TableCursor::new(&tab);

    assert_eq!(cursor.name(), "t");
    assert_eq!(cursor.columns(), tab.columns());
    assert_eq!(cursor.next(), Some(vec![Value::Int(1)]));
    assert_eq!(cursor.next(), Some(vec![Value::Int(2)]));
    assert_eq!(cursor.next(), None);
    // 结束之后一直结束(`drain` 与 `Limit` 的跳过循环都依赖这一点)
    assert_eq!(cursor.next(), None);
    assert!(cursor.error().is_none(), "正常结束不是错误");
}

#[test]
fn drain_and_materialize_agree() {
    let shape = ColumnsShape::new(vec![Column::new("x", DataType::Int)]);
    let mut tab = PhysTab::new("t", &shape);
    for v in [1, 2, 3] {
        tab.insert(vec![Value::Int(v)]).unwrap();
    }

    let mut source = TableCursor::new(&tab);
    let rows = drain(&mut source).unwrap();

    let mut source = TableCursor::new(&tab);
    let table = materialize(&mut source).unwrap();

    assert_eq!(table.name(), "t", "物化表的名字取自源");
    assert_eq!(table.columns(), tab.columns());
    assert_eq!(table.nrows(), rows.len());
    for (r, row) in rows.iter().enumerate() {
        assert_eq!(table.row(r), Some(row.clone()), "第 {r} 行");
    }
}
