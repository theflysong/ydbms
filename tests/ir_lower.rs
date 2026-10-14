//! AST → 线性 IR 降级的 testbench：写 SQL，检查计划与结果。
//!
//! 库是两张表（沿用 `phys_tab.rs` 的 `f`，再加一张同名列 `t` 用来测歧义）：
//!
//! * `f(x, value, drvd)` —— `x ∈ [-5, 5]`、`value = x² + 1`、`drvd = 2x`
//! * `t(x, label)` —— `x ∈ {-4, -2, 0, 2, 4}`、`label = x * 100`
//!
//! 除了核对查询结果，重点验证两类东西：**不支持的写法必须显式拒绝**（尤其是
//! `IS NULL`——值模型里没有 NULL，静默当成假会给出安静的错误答案），以及
//! **降级出来的计划必须通过 `analyze`**（绑定器算错列下标会被抓住，而不是读错列）。

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, TabShape, Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{compile, execute, SymbolTable};

const X_MIN: i32 = -5;
const X_MAX: i32 = 5;
/// `f` 的行数。
const N: usize = (X_MAX - X_MIN + 1) as usize;

/// f(x) = x² + 1
fn f(x: i32) -> i32 {
    x * x + 1
}

/// f'(x) = 2x
fn df(x: i32) -> i32 {
    2 * x
}

/// 建库：`f` 与 `t`。
fn build_database() -> SymbolTable {
    let f_shape = ColumnsShape::new(vec![
        Column::new("x", DataType::Int),
        Column::new("value", DataType::Int),
        Column::new("drvd", DataType::Int),
    ]);
    let mut f_tab = PhysTab::new("f", &f_shape);
    for x in X_MIN..=X_MAX {
        f_tab
            .insert(vec![Value::from(x), Value::from(f(x)), Value::from(df(x))])
            .expect("insert");
    }

    let t_shape = ColumnsShape::new(vec![
        Column::new("x", DataType::Int),
        Column::new("label", DataType::Int),
    ]);
    let mut t_tab = PhysTab::new("t", &t_shape);
    for x in X_MIN..=X_MAX {
        if x % 2 == 0 {
            t_tab
                .insert(vec![Value::Int(x), Value::Int(x * 100)])
                .expect("insert");
        }
    }

    let mut symbols = SymbolTable::new();
    symbols.define("f", f_tab).expect("注册 f");
    symbols.define("t", t_tab).expect("注册 t");
    symbols
}

/// 跑一条 SQL。
fn query(symbols: &SymbolTable, sql: &str) -> Result<PhysTab, DbError> {
    let plan = compile(sql, symbols)?;
    execute(&plan, symbols)
}

/// 把第 `c` 列取成 `Vec<i32>`。
fn col_i32(table: &PhysTab, c: usize) -> Vec<i32> {
    (0..table.nrows())
        .map(|r| table.value(r, c).and_then(|v| v.as_int()).expect("INT 列"))
        .collect()
}

/// 走一遍 EXPLAIN 的路径。
fn explain(symbols: &SymbolTable, sql: &str) -> String {
    compile(sql, symbols)
        .expect("应当能降级")
        .explain(symbols)
        .expect("应当能渲染")
}

/// 断言被显式拒绝，并返回错误以便进一步查消息。
fn rejected(symbols: &SymbolTable, sql: &str) -> DbError {
    match query(symbols, sql) {
        Err(e) => e,
        Ok(table) => panic!("`{sql}` 本应被拒绝，却返回了 {} 行", table.nrows()),
    }
}

// ------------------------------------------------------------ 基本 SELECT

#[test]
fn select_star_returns_the_whole_table() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT * FROM f").unwrap();

    assert_eq!(out.nrows(), N);
    assert_eq!(out.arity(), 3);
    assert_eq!(
        out.row(0),
        Some(vec![Value::Int(-5), Value::Int(26), Value::Int(-10)])
    );
}

#[test]
fn select_star_needs_no_project_node() {
    let symbols = build_database();
    // `*` 展开后就是全部列，恒等的 Project 应当被省掉
    assert_eq!(explain(&symbols, "SELECT * FROM f"), "Scan(f, filter=[])\n");
}

#[test]
fn select_named_columns_in_the_given_order() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT drvd, x FROM f").unwrap();

    assert_eq!(
        out.columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["drvd", "x"]
    );
    assert_eq!(col_i32(&out, 0), (X_MIN..=X_MAX).map(df).collect::<Vec<_>>());
    assert_eq!(col_i32(&out, 1), (X_MIN..=X_MAX).collect::<Vec<_>>());
}

