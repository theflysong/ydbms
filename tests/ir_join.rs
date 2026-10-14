//! 连接的三种执行策略，以及扫描的索引访问路径。
//!
//! 重点是**三种策略必须给出同一份结果**（逐行逐列），以及几条防「安静漏行」的守卫：
//! 跨大类的键不当键用、索引的键不读列、索引嵌套循环的内侧必须是基表。
//!
//! 手写 IR 的部分是为了**强迫**某一种策略——降级出来的策略是绑定器挑的，这里要能
//! 单独把每种策略跑出来对照。

use std::cell::Cell;
use std::rc::Rc;

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, Table, TabShape, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{
    compile, execute, BinOp, EqKey, IndexPath, JoinKind, JoinStrategy, Plan, PlanBuilder, Prog,
    ProgBuilder, SymbolTable,
};

// ------------------------------------------------------------------ 建库

/// `t(k INT, v INT)`。
fn kv_table(name: &str, rows: &[(i32, i32)]) -> PhysTab {
    let shape = ColumnsShape::new(vec![
        Column::new("k", DataType::Int),
        Column::new("v", DataType::Int),
    ]);
    let mut table = PhysTab::new(name, &shape);
    for (k, v) in rows {
        table.insert(vec![Value::Int(*k), Value::Int(*v)]).unwrap();
    }
    table
}

/// 两张 `(k INT, v INT)` 的库：`a` 与 `b`。
fn ab(a: &[(i32, i32)], b: &[(i32, i32)]) -> SymbolTable {
    let mut symbols = SymbolTable::new();
    symbols.define("a", kv_table("a", a)).unwrap();
    symbols.define("b", kv_table("b", b)).unwrap();
    symbols
}

/// 在 [`ab`] 的基础上加一张 `c(n NUMERIC)`——用来试跨大类的键。
fn ab_with_numeric(a: &[(i32, i32)], b: &[(i32, i32)], c: &[&str]) -> SymbolTable {
    let mut symbols = ab(a, b);
    let shape = ColumnsShape::new(vec![Column::new("n", DataType::NumericAny)]);
    let mut table = PhysTab::new("c", &shape);
    for n in c {
        table.insert(vec![Value::numeric(n).unwrap()]).unwrap();
    }
    symbols.define("c", table).unwrap();
    symbols
}

/// 允许键是 NULL 的 `t(k INT, v INT)`。
fn nullable_kv_table(name: &str, rows: &[(Option<i32>, i32)]) -> PhysTab {
    let shape = ColumnsShape::new(vec![
        Column::new("k", DataType::Int),
        Column::new("v", DataType::Int),
    ]);
    let mut table = PhysTab::new(name, &shape);
    for (k, v) in rows {
        table
            .insert(vec![
                k.map(Value::Int).unwrap_or(Value::Null),
                Value::Int(*v),
            ])
            .unwrap();
    }
    table
}

/// 一条「原样取第 `index` 列」的程序。
fn col_prog(index: usize) -> Prog {
    let mut p = ProgBuilder::new();
    let c = p.col(index);
    p.finish(c)
}

/// `a.k = b.k` 的键（两侧各自面对自己那一支的 schema）。
fn key_k() -> EqKey {
    EqKey {
        outer: col_prog(0),
        inner: col_prog(0),
    }
}

/// `a.v = b.v` 的键。
fn key_v() -> EqKey {
    EqKey {
        outer: col_prog(1),
        inner: col_prog(1),
    }
}

/// 手写一个 `a JOIN b ON a.k = b.k` 的计划，策略由调用方指定。
///
/// 输出列是 `a.k, a.v, b.k, b.v`。
fn single_key_plan(strategy: JoinStrategy, kind: JoinKind) -> Plan {
    let mut b = PlanBuilder::new();
    let left = b.scan("a");
    let right = b.scan("b");

    // `ON` 的下标按「左 ++ 右」编：a 有 2 列，所以 b.k 是 2
    let mut p = ProgBuilder::new();
    let lk = p.col(0);
    let rk = p.col(2);
    let eq = p.binary(BinOp::Eq, lk, rk);
    let on = p.finish(eq);

    let join = b.join(left, right, kind, Some(on), strategy).unwrap();
    b.finish(join).unwrap()
}

