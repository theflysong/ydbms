//! 线性 IR 的 testbench:**手写 IR** 跑查询,核对结果.
//!
//! 按 `tests/phys_tab.rs` 的方式建表(`x / f(x)=x²+1 / f'(x)=2x`,x ∈ [-5,5]),
//! 然后把查询**手工**写成 [`PlanBuilder`] + [`ProgBuilder`] 的调用序列--本版没有
//! AST → IR 的降级,IR 就是手写的.
//!
//! 除了核对查询结果,还重点验证"线性"不是形式而是有静态保证的:
//! [`Plan::analyze`] 必须挡住寄存器先使用后定义,列下标越界,把布尔当结果列,
//! 以及违反拓扑序的输入.

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, Table, TabShape, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{
    execute, BinOp, JoinKind, JoinStrategy, Node, Op, Plan, PlanBuilder, Prog, ProgBuilder, SortKey,
    Step, Symbol, SymbolTable, UnOp,
};

const X_MIN: i32 = -5;
const X_MAX: i32 = 5;
/// 行数:x 取遍闭区间 `[-5, 5]`.
const N: usize = (X_MAX - X_MIN + 1) as usize;

/// f(x) = x² + 1
fn f(x: i32) -> i32 {
    x * x + 1
}

/// f'(x) = 2x
fn df(x: i32) -> i32 {
    2 * x
}

/// 主表形状:`x INT, value INT, drvd INT`.
fn quadratic_shape() -> ColumnsShape {
    ColumnsShape::new(vec![
        Column::new("x", DataType::Int),
        Column::new("value", DataType::Int),
        Column::new("drvd", DataType::Int),
    ])
}

/// 建主表,`x ∈ [-5, 5]`.
fn build_quadratic_table(name: &str) -> PhysTab {
    let shape = quadratic_shape();
    let mut tab = PhysTab::new(name, &shape);
    for x in X_MIN..=X_MAX {
        tab.insert(vec![Value::from(x), Value::from(f(x)), Value::from(df(x))])
            .expect("insert should succeed");
    }
    tab
}

/// 第二张表 `t(id, label)`,`id` 取 `[-5, 5]` 里的偶数(5 行),`label = id * 100`.
fn build_labels_table() -> PhysTab {
    let shape = ColumnsShape::new(vec![
        Column::new("id", DataType::Int),
        Column::new("label", DataType::Int),
    ]);
    let mut tab = PhysTab::new("t", &shape);
    for x in X_MIN..=X_MAX {
        if x % 2 == 0 {
            tab.insert(vec![Value::Int(x), Value::Int(x * 100)])
                .expect("insert should succeed");
        }
    }
    tab
}

/// 只装了主表 `f` 的符号表.
fn symbols_with_f() -> SymbolTable {
    let mut symbols = SymbolTable::new();
    symbols
        .define("f", build_quadratic_table("f"))
        .expect("注册 f");
    symbols
}

/// 把第 `c` 列取成 `Vec<i32>`,便于直接断言.
fn column_of(table: &PhysTab, c: usize) -> Vec<i32> {
    (0..table.nrows())
        .map(|r| {
            table
                .value(r, c)
                .and_then(|v| v.as_int())
                .expect("INT 列")
        })
        .collect()
}

/// 造一个 `col(index) OP 常量` 形式的谓词.
fn col_cmp(index: usize, op: BinOp, value: i32) -> Prog {
    let mut p = ProgBuilder::new();
    let c = p.col(index);
    let k = p.int(value);
    let r = p.binary(op, c, k);
    p.finish(r)
}

/// 造一组"直接取某几列"的投影.
fn project_cols(indices: &[usize], names: &[&str]) -> (Vec<Prog>, Vec<String>) {
    let progs = indices
        .iter()
        .map(|&i| {
            let mut p = ProgBuilder::new();
            let c = p.col(i);
            p.finish(c)
        })
        .collect();
    (progs, names.iter().map(|s| s.to_string()).collect())
}

/// 造一个"取第 `index` 列"的排序键.
fn key_of(index: usize, desc: bool) -> SortKey {
    let mut p = ProgBuilder::new();
    let c = p.col(index);
    SortKey {
        prog: p.finish(c),
        desc,
        nulls_first: desc,
    }
}

