//! 常量折叠与恒真 / 恒假的合取项(TODO 3.7).
//!
//! 四件事要盯住:
//!
//! 1. **算得出来的当场算掉**:`WHERE a = 1 + 1` 编成 `a = 2`,每行少算一次加法;
//! 2. **恒真的合取项直接丢掉**(它不影响哪些行留下);
//! 3. **恒不成立的合取项让整条 `WHERE` 变成空结果**:谓词折成一个常量,`Filter`
//!    一行都不拉,而**那张表仍旧留在计划里**(权限检查问的是 `Plan::used_columns`);
//! 4. **惰性不变**:`AND` 仍然从左到右求值,折叠不动死分支里的错误
//!    (`tests/ir_lazy.rs` 那套口径)--
//!
//! 计划文本与行数都是**手算的**.

use std::cell::Cell;
use std::rc::Rc;

use ydbms::db::table::{Column, ColumnsShape, DataType, Row, TabShape, Table, Value};
use ydbms::ir::{compile, execute, SymbolTable};

/// 一张 100 行的计数表:每次整行读记一笔.
struct CountingTable {
    shape: ColumnsShape,
    rows: Vec<Row>,
    reads: Rc<Cell<usize>>,
}

impl CountingTable {
    fn new(n: i32, reads: &Rc<Cell<usize>>) -> Self {
        CountingTable {
            shape: ColumnsShape::new(vec![Column::new("a", DataType::Int)]),
            rows: (0..n).map(|a| vec![Value::Int(a)]).collect(),
            reads: Rc::clone(reads),
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
        self.reads.set(self.reads.get() + 1);
        self.rows.get(r).cloned()
    }

    fn row_ref(&self, r: usize) -> Option<&[Value]> {
        self.rows.get(r).map(|row| row.as_slice())
    }
}

/// 一张 100 行的 `t(a INT)`,外加它的读数计数器.
fn counting() -> (SymbolTable, Rc<Cell<usize>>) {
    let reads = Rc::new(Cell::new(0));
    let mut symbols = SymbolTable::new();
    symbols
        .define("t", CountingTable::new(100, &reads))
        .expect("建 t");
    (symbols, reads)
}

/// 走一遍 `EXPLAIN`.
fn explain(symbols: &SymbolTable, sql: &str) -> String {
    compile(sql, symbols)
        .expect("应当能降级")
        .explain(symbols)
        .expect("应当能渲染")
}

/// 跑一条查询,要求成功.
fn q(symbols: &SymbolTable, sql: &str) -> ydbms::db::PhysTab {
    let plan = compile(sql, symbols).unwrap_or_else(|e| panic!("`{sql}` 降级失败:{e}"));
    execute(&plan, symbols).unwrap_or_else(|e| panic!("`{sql}` 执行失败:{e}"))
}

// --------------------------------------------------------------- 折成常量

#[test]
fn a_constant_operand_is_folded() {
    let (symbols, _) = counting();

    // `1 + 1` 在降级期就算掉了(这里 `a` 上没有索引,所以就是顺序扫 + 谓词)
    assert_eq!(
        explain(&symbols, "SELECT a FROM t WHERE a = 1 + 1"),
        "Scan(t, filter=[a = 2])\n"
    );

    // 投影里的常量子表达式同理
    let rendered = explain(&symbols, "SELECT a + (1 + 1) AS b FROM t");
    assert!(rendered.contains("exprs=[a + 2]"), "{rendered}");

    // 折出来的是**同一个答案**,不是"少算一次就变了"
    let out = q(&symbols, "SELECT a FROM t WHERE a = 1 + 1");
    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Int(2)));
}

#[test]
fn a_program_that_cannot_be_folded_is_left_alone() {
    let (symbols, _) = counting();

    // 折叠失败(`1/0`)就原样留着:**错误仍旧在运行期报**,不把它变成降级期错误
    let rendered = explain(&symbols, "SELECT 1 / 0 AS x");
    assert!(rendered.contains("1 / 0"), "{rendered}");

    let err = execute(
        &compile("SELECT 1 / 0 AS x", &symbols).expect("降级"),
        &symbols,
    )
    .expect_err("除以零");
    assert!(err.to_string().contains("除以零"), "{err}");
}

// ------------------------------------------------------- 恒真 / 恒假的合取项

#[test]
fn a_true_constant_predicate_is_dropped() {
    let (symbols, reads) = counting();

    // 恒真:连 `Filter` 都不必建(它不影响哪些行留下)
    assert_eq!(explain(&symbols, "SELECT a FROM t WHERE 1 = 1"), "Scan(t, filter=[])\n");
    assert_eq!(q(&symbols, "SELECT a FROM t WHERE 1 = 1").nrows(), 100);
    assert_eq!(reads.get(), 100, "该读的还是读");
}

#[test]
fn a_false_constant_predicate_pulls_no_row() {
    let (symbols, reads) = counting();

    let rendered = explain(&symbols, "SELECT a FROM t WHERE 1 = 0");
    assert!(rendered.contains("Filter([false])"), "{rendered}");
    assert!(rendered.contains("Scan(t, filter=[])"), "{rendered}");

    // 恒不成立:一行都不拉(所以一次整行读都没有),结果 0 行
    assert_eq!(q(&symbols, "SELECT a FROM t WHERE 1 = 0").nrows(), 0);
    assert_eq!(reads.get(), 0, "恒不成立:一行都不该被读");
}

#[test]
fn the_table_stays_in_the_plan_for_the_privilege_check() {
    let (symbols, _) = counting();

    // **坑(必须记)**:把整棵计划换成常量会让那张表从计划里消失,而语句层的权限
    // 检查正是拿 `Plan::used_columns` 问"读了哪些表"的--所以 `Filter` 留着,
    // 那张表也留着(空集合 = "这张表上至少有一列",与 `COUNT(*)` 同一个口径)
    let plan = compile("SELECT a FROM t WHERE 1 = 0", &symbols).expect("降级");
    let used = plan.used_columns(&symbols).expect("列使用分析");
    assert!(
        matches!(used.get("t"), Some(Some(columns)) if columns.contains("a")),
        "{used:?}"
    );
}

#[test]
fn a_false_conjunct_keeps_the_ones_before_it() {
    let (symbols, reads) = counting();

    // `AND` 从左到右求值(`tests/ir_lazy.rs` 钉着这条口径):恒假项**右边**的项
    // 是死项,可以丢;它**左边**的项照旧要在每一行上算--所以这里仍然要读行.
    assert_eq!(q(&symbols, "SELECT a FROM t WHERE a < 0 AND 1 = 0").nrows(), 0);
    assert_eq!(reads.get(), 100, "左边的 `a < 0` 仍然逐行算");

    // 反过来,恒假项打头时左边什么都没有:一行都不必读
    let before = reads.get();
    assert_eq!(q(&symbols, "SELECT a FROM t WHERE 1 = 0 AND a < 0").nrows(), 0);
    assert_eq!(reads.get(), before, "恒假项打头:连扫都不扫");
}