/// 三种策略各来一份。
fn all_strategies() -> Vec<(&'static str, JoinStrategy)> {
    vec![
        ("nested-loop", JoinStrategy::NestedLoop),
        (
            "hash",
            JoinStrategy::Hash {
                keys: vec![key_k()],
            },
        ),
        ("index", JoinStrategy::IndexNestedLoop { key: key_k() }),
    ]
}

/// 两张结果表逐行逐列完全一致。
fn assert_same_rows(left: &PhysTab, right: &PhysTab) {
    assert_eq!(left.nrows(), right.nrows(), "行数");
    for r in 0..left.nrows() {
        assert_eq!(left.row(r), right.row(r), "第 {r} 行");
    }
}

/// 走一遍 EXPLAIN。
fn explain(symbols: &SymbolTable, sql: &str) -> String {
    compile(sql, symbols)
        .expect("应当能降级")
        .explain(symbols)
        .expect("应当能渲染")
}

// ------------------------------------------------------------ 三种策略的等价

#[test]
fn every_strategy_gives_the_same_rows() {
    let symbols = ab(
        &[(1, 10), (2, 20), (1, 11), (3, 30)],
        &[(1, 100), (1, 101), (2, 200), (4, 400)],
    );

    let nested = execute(
        &single_key_plan(JoinStrategy::NestedLoop, JoinKind::Inner),
        &symbols,
    )
    .unwrap();
    assert_eq!(nested.nrows(), 5);

    for (name, strategy) in all_strategies() {
        let out = execute(&single_key_plan(strategy, JoinKind::Inner), &symbols).unwrap();
        assert_eq!(out.nrows(), 5, "{name}");
        assert_same_rows(&nested, &out);
    }
}

#[test]
fn a_multi_key_join_matches_nested_loop_row_for_row() {
    let symbols = ab(&[(1, 10), (2, 20), (1, 11)], &[(1, 10), (1, 99), (2, 20)]);

    // `ON a.k = b.k AND a.v = b.v`：**两个**键，候选集比单键更小
    let mut p = ProgBuilder::new();
    let lk = p.col(0);
    let rk = p.col(2);
    let eq_k = p.binary(BinOp::Eq, lk, rk);
    let lv = p.col(1);
    let rv = p.col(3);
    let eq_v = p.binary(BinOp::Eq, lv, rv);
    let both = p.binary(BinOp::And, eq_k, eq_v);
    let on = p.finish(both);

    let plan = |strategy: JoinStrategy| {
        let mut b = PlanBuilder::new();
        let left = b.scan("a");
        let right = b.scan("b");
        let join = b
            .join(left, right, JoinKind::Inner, Some(on.clone()), strategy)
            .unwrap();
        b.finish(join).unwrap()
    };

    let nested = execute(&plan(JoinStrategy::NestedLoop), &symbols).unwrap();
    let hash = execute(
        &plan(JoinStrategy::Hash {
            keys: vec![key_k(), key_v()],
        }),
        &symbols,
    )
    .unwrap();

    // (1,10) 与 (2,20) 各配上一行；(1,11) 对不上 (1,99)
    assert_eq!(nested.nrows(), 2);
    assert_same_rows(&nested, &hash);
}

#[test]
fn every_strategy_pads_a_left_join_with_nulls() {
    let symbols = ab(&[(1, 10), (2, 20)], &[(1, 100)]);

    for (name, strategy) in all_strategies() {
        let out = execute(&single_key_plan(strategy, JoinKind::Left), &symbols).unwrap();

        assert_eq!(out.nrows(), 2, "{name}");
        assert_eq!(
            out.row(0),
            Some(vec![
                Value::Int(1),
                Value::Int(10),
                Value::Int(1),
                Value::Int(100),
            ]),
            "{name}: 配上的一行"
        );
        assert_eq!(
            out.row(1),
            Some(vec![
                Value::Int(2),
                Value::Int(20),
                Value::Null,
                Value::Null,
            ]),
            "{name}: 没配上的左行，右支补 NULL"
        );
    }
}