#[test]
fn as_renames_the_output_column() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT x AS k FROM f").unwrap();

    assert_eq!(out.columns()[0].name, "k");
    assert_eq!(col_i32(&out, 0), (X_MIN..=X_MAX).collect::<Vec<_>>());
}

#[test]
fn unnamed_expression_gets_the_postgres_placeholder() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT x + 1 FROM f").unwrap();

    assert_eq!(out.columns()[0].name, "?column?");
}

#[test]
fn select_without_from_yields_one_row() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT 1 + 1 AS two").unwrap();

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.arity(), 1);
    assert_eq!(out.row(0), Some(vec![Value::Int(2)]));
}

// ---------------------------------------------------------------- WHERE

#[test]
fn where_filters_rows() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT x FROM f WHERE value > 10").unwrap();

    // f(x) = x² + 1 > 10  ⟺  |x| ≥ 4
    assert_eq!(col_i32(&out, 0), vec![-5, -4, 4, 5]);
}

#[test]
fn where_supports_and_or_not() {
    let symbols = build_database();

    let and = query(&symbols, "SELECT x FROM f WHERE value > 10 AND x < 0").unwrap();
    assert_eq!(col_i32(&and, 0), vec![-5, -4]);

    let or = query(&symbols, "SELECT x FROM f WHERE x = -5 OR x = 5").unwrap();
    assert_eq!(col_i32(&or, 0), vec![-5, 5]);

    let not = query(&symbols, "SELECT x FROM f WHERE NOT (x >= -2)").unwrap();
    assert_eq!(col_i32(&not, 0), vec![-5, -4, -3]);
}

#[test]
fn where_supports_arithmetic_on_columns() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT * FROM f WHERE x * 2 >= 6").unwrap();

    assert_eq!(col_i32(&out, 0), vec![3, 4, 5]);
}

#[test]
fn where_can_compare_two_columns() {
    let symbols = build_database();
    // value > drvd  ⟺  x² + 1 > 2x  ⟺  (x-1)² > 0  ⟺  x ≠ 1
    let out = query(&symbols, "SELECT x FROM f WHERE value > drvd").unwrap();

    assert_eq!(
        col_i32(&out, 0),
        vec![-5, -4, -3, -2, -1, 0, 2, 3, 4, 5]
    );
}

// ------------------------------------------------------- ORDER BY / LIMIT

#[test]
fn order_by_desc_with_limit() {
    let symbols = build_database();
    let out = query(
        &symbols,
        "SELECT x, value FROM f WHERE value > 10 ORDER BY x DESC LIMIT 3",
    )
    .unwrap();

    assert_eq!(col_i32(&out, 0), vec![5, 4, -4]);
    assert_eq!(col_i32(&out, 1), vec![26, 17, 17]);
}

#[test]
fn order_by_several_keys() {
    let symbols = build_database();
    // value 降序、value 相同再按 x 升序
    let out = query(
        &symbols,
        "SELECT x FROM f ORDER BY value DESC, x ASC LIMIT 4",
    )
    .unwrap();

    // value: 26 → x=-5,5；17 → x=-4,4
    assert_eq!(col_i32(&out, 0), vec![-5, 5, -4, 4]);
}

#[test]
fn limit_with_offset() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT x FROM f ORDER BY x LIMIT 3 OFFSET 2").unwrap();

    assert_eq!(col_i32(&out, 0), vec![-3, -2, -1]);
}

#[test]
fn order_by_a_column_that_is_not_in_the_output() {
    // ORDER BY 先按输出列解，解不动就退回输入列，此时 Sort 落在 Project 下面
    let symbols = build_database();
    let out = query(&symbols, "SELECT x FROM f ORDER BY value DESC LIMIT 2").unwrap();

    // value 最大的是 x = -5 与 x = 5（都是 26），稳定排序保持原有行序
    assert_eq!(col_i32(&out, 0), vec![-5, 5]);
}

#[test]
fn order_by_an_output_alias() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT x + 100 AS y FROM f ORDER BY y DESC LIMIT 2").unwrap();

    assert_eq!(col_i32(&out, 0), vec![105, 104]);
}

// --------------------------------------------------------- 限定名与别名

#[test]
fn table_alias_qualifies_columns() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT g.x FROM f AS g WHERE g.x = 0").unwrap();

    assert_eq!(col_i32(&out, 0), vec![0]);
}

#[test]
fn table_name_qualifies_columns() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT f.value FROM f WHERE f.x = -5").unwrap();

    assert_eq!(col_i32(&out, 0), vec![26]);
}