// ---------------------------------------------------------------- 扫描与投影

#[test]
fn scan_returns_all_rows() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let plan = b.finish(s).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), N);
    assert_eq!(out.arity(), 3);
    assert_eq!(out.name(), "f");
    assert_eq!(out.columns(), symbols.columns("f").unwrap());
    assert_eq!(
        out.row(0),
        Some(vec![Value::Int(-5), Value::Int(26), Value::Int(-10)])
    );
}

#[test]
fn project_single_column() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let (progs, names) = project_cols(&[0], &["x"]);
    let p = b.project(s, progs, names).unwrap();
    let plan = b.finish(p).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), N);
    assert_eq!(out.arity(), 1);
    assert_eq!(out.columns()[0].name, "x");
    assert_eq!(column_of(&out, 0), (X_MIN..=X_MAX).collect::<Vec<_>>());
}

#[test]
fn project_expression_computes_x_plus_one() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");

    let mut p = ProgBuilder::new();
    let x = p.col(0);
    let one = p.int(1);
    let sum = p.binary(BinOp::Add, x, one);
    let progs = vec![p.finish(sum)];

    let pr = b.project(s, progs, vec!["y".to_string()]).unwrap();
    let plan = b.finish(pr).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.columns()[0].name, "y");
    assert_eq!(
        column_of(&out, 0),
        ((X_MIN + 1)..=(X_MAX + 1)).collect::<Vec<_>>()
    );
}

// -------------------------------------------------------------------- 过滤

#[test]
fn filter_selects_matching_rows() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let flt = b.filter(s, col_cmp(1, BinOp::Gt, 10)).unwrap(); // value > 10
    let (progs, names) = project_cols(&[0], &["x"]);
    let pr = b.project(flt, progs, names).unwrap();
    let plan = b.finish(pr).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    // f(x) = x² + 1 > 10  ⟺  |x| ≥ 4
    assert_eq!(column_of(&out, 0), vec![-5, -4, 4, 5]);
}

#[test]
fn filter_with_logical_combination() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");

    // value > 10 AND x < 0
    let mut p = ProgBuilder::new();
    let value = p.col(1);
    let ten = p.int(10);
    let gt = p.binary(BinOp::Gt, value, ten);
    let x = p.col(0);
    let zero = p.int(0);
    let lt = p.binary(BinOp::Lt, x, zero);
    let both = p.binary(BinOp::And, gt, lt);
    let pred = p.finish(both);

    let flt = b.filter(s, pred).unwrap();
    let (progs, names) = project_cols(&[0], &["x"]);
    let pr = b.project(flt, progs, names).unwrap();
    let plan = b.finish(pr).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(column_of(&out, 0), vec![-5, -4]);
}

#[test]
fn unary_negate_and_not() {
    // SELECT -x FROM f WHERE NOT (x >= 0)
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");

    let mut p = ProgBuilder::new();
    let x = p.col(0);
    let zero = p.int(0);
    let ge = p.binary(BinOp::Ge, x, zero);
    let not = p.unary(UnOp::Not, ge);
    let flt = b.filter(s, p.finish(not)).unwrap();

    let mut q = ProgBuilder::new();
    let x = q.col(0);
    let neg = q.unary(UnOp::Neg, x);
    let pr = b
        .project(flt, vec![q.finish(neg)], vec!["neg_x".to_string()])
        .unwrap();
    let plan = b.finish(pr).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    // 只留下 x < 0 的行,取负后是 5..1
    assert_eq!(column_of(&out, 0), vec![5, 4, 3, 2, 1]);
}

#[test]
fn pushed_down_filter_matches_an_explicit_filter_node() {
    let symbols = symbols_with_f();

    // (a) 显式 Filter 节点
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let flt = b.filter(s, col_cmp(1, BinOp::Gt, 10)).unwrap();
    let plan_filter = b.finish(flt).unwrap();

    // (b) 把同一个谓词下推到 Scan
    let mut b = PlanBuilder::new();
    let s = b.scan_with_filter("f", col_cmp(1, BinOp::Gt, 10));
    let plan_pushed = b.finish(s).unwrap();

    let a = execute(&plan_filter, &symbols).unwrap();
    let c = execute(&plan_pushed, &symbols).unwrap();

    assert_eq!(a.nrows(), 4);
    assert_eq!(a.nrows(), c.nrows());
    for r in 0..a.nrows() {
        assert_eq!(a.row(r), c.row(r), "第 {r} 行");
    }
}