#[test]
fn a_null_key_never_matches_in_any_strategy() {
    let mut symbols = SymbolTable::new();
    symbols
        .define("a", nullable_kv_table("a", &[(None, 10), (Some(1), 11)]))
        .unwrap();
    symbols
        .define("b", nullable_kv_table("b", &[(None, 100), (Some(1), 101)]))
        .unwrap();

    for (name, strategy) in all_strategies() {
        // 内连接：`NULL = NULL` 是 UNKNOWN，一行都配不上
        let inner = execute(&single_key_plan(strategy.clone(), JoinKind::Inner), &symbols).unwrap();
        assert_eq!(inner.nrows(), 1, "{name}");
        assert_eq!(
            inner.row(0),
            Some(vec![
                Value::Int(1),
                Value::Int(11),
                Value::Int(1),
                Value::Int(101),
            ]),
            "{name}"
        );

        // 左外连接：NULL 那一行没配上，补 NULL
        let left = execute(&single_key_plan(strategy, JoinKind::Left), &symbols).unwrap();
        assert_eq!(left.nrows(), 2, "{name}");
        assert_eq!(
            left.row(0),
            Some(vec![
                Value::Null,
                Value::Int(10),
                Value::Null,
                Value::Null,
            ]),
            "{name}"
        );
    }
}

#[test]
fn the_rest_of_the_on_predicate_is_still_checked() {
    // 键只用来缩小候选集：`ON a.k = b.k AND a.v > b.v` 里的第二条仍要真
    let symbols = ab(&[(1, 200)], &[(1, 100), (1, 300)]);

    let mut p = ProgBuilder::new();
    let lk = p.col(0);
    let rk = p.col(2);
    let eq = p.binary(BinOp::Eq, lk, rk);
    let lv = p.col(1);
    let rv = p.col(3);
    let gt = p.binary(BinOp::Gt, lv, rv);
    let both = p.binary(BinOp::And, eq, gt);
    let on = p.finish(both);

    for (name, strategy) in all_strategies() {
        let mut b = PlanBuilder::new();
        let left = b.scan("a");
        let right = b.scan("b");
        let join = b
            .join(left, right, JoinKind::Inner, Some(on.clone()), strategy)
            .unwrap();
        let out = execute(&b.finish(join).unwrap(), &symbols).unwrap();

        assert_eq!(out.nrows(), 1, "{name}");
        assert_eq!(
            out.row(0),
            Some(vec![
                Value::Int(1),
                Value::Int(200),
                Value::Int(1),
                Value::Int(100),
            ]),
            "{name}"
        );
    }
}

// ---------------------------------------------------------------- 绑定器

#[test]
fn a_single_key_join_on_a_base_table_uses_the_index() {
    let mut symbols = ab(&[(1, 10)], &[(1, 100)]);
    symbols.create_index("b_k", "b", 0, false).unwrap();

    let rendered = explain(&symbols, "SELECT a.k FROM a JOIN b ON a.k = b.k");

    assert!(
        rendered.contains("Join(inner, index=k, on=[k = k])"),
        "{rendered}"
    );
}

#[test]
fn a_multi_key_join_compiles_to_a_hash_join() {
    let symbols = ab(&[(1, 10)], &[(1, 10)]);

    let rendered = explain(
        &symbols,
        "SELECT a.k FROM a JOIN b ON a.k = b.k AND a.v = b.v",
    );

    assert!(rendered.contains("hash=[k = k, v = v]"), "{rendered}");
}

#[test]
fn a_non_equi_join_falls_back_to_nested_loop() {
    let symbols = ab(&[(1, 10)], &[(1, 100)]);

    let rendered = explain(&symbols, "SELECT a.k FROM a JOIN b ON a.k > b.k");

    assert!(rendered.contains("Join(inner, on=[k > k])"), "{rendered}");
    assert!(!rendered.contains("hash="), "{rendered}");
    assert!(!rendered.contains("index="), "{rendered}");
}

#[test]
fn a_key_must_not_mix_int_and_numeric() {
    let symbols = ab_with_numeric(&[(1, 10)], &[(1, 10)], &["1", "2"]);

    // `a.k` 是 INT、`c.n` 是 NUMERIC：比较合法（有隐式放大），但**不能当键**——
    // 跨大类在值层面不是同一个键，哈希/索引会安静地漏行。于是退回嵌套循环。
    let rendered = explain(&symbols, "SELECT a.k FROM a JOIN c ON a.k = c.n");

    assert!(rendered.contains("Join(inner, on=[k = n])"), "{rendered}");
    assert!(!rendered.contains("hash="), "{rendered}");
    assert!(!rendered.contains("index="), "{rendered}");

    // 结果照旧是对的：比较走放大
    let out = execute(
        &compile("SELECT a.k FROM a JOIN c ON a.k = c.n", &symbols).unwrap(),
        &symbols,
    )
    .unwrap();
    assert_eq!(out.nrows(), 1);
}