// ---------------------------------------------------------------- JOIN

#[test]
fn inner_join_on_equality() {
    let symbols = build_database();
    let out = query(
        &symbols,
        "SELECT f.x, t.label FROM f JOIN t ON f.x = t.x",
    )
    .unwrap();

    assert_eq!(out.arity(), 2);
    assert_eq!(col_i32(&out, 0), vec![-4, -2, 0, 2, 4]);
    assert_eq!(col_i32(&out, 1), vec![-400, -200, 0, 200, 400]);
}

#[test]
fn explicit_join_and_comma_join_agree() {
    let symbols = build_database();

    let explicit = query(&symbols, "SELECT f.x FROM f JOIN t ON f.x = t.x").unwrap();
    let comma = query(&symbols, "SELECT f.x FROM f, t WHERE f.x = t.x").unwrap();

    assert_eq!(explicit.nrows(), comma.nrows());
    for r in 0..explicit.nrows() {
        assert_eq!(explicit.row(r), comma.row(r), "第 {r} 行");
    }
    assert_eq!(explicit.nrows(), 5);
}

#[test]
fn star_expands_both_sides_of_a_join() {
    let symbols = build_database();
    let out = query(&symbols, "SELECT * FROM f JOIN t ON f.x = t.x").unwrap();

    assert_eq!(out.arity(), 5, "左 3 列 ++ 右 2 列");
    assert_eq!(
        out.columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["x", "value", "drvd", "x", "label"],
        "同名列会重复出现（限定名是 binder 以后的事）"
    );
}

#[test]
fn unqualified_shared_column_is_ambiguous() {
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT x FROM f JOIN t ON f.x = t.x");

    match err {
        DbError::AmbiguousColumn { ref name, ref candidates } => {
            assert_eq!(name, "x");
            assert_eq!(candidates.len(), 2, "{candidates:?}");
        }
        other => panic!("expected AmbiguousColumn, got {other:?}"),
    }
}

// ------------------------------------------------------ 名字解析的失败

#[test]
fn unknown_column_is_reported_with_the_visible_names() {
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT nope FROM f");

    match err {
        DbError::UnknownColumn { ref name, ref available } => {
            assert_eq!(name, "nope");
            assert_eq!(available, &["x", "value", "drvd"]);
        }
        other => panic!("expected UnknownColumn, got {other:?}"),
    }
}

#[test]
fn unknown_table_is_reported() {
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT * FROM nope");

    assert_eq!(
        err,
        DbError::UnknownSymbol {
            name: "nope".to_string()
        }
    );
}

#[test]
fn qualified_column_of_an_unknown_table_is_reported() {
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT zzz.x FROM f");

    assert!(matches!(err, DbError::UnknownColumn { .. }), "got {err:?}");
}

// ------------------------------------------- 必须显式拒绝的写法（不许猜）

#[test]
fn is_null_now_works_instead_of_being_rejected() {
    // 过去这一条是被拒绝的（"IS NULL 会恒为假，宁可拒绝"）。NULL 落地之后它
    // 真的能算，完整语义见 tests/ir_null.rs。
    let symbols = build_database();
    let out = query(&symbols, "SELECT x FROM f WHERE value IS NULL").unwrap();

    // f 里没有 NULL，所以是 0 行——但这是"真的算出来 0"，不是"拒绝执行"
    assert_eq!(out.nrows(), 0);
}

#[test]
fn string_literal_against_an_int_column_is_a_value_error() {
    // 字符串字面量**现在是支持的**（TEXT / TIMESTAMP / NUMERIC 都要用它）。
    // 只有当它对着 INT 列、又解析不成整数时才报错——报的是"值不合类型"，
    // 而不是过去那句"不支持字符串字面量"。
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT x FROM f WHERE x = 'a'");

    match err {
        DbError::BadValue { ref ty, .. } => assert_eq!(ty, &DataType::Int),
        other => panic!("expected BadValue, got {other:?}"),
    }
}

#[test]
fn right_and_full_join_are_still_rejected() {
    // LEFT JOIN 现在支持了（见 tests/ir_null.rs），但 RIGHT / FULL 还没有
    let symbols = build_database();
    for sql in [
        "SELECT f.x FROM f RIGHT JOIN t ON f.x = t.x",
        "SELECT f.x FROM f FULL JOIN t ON f.x = t.x",
    ] {
        let err = rejected(&symbols, sql);
        match err {
            DbError::Unsupported { ref feature } => {
                assert!(feature.contains("JOIN"), "{sql} => {feature}");
            }
            other => panic!("{sql}: expected Unsupported, got {other:?}"),
        }
    }
}