// ---------------------------------------------------------------- 排序与截断

#[test]
fn sort_desc_then_limit() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let sorted = b.sort(s, vec![key_of(0, true)]).unwrap();
    let (progs, names) = project_cols(&[0], &["x"]);
    let pr = b.project(sorted, progs, names).unwrap();
    let root = b.limit(pr, Some(3), 0).unwrap();
    let plan = b.finish(root).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(column_of(&out, 0), vec![5, 4, 3]);
}

#[test]
fn limit_with_offset() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let (progs, names) = project_cols(&[0], &["x"]);
    let pr = b.project(s, progs, names).unwrap();
    let root = b.limit(pr, Some(3), 2).unwrap();
    let plan = b.finish(root).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(column_of(&out, 0), vec![-3, -2, -1]);
}

#[test]
fn limit_zero_returns_nothing_but_keeps_the_shape() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let root = b.limit(s, Some(0), 0).unwrap();
    let plan = b.finish(root).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 0);
    assert_eq!(out.arity(), 3);
}

#[test]
fn offset_beyond_the_end_returns_nothing() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let root = b.limit(s, None, 999).unwrap();
    let plan = b.finish(root).unwrap();

    assert_eq!(execute(&plan, &symbols).unwrap().nrows(), 0);
}

// ------------------------------------------------------ 没有 FROM 的常量查询

#[test]
fn constant_query_without_from() {
    // SELECT 1 + 1
    let symbols = SymbolTable::new();
    let mut b = PlanBuilder::new();
    let base = b.single_row();

    let mut p = ProgBuilder::new();
    let one = p.int(1);
    let other = p.int(1);
    let sum = p.binary(BinOp::Add, one, other);
    let progs = vec![p.finish(sum)];

    let pr = b
        .project(base, progs, vec!["two".to_string()])
        .unwrap();
    let plan = b.finish(pr).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.arity(), 1);
    assert_eq!(out.row(0), Some(vec![Value::Int(2)]));
}

// -------------------------------------------------------------------- 连接

#[test]
fn inner_join_on_equality() {
    let mut symbols = symbols_with_f();
    symbols.define("t", build_labels_table()).unwrap();

    let mut b = PlanBuilder::new();
    let left = b.scan("f");
    let right = b.scan("t");

    // f.x = t.id:左支 0..2,右支从 3 开始(扁平下标)
    let mut p = ProgBuilder::new();
    let x = p.col(0);
    let id = p.col(3);
    let eq = p.binary(BinOp::Eq, x, id);
    let on = p.finish(eq);

    let j = b.join(left, right, JoinKind::Inner, Some(on), JoinStrategy::NestedLoop).unwrap();
    let plan = b.finish(j).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.arity(), 5, "左 3 列 ++ 右 2 列");
    assert_eq!(out.nrows(), 5, "id 取到 -4,-2,0,2,4");
    assert_eq!(
        out.row(0),
        Some(vec![
            Value::Int(-4),
            Value::Int(17),
            Value::Int(-8),
            Value::Int(-4),
            Value::Int(-400),
        ])
    );
    assert_eq!(column_of(&out, 0), vec![-4, -2, 0, 2, 4]);
    assert_eq!(column_of(&out, 4), vec![-400, -200, 0, 200, 400]);
}

#[test]
fn join_with_an_empty_side_yields_nothing() {
    let mut symbols = symbols_with_f();
    let empty_shape = ColumnsShape::new(vec![
        Column::new("id", DataType::Int),
        Column::new("label", DataType::Int),
    ]);
    symbols
        .define("t", PhysTab::new("t", &empty_shape))
        .unwrap();

    let mut b = PlanBuilder::new();
    let left = b.scan("f");
    let right = b.scan("t");

    let mut p = ProgBuilder::new();
    let x = p.col(0);
    let id = p.col(3);
    let eq = p.binary(BinOp::Eq, x, id);
    let on = p.finish(eq);

    let j = b.join(left, right, JoinKind::Inner, Some(on), JoinStrategy::NestedLoop).unwrap();
    let plan = b.finish(j).unwrap();

    assert_eq!(execute(&plan, &symbols).unwrap().nrows(), 0);
}