// ------------------------------------------------------ analyze 的几条守卫

#[test]
fn analyze_rejects_a_hash_key_mixing_int_and_numeric() {
    let symbols = ab_with_numeric(&[(1, 10)], &[(1, 10)], &["1"]);

    // 手写一个跨大类的哈希键：`a.k`(INT) 与 `c.n`(NUMERIC)
    let mut b = PlanBuilder::new();
    let left = b.scan("a");
    let right = b.scan("c");
    let mut p = ProgBuilder::new();
    let lk = p.col(0);
    let rn = p.col(2);
    let eq = p.binary(BinOp::Eq, lk, rn);
    let on = p.finish(eq);
    let key = EqKey {
        outer: col_prog(0),
        inner: col_prog(0),
    };
    let join = b
        .join(
            left,
            right,
            JoinKind::Inner,
            Some(on),
            JoinStrategy::Hash { keys: vec![key] },
        )
        .unwrap();

    let err = b.finish(join).unwrap().analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("同一大类"), "{err}");
}

#[test]
fn analyze_rejects_a_hash_key_that_is_not_a_stored_value() {
    let symbols = ab(&[(1, 10)], &[(1, 100)]);

    let mut b = PlanBuilder::new();
    let left = b.scan("a");
    let right = b.scan("b");
    let mut p = ProgBuilder::new();
    let lk = p.col(0);
    let rk = p.col(2);
    let eq = p.binary(BinOp::Eq, lk, rk);
    let on = p.finish(eq);

    // 键取了一个**真值**（比较的结果），它不能当键
    let mut kp = ProgBuilder::new();
    let k = kp.col(0);
    let one = kp.int(1);
    let eq = kp.binary(BinOp::Eq, k, one);
    let truth = kp.finish(eq);

    let key = EqKey {
        outer: truth,
        inner: col_prog(0),
    };
    let join = b
        .join(
            left,
            right,
            JoinKind::Inner,
            Some(on),
            JoinStrategy::Hash { keys: vec![key] },
        )
        .unwrap();

    let err = b.finish(join).unwrap().analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("同一大类"), "{err}");
}

#[test]
fn analyze_rejects_an_index_key_that_reads_a_column() {
    let symbols = ab(&[(1, 10)], &[(1, 100)]);

    let mut b = PlanBuilder::new();
    let scan = b.scan("a");
    b.set_scan_index(
        scan,
        IndexPath {
            column: 0,
            key: col_prog(0),
        },
    )
    .unwrap();

    let err = b.finish(scan).unwrap().analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("不能引用列"), "{err}");
}

#[test]
fn analyze_rejects_an_index_key_of_another_family() {
    let symbols = ab(&[(1, 10)], &[(1, 100)]);

    let mut b = PlanBuilder::new();
    let scan = b.scan("a");
    // 键是个字符串常量、列是 INT：查不到任何行，是个安静的错答案
    let mut p = ProgBuilder::new();
    let text = p.value(Value::Text("1".to_string()));
    let key = p.finish(text);
    b.set_scan_index(scan, IndexPath { column: 0, key }).unwrap();

    let err = b.finish(scan).unwrap().analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("同一大类"), "{err}");
}

#[test]
fn analyze_rejects_index_nested_loop_without_a_base_scan_inside() {
    let symbols = ab(&[(1, 10)], &[(1, 100)]);

    // 内侧是一个 `Project`（不是基表 Scan）：索引建不上去
    let mut b = PlanBuilder::new();
    let left = b.scan("a");
    let right = b.scan("b");
    let projected = b
        .project(right, vec![col_prog(0)], vec!["k".to_string()])
        .unwrap();

    let mut p = ProgBuilder::new();
    let lk = p.col(0);
    let rk = p.col(2);
    let eq = p.binary(BinOp::Eq, lk, rk);
    let on = p.finish(eq);

    let join = b
        .join(
            left,
            projected,
            JoinKind::Inner,
            Some(on),
            JoinStrategy::IndexNestedLoop { key: key_k() },
        )
        .unwrap();

    let err = b.finish(join).unwrap().analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("基表 Scan"), "{err}");
}