#[test]
fn using_is_rejected_because_it_merges_the_join_column() {
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT * FROM f JOIN t USING (x)");

    assert!(matches!(err, DbError::Unsupported { .. }), "got {err:?}");
}

#[test]
fn group_by_now_works() {
    // 过去 GROUP BY 是被拒绝的；现在真的能算，语义见 tests/ir_agg.rs
    let symbols = build_database();
    let plan = compile("SELECT x FROM f GROUP BY x", &symbols).expect("应当能降级");
    let schema = plan.schema(&symbols).expect("应当能推 schema");

    assert_eq!(schema.len(), 1);
}

#[test]
fn aggregate_call_now_works() {
    let symbols = build_database();
    let plan = compile("SELECT count(*) FROM f", &symbols).expect("应当能降级");
    let schema = plan.schema(&symbols).expect("应当能推 schema");

    assert_eq!(schema[0].ty, DataType::Int, "COUNT 返回 INT");
}

#[test]
fn distinct_is_rejected() {
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT DISTINCT x FROM f");

    assert!(matches!(err, DbError::Unsupported { .. }), "got {err:?}");
}

#[test]
fn in_list_is_rejected() {
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT x FROM f WHERE x IN (1, 2)");

    match err {
        DbError::Unsupported { ref feature } => assert!(feature.contains("IN"), "{feature}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn statement_kinds_that_are_not_queries_are_rejected() {
    let symbols = build_database();

    // UPDATE / DELETE / DROP 仍然完全不支持。
    for sql in [
        "UPDATE f SET x = 1",
        "DELETE FROM f",
        "DROP TABLE f",
        "CREATE INDEX i ON f (x)",
    ] {
        let err = rejected(&symbols, sql);
        assert!(matches!(err, DbError::Unsupported { .. }), "{sql} => {err:?}");
    }

    // INSERT **已经支持**了（走 `run_script` 那条路），但 `compile` 只接受查询，
    // 所以它在 `rejected` 里同样落到 Unsupported——只是原因换了。
    let err = rejected(&symbols, "INSERT INTO f VALUES (1, 2, 3)");
    match err {
        DbError::Unsupported { ref feature } => {
            assert!(feature.contains("不是一条查询语句"), "{feature}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

// ------------------------------------------------------------- 字面量边界

#[test]
fn an_out_of_range_integer_literal_becomes_numeric() {
    // 2147483648 超出 i32：pg_query 会把它放进 Fval。过去这里报错，现在**降级成
    // NUMERIC** —— 裸小数与超范围整数都走这条路，和 PostgreSQL 的
    // `SELECT 2147483648` 得到 numeric 一致。
    let symbols = build_database();
    let plan = compile("SELECT 2147483648 AS big", &symbols).expect("应当能降级");
    let schema = plan.schema(&symbols).expect("应当能推 schema");

    assert_eq!(schema[0].ty, DataType::NumericAny);
}

#[test]
fn i32_min_is_still_accepted() {
    // `-2147483648` 也会落在 Fval 里（不是 Ival），但它**是**合法的 i32。
    // 先按整数试一次就是为了救回这一个值。
    let symbols = build_database();
    let out = query(&symbols, "SELECT -2147483648 AS m").unwrap();

    assert_eq!(out.row(0), Some(vec![Value::Int(i32::MIN)]));
}

// --------------------------------------------------------- 解析与语句数

#[test]
fn a_syntax_error_is_reported_as_a_parse_error() {
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT FROM WHERE");

    assert!(matches!(err, DbError::Parse { .. }), "got {err:?}");
}

#[test]
fn multiple_statements_are_rejected_by_compile() {
    // `compile` / EXPLAIN 只接受恰好一条查询；多语句请走 `run_script`。
    let symbols = build_database();
    let err = rejected(&symbols, "SELECT x FROM f; SELECT x FROM f;");

    match err {
        DbError::Unsupported { ref feature } => assert!(feature.contains("一条查询"), "{feature}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

// ----------------------------------------------------------------- explain

#[test]
fn explain_shows_the_whole_pipeline() {
    let symbols = build_database();

    // `WHERE value > 10` 只引用 `f` 一列，于是被推到 `Scan` 里：**没有** `Filter` 节点
    let expected = concat!(
        "Limit(limit=3, offset=0)\n",
        "  Sort(keys=[x DESC])\n",
        "    Project(names=[x, value], exprs=[x, value])\n",
        "      Scan(f, filter=[value > 10])\n",
    );

    assert_eq!(
        explain(
            &symbols,
            "SELECT x, value FROM f WHERE value > 10 ORDER BY x DESC LIMIT 3"
        ),
        expected
    );
}

#[test]
fn explain_shows_a_join_with_both_inputs() {
    let symbols = build_database();

    // 单键等值连接、内侧是基表 → 索引嵌套循环
    let expected = concat!(
        "Project(names=[x, label], exprs=[x, label])\n",
        "  Join(inner, index=x, on=[x = x])\n",
        "    Scan(f, filter=[])\n",
        "    Scan(t, filter=[])\n",
    );

    assert_eq!(
        explain(&symbols, "SELECT f.x, t.label FROM f JOIN t ON f.x = t.x"),
        expected
    );
}

// ------------------------------------------------------------ 谓词下推（3.3）

#[test]
fn a_single_table_where_is_pushed_into_the_scan() {
    let symbols = build_database();

    // `SELECT *` 是恒等投影，没有 Project 节点，形状最干净
    assert_eq!(
        explain(&symbols, "SELECT * FROM f WHERE value > 10"),
        "Scan(f, filter=[value > 10])\n"
    );

    // 多个合取项照样并成一个 `filter`
    assert_eq!(
        explain(&symbols, "SELECT * FROM f WHERE value > 10 AND x < 0"),
        "Scan(f, filter=[(value > 10) AND (x < 0)])\n"
    );
}

#[test]
fn a_where_on_one_side_of_a_join_is_pushed_below_it() {
    let symbols = build_database();

    // 谓词下推的验收形状：`WHERE` 里只引用 `f` 的合取项落到 Join **之下**，
    // 于是整个计划里根本没有 `Filter` 节点
    let rendered = explain(
        &symbols,
        "SELECT f.x FROM f JOIN t ON f.x = t.x WHERE f.value = 10",
    );

    assert!(
        rendered.contains("Scan(f, index=[value], filter=[value = 10])"),
        "{rendered}"
    );
    assert!(!rendered.contains("Filter("), "{rendered}");
}

#[test]
fn a_predicate_over_two_tables_stays_in_a_filter() {
    let symbols = build_database();

    // `f.value + t.label > 10` 引用了两张表 → 推不下去，留在 Join 之上
    let rendered = explain(
        &symbols,
        "SELECT f.x FROM f JOIN t ON f.x = t.x WHERE f.value + t.label > 10",
    );

    assert!(rendered.contains("Filter([(value + label) > 10])"), "{rendered}");
    // 而只引用 `f` 的那一项仍然推得下去
    let rendered = explain(
        &symbols,
        "SELECT f.x FROM f JOIN t ON f.x = t.x WHERE f.value > 10 AND f.x + t.label > 10",
    );
    assert!(rendered.contains("Scan(f, filter=[value > 10])"), "{rendered}");
    assert!(rendered.contains("Filter([(x + label) > 10])"), "{rendered}");
}

#[test]
fn a_constant_predicate_is_not_pushed() {
    let symbols = build_database();

    // 常量谓词（不读列）不推：推到哪张表都是白搭，留在 Filter 里
    assert_eq!(
        explain(&symbols, "SELECT * FROM f WHERE 1 = 1"),
        "Filter([1 = 1])\n  Scan(f, filter=[])\n"
    );
}

#[test]
fn nothing_is_pushed_into_the_nullable_side_of_a_left_join() {
    let symbols = build_database();

    // `t.label` 在 LEFT JOIN 的可空侧：补出来的 NULL 也满足 `IS NULL`，
    // 推下去会让那些行凭空消失——所以这一项不推。
    let rendered = explain(
        &symbols,
        "SELECT f.x FROM f LEFT JOIN t ON f.x = t.x WHERE t.label IS NULL",
    );

    assert!(rendered.contains("Filter([(label IS NULL)])"), "{rendered}");
    assert!(rendered.contains("Scan(t, filter=[])"), "{rendered}");
}

#[test]
fn pushdown_does_not_change_the_rows() {
    let symbols = build_database();

    // 同一批数据，写成一个合取项与拆成两个合取项必须一模一样
    let joined = query(
        &symbols,
        "SELECT f.x FROM f WHERE value > 10 AND x < 0",
    )
    .unwrap();
    let nested = query(
        &symbols,
        "SELECT f.x FROM f WHERE (value > 10 AND x < 0)",
    )
    .unwrap();

    assert_eq!(col_i32(&joined, 0), vec![-5, -4]);
    assert_eq!(col_i32(&joined, 0), col_i32(&nested, 0));
}