#[test]
fn join_without_condition_is_a_cross_product() {
    let mut symbols = symbols_with_f();
    symbols.define("t", build_labels_table()).unwrap();

    let mut b = PlanBuilder::new();
    let left = b.scan("f");
    let right = b.scan("t");
    let j = b.join(left, right, JoinKind::Inner, None, JoinStrategy::NestedLoop).unwrap();
    let plan = b.finish(j).unwrap();

    let out = execute(&plan, &symbols).unwrap();

    assert_eq!(out.nrows(), N * 5);
    assert_eq!(out.arity(), 5);
}

// ------------------------------------------------------------------ 符号表

#[test]
fn alias_resolves_to_the_same_table() {
    let mut symbols = SymbolTable::new();
    symbols
        .define("study.people", build_quadratic_table("study.people"))
        .unwrap();
    symbols.alias("p", "study.people").unwrap();

    assert_eq!(symbols.names(), vec!["study.people", "p"]);
    assert_eq!(symbols.columns("p").unwrap().len(), 3);
    assert_eq!(symbols.column_index("p", "value"), Some(1));
    assert_eq!(symbols.column_index("p", "nope"), None);

    // 通过别名扫描,结果与原名一致
    let mut b = PlanBuilder::new();
    let s = b.scan("p");
    let plan = b.finish(s).unwrap();

    let out = execute(&plan, &symbols).unwrap();
    assert_eq!(out.nrows(), N);
}

#[test]
fn bulk_import_mirrors_database_loading() {
    let mut symbols = SymbolTable::new();
    symbols
        .extend([
            Symbol::relation("f", build_quadratic_table("f")),
            Symbol::relation("t", build_labels_table()),
        ])
        .unwrap();

    assert_eq!(symbols.names(), vec!["f", "t"]);
    assert_eq!(symbols.relation("f").unwrap().nrows(), N);
    assert_eq!(symbols.relation("t").unwrap().nrows(), 5);
}

#[test]
fn duplicate_symbol_is_rejected() {
    let mut symbols = symbols_with_f();
    let err = symbols
        .define("f", build_quadratic_table("f"))
        .unwrap_err();

    assert_eq!(err, DbError::DuplicateSymbol { name: "f".to_string() });
    assert_eq!(symbols.names(), vec!["f"]);
}

#[test]
fn bulk_import_is_atomic() {
    let mut symbols = symbols_with_f();

    // 第二条与已有的 f 冲突,整批都不该生效
    let err = symbols
        .extend([
            Symbol::relation("t", build_labels_table()),
            Symbol::relation("f", build_quadratic_table("f")),
        ])
        .unwrap_err();

    assert_eq!(err, DbError::DuplicateSymbol { name: "f".to_string() });
    assert_eq!(symbols.names(), vec!["f"], "t 不应被留下");
}

#[test]
fn scanning_an_unregistered_table_is_rejected() {
    let symbols = SymbolTable::new();
    let mut b = PlanBuilder::new();
    let s = b.scan("nope");
    let plan = b.finish(s).unwrap();

    let err = execute(&plan, &symbols).unwrap_err();

    assert_eq!(
        err,
        DbError::UnknownSymbol {
            name: "nope".to_string()
        }
    );
}

// -------------------------------------------------- 静态校验:坏 IR 必须被挡住

#[test]
fn verify_rejects_a_register_used_before_being_written() {
    let symbols = symbols_with_f();

    // 手搓一个坏程序:Binary 读了它这一条正要写的 1 号寄存器
    let prog = Prog::new(
        vec![
            Step::Col {
                dst: 0,
                index: 0,
            },
            Step::Binary {
                dst: 1,
                op: BinOp::Add,
                lhs: 0,
                rhs: 1,
            },
        ],
        1,
        2,
    );

    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let flt = b.filter(s, prog).unwrap();
    let plan = b.finish(flt).unwrap();

    let err = plan.analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("先定义后使用"), "{err}");
}