#[test]
fn analyze_rejects_a_hash_strategy_without_an_on() {
    let symbols = ab(&[(1, 10)], &[(1, 100)]);

    let mut b = PlanBuilder::new();
    let left = b.scan("a");
    let right = b.scan("b");
    let join = b
        .join(
            left,
            right,
            JoinKind::Inner,
            None,
            JoinStrategy::Hash {
                keys: vec![key_k()],
            },
        )
        .unwrap();

    let err = b.finish(join).unwrap().analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("要求有 ON"), "{err}");
}

// ------------------------------------------------------ 索引访问路径（3.1）

/// 一张表分别被整行读过几次、单格读过几次。
#[derive(Default)]
struct Counts {
    rows: Cell<usize>,
    cells: Cell<usize>,
}

/// 一张会分别数 `row()` 与 `value()` 的表。
struct CountingTable {
    name: String,
    shape: ColumnsShape,
    rows: Vec<Vec<Value>>,
    counts: Rc<Counts>,
}

impl CountingTable {
    /// `t(x INT)`，`x` 取 `values`。
    fn single(name: &str, values: &[i32], counts: &Rc<Counts>) -> Self {
        CountingTable {
            name: name.to_string(),
            shape: ColumnsShape::new(vec![Column::new("x", DataType::Int)]),
            rows: values.iter().map(|v| vec![Value::Int(*v)]).collect(),
            counts: Rc::clone(counts),
        }
    }

    /// `t(k INT, v INT)`，`v = k * 10`。
    fn kv(name: &str, values: &[i32], counts: &Rc<Counts>) -> Self {
        CountingTable {
            name: name.to_string(),
            shape: ColumnsShape::new(vec![
                Column::new("k", DataType::Int),
                Column::new("v", DataType::Int),
            ]),
            rows: values
                .iter()
                .map(|v| vec![Value::Int(*v), Value::Int(v * 10)])
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
        &self.name
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
}

/// 装着一张会数数的 `t(x INT)` 的库，外加它的计数器。
fn counting(values: &[i32]) -> (SymbolTable, Rc<Counts>) {
    let counts = Rc::new(Counts::default());
    let mut symbols = SymbolTable::new();
    symbols
        .define("t", CountingTable::single("t", values, &counts))
        .unwrap();
    (symbols, counts)
}

/// 装着一张会数数的 `a(k INT, v INT)` 与一张会数数的 `b(k INT, v INT)` 的库。
///
/// 两张表**各有一份**计数器：只有分开看，才知道内侧被读了多少。
fn counting_pair(a: &[i32], b: &[i32]) -> (SymbolTable, Rc<Counts>, Rc<Counts>) {
    let a_counts = Rc::new(Counts::default());
    let b_counts = Rc::new(Counts::default());

    let mut symbols = SymbolTable::new();
    symbols
        .define("a", CountingTable::kv("a", a, &a_counts))
        .unwrap();
    symbols
        .define("b", CountingTable::kv("b", b, &b_counts))
        .unwrap();
    (symbols, a_counts, b_counts)
}

#[test]
fn an_index_scan_fetches_only_the_matching_rows() {
    let (mut symbols, counts) = counting(&[10, 11, 12, 13, 14]);
    // 访问路径只来自目录：先 `CREATE INDEX`（它扫一遍这一列把索引建出来）
    symbols.create_index("t_x", "t", 0, false).unwrap();

    let plan = compile("SELECT * FROM t WHERE x = 13", &symbols).unwrap();
    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Int(13)));
    // 建索引读一遍**这一列**，然后只有命中的那一行被整行取出
    assert_eq!(counts.cells.get(), 5, "建索引（CREATE INDEX 时）读一列");
    assert_eq!(counts.rows.get(), 1, "只有命中的行被整行取出");
}