#[test]
fn verify_rejects_an_out_of_range_column() {
    let symbols = symbols_with_f();

    // f 只有 3 列
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let flt = b.filter(s, col_cmp(9, BinOp::Gt, 1)).unwrap();
    let plan = b.finish(flt).unwrap();

    let err = plan.analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("列下标 9"), "{err}");
}

#[test]
fn verify_rejects_a_boolean_result_column() {
    let symbols = symbols_with_f();

    // 把谓词直接当结果列投影出来
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let pr = b
        .project(s, vec![col_cmp(1, BinOp::Gt, 10)], vec!["flag".to_string()])
        .unwrap();
    let plan = b.finish(pr).unwrap();

    let err = plan.analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("布尔不能作为结果列"), "{err}");
}

#[test]
fn analyze_rejects_a_non_topological_input() {
    let symbols = symbols_with_f();

    // 手工造一个自引用的计划:0 号节点的输入是它自己
    let plan = Plan {
        nodes: vec![Node {
            op: Op::Filter {
                predicate: col_cmp(0, BinOp::Eq, 0),
            },
            inputs: vec![0],
        }],
        root: 0,
    };

    let err = plan.analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("拓扑序"), "{err}");
}

#[test]
fn builder_rejects_a_forward_reference() {
    let mut b = PlanBuilder::new();

    // 还没有任何节点,就引用 5 号
    let err = b.filter(5, col_cmp(0, BinOp::Eq, 0)).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("拓扑序"), "{err}");
}

#[test]
fn analyze_rejects_a_project_arity_mismatch() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let (progs, _) = project_cols(&[0, 1], &["x", "value"]);
    let pr = b.project(s, progs, vec!["only".to_string()]).unwrap();
    let plan = b.finish(pr).unwrap();

    let err = plan.analyze(&symbols).unwrap_err();

    assert!(matches!(err, DbError::IrInvalid { .. }), "got {err:?}");
    assert!(err.to_string().contains("2 个表达式"), "{err}");
}

// ------------------------------------------------------------------ 运行期

#[test]
fn division_by_zero_is_a_runtime_error() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");

    let mut p = ProgBuilder::new();
    let x = p.col(0);
    let zero = p.int(0);
    let quotient = p.binary(BinOp::Div, x, zero);
    let progs = vec![p.finish(quotient)];

    let pr = b
        .project(s, progs, vec!["q".to_string()])
        .unwrap();
    let plan = b.finish(pr).unwrap();

    // 静态校验应当通过(两个 INT 相除类型正确),错误发生在求值
    plan.analyze(&symbols).expect("类型上没问题");

    let err = execute(&plan, &symbols).unwrap_err();

    assert!(matches!(err, DbError::IrRuntime { .. }), "got {err:?}");
    assert!(err.to_string().contains("除以零"), "{err}");
}

// ------------------------------------------------------------------ explain

#[test]
fn explain_renders_the_whole_pipeline() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan("f");
    let flt = b.filter(s, col_cmp(1, BinOp::Gt, 10)).unwrap();
    let (progs, names) = project_cols(&[0, 1], &["x", "value"]);
    let pr = b.project(flt, progs, names).unwrap();
    let sorted = b.sort(pr, vec![key_of(0, true)]).unwrap();
    let root = b.limit(sorted, Some(3), 0).unwrap();
    let plan = b.finish(root).unwrap();

    let expected = concat!(
        "Limit(limit=3, offset=0)\n",
        "  Sort(keys=[x DESC])\n",
        "    Project(names=[x, value], exprs=[x, value])\n",
        "      Filter([value > 10])\n",
        "        Scan(f, filter=[])\n",
    );

    assert_eq!(plan.explain(&symbols).unwrap(), expected);
}

#[test]
fn explain_shows_a_pushed_down_predicate_on_the_scan() {
    let symbols = symbols_with_f();
    let mut b = PlanBuilder::new();
    let s = b.scan_with_filter("f", col_cmp(1, BinOp::Gt, 10));
    let plan = b.finish(s).unwrap();

    assert_eq!(
        plan.explain(&symbols).unwrap(),
        "Scan(f, filter=[value > 10])\n"
    );
}