#[test]
fn explain_shows_the_index_access_path() {
    let (mut symbols, _) = counting(&[10, 11, 12]);

    // **没有** `CREATE INDEX` 就是顺序扫描：本版不再为了"也许有用"先扫一遍表
    assert_eq!(
        explain(&symbols, "SELECT * FROM t WHERE x = 12"),
        "Scan(t, filter=[x = 12])\n"
    );

    symbols.create_index("t_x", "t", 0, false).unwrap();
    assert_eq!(
        explain(&symbols, "SELECT * FROM t WHERE x = 12"),
        "Scan(t, index=[x], filter=[x = 12])\n"
    );

    // 不是等值（没有索引可走）就还是顺序扫描
    assert_eq!(
        explain(&symbols, "SELECT * FROM t WHERE x > 11"),
        "Scan(t, filter=[x > 11])\n"
    );
}

#[test]
fn an_index_scan_ignores_null_keys_and_keeps_duplicates() {
    let shape = ColumnsShape::new(vec![Column::new("x", DataType::Int)]);
    let mut table = PhysTab::new("t", &shape);
    for value in [Value::Int(5), Value::Null, Value::Int(5), Value::Int(7)] {
        table.insert(vec![value]).unwrap();
    }
    let mut symbols = SymbolTable::new();
    symbols.define("t", table).unwrap();
    symbols.create_index("t_x", "t", 0, false).unwrap();

    let plan = compile("SELECT * FROM t WHERE x = 5", &symbols).unwrap();
    assert!(
        plan.explain(&symbols).unwrap().contains("index=[x]"),
        "应当走索引"
    );

    let out = execute(&plan, &symbols).unwrap();

    // 两行 5；NULL 与 7 都不该被捞出来
    assert_eq!(out.nrows(), 2);
}

#[test]
fn a_limit_of_zero_never_rebuilds_an_invalidated_index() {
    let (mut symbols, counts) = counting(&[10, 11, 12, 13, 14]);
    symbols.create_index("t_x", "t", 0, false).unwrap();
    let after_build = counts.cells.get();

    // 索引失效之后是**惰性**重建的：上层一行都不要，就一直不建
    symbols.invalidate_indexes("t");
    let plan = compile("SELECT * FROM t WHERE x = 13 LIMIT 0", &symbols).unwrap();
    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 0);
    assert_eq!(counts.cells.get(), after_build, "LIMIT 0 不该重建索引");
    assert_eq!(counts.rows.get(), 0);
}

#[test]
fn an_index_join_does_not_materialize_the_inner_rows() {
    // 外侧 3 行、内侧 3 行，只有键 1 对得上
    let (mut symbols, _, inner) = counting_pair(&[1, 2, 3], &[1, 4, 5]);
    symbols.create_index("b_k", "b", 0, false).unwrap();

    let plan = compile("SELECT a.k FROM a JOIN b ON a.k = b.k", &symbols).unwrap();
    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 1);
    // 内侧只被**建索引**扫了一遍（一列 3 格），整行只取了命中的那 1 行
    assert_eq!(inner.cells.get(), 3, "建索引（CREATE INDEX 时）读一列");
    assert_eq!(inner.rows.get(), 1, "内侧没有物化");
}

#[test]
fn a_nested_loop_materializes_the_inner_rows() {
    // 对照：同样的查询硬走嵌套循环，内侧要被整行物化一遍
    let (symbols, _, inner) = counting_pair(&[1, 2, 3], &[1, 4, 5]);

    let plan = single_key_plan(JoinStrategy::NestedLoop, JoinKind::Inner);
    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 1);
    assert_eq!(inner.rows.get(), 3, "嵌套循环要重扫内侧，先物化一遍");
}

// ------------------------------------------------------------------ 基准

/// 连接的规模曲线：`TODO.md` 基线的「连接规模」就是它跑出来的。
///
/// `#[ignore]`：这是基准、不是断言。跑法：
///
/// ```text
/// cargo test --offline -- --ignored --nocapture join_scaling
/// ```
#[test]
#[ignore = "基准，不在常规测试里跑"]
fn join_scaling_benchmark() {
    for n in [250usize, 500, 1000, 2000] {
        let rows: Vec<(i32, i32)> = (0..n).map(|i| (i as i32, i as i32)).collect();
        let symbols = ab(&rows, &rows);

        for (name, strategy) in all_strategies() {
            let plan = single_key_plan(strategy, JoinKind::Inner);

            let started = std::time::Instant::now();
            let out = execute(&plan, &symbols).unwrap();
            let elapsed = started.elapsed();

            assert_eq!(out.nrows(), n);
            println!("{name:>11}  N = {n:>5}  {elapsed:>12.3?}");
        }
    }
}
