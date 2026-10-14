//! [`PhysTab`] 的 testbench。
//!
//! 构造一张物理表 `(x, value, drvd)`，其中
//!
//! * `x ∈ {-5, ..., 5}`
//! * `value = f(x) = x² + 1`
//! * `drvd  = f'(x) = 2x`
//!
//! 除了逐行核对数值，还验证了表抽象本身：形状确实来自 tabshape、trait object
//! 可用、越界与元数不匹配被拒绝、以及「只实现必需方法即为只读表」的默认行为。

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, Row, TabShape, Table, Value};
use ydbms::db::PhysTab;

const X_MIN: i32 = -5;
const X_MAX: i32 = 5;
/// 行数：x 取遍闭区间 `[-5, 5]`。
const N: usize = (X_MAX - X_MIN + 1) as usize;

/// f(x) = x² + 1
fn f(x: i32) -> i32 {
    x * x + 1
}

/// f'(x) = 2x
fn df(x: i32) -> i32 {
    2 * x
}

/// 表形状：`x INT, value INT, drvd INT`。
fn quadratic_shape() -> ColumnsShape {
    ColumnsShape::new(vec![
        Column::new("x", DataType::Int),
        Column::new("value", DataType::Int),
        Column::new("drvd", DataType::Int),
    ])
}

/// 把 `x ∈ [-5, 5]` 的 `(x, f(x), f'(x))` 全部插入，返回建好的物理表。
fn build_quadratic_table() -> PhysTab {
    let shape = quadratic_shape();
    let mut tab = PhysTab::new("f", &shape);
    for x in X_MIN..=X_MAX {
        tab.insert(vec![Value::from(x), Value::from(f(x)), Value::from(df(x))])
            .expect("insert should succeed");
    }
    tab
}

/// 把 x 换成它在表中的行号。行序与 `x` 的升序一致。
fn row_of(x: i32) -> usize {
    (x - X_MIN) as usize
}

#[test]
fn shape_is_derived_from_a_tabshape() {
    let shape = quadratic_shape();
    let tab = PhysTab::new("f", &shape);

    assert_eq!(shape.arity(), 3);
    assert_eq!(tab.arity(), 3, "形状应当由 tabshape 传进来");
    assert_eq!(tab.name(), "f");
    assert_eq!(tab.nrows(), 0, "刚生成的物理表是空的");

    let names: Vec<&str> = tab.columns().iter().map(|c| c.name()).collect();
    assert_eq!(names, ["x", "value", "drvd"], "列序即建表顺序");

    assert_eq!(tab.index_of("x"), Some(0));
    assert_eq!(tab.index_of("value"), Some(1));
    assert_eq!(tab.index_of("drvd"), Some(2));
    assert_eq!(tab.index_of("nope"), None);

    assert_eq!(tab.column(1).map(|c| c.ty()), Some(DataType::Int));
    assert_eq!(tab.column(3), None);
}

#[test]
fn shape_is_copied_at_construction() {
    let mut shape = quadratic_shape();
    let tab = PhysTab::new("f", &shape);

    shape.push(Column::new("extra", DataType::Int));

    assert_eq!(shape.arity(), 4);
    assert_eq!(tab.arity(), 3, "建表之后原 shape 的改动不应影响这张表");
}

#[test]
fn accepts_any_tabshape_including_a_trait_object() {
    let shape = quadratic_shape();
    let dyn_shape: &dyn TabShape = &shape;
    let tab = PhysTab::new("f", dyn_shape);

    assert_eq!(tab.arity(), 3);
    assert_eq!(tab.name(), "f");
}

#[test]
fn builds_one_row_per_x() {
    let tab = build_quadratic_table();

    assert_eq!(tab.nrows(), N);
    assert_eq!(
        tab.row(0),
        Some(vec![Value::Int(-5), Value::Int(26), Value::Int(-10)])
    );
    assert_eq!(
        tab.row(N - 1),
        Some(vec![Value::Int(5), Value::Int(26), Value::Int(10)])
    );
    assert_eq!(tab.row(N), None, "越界读取应当是 None");
}

#[test]
fn value_column_is_x_squared_plus_one() {
    let tab = build_quadratic_table();

    for x in X_MIN..=X_MAX {
        let r = row_of(x);
        assert_eq!(tab.value(r, 0), Some(Value::Int(x)), "第 {r} 行的 x");
        assert_eq!(tab.value(r, 1), Some(Value::Int(f(x))), "f({x})");
        assert_eq!(
            tab.value(r, 1).and_then(|v| v.as_int()),
            Some(x * x + 1),
            "f({x}) = x² + 1"
        );
    }
}

#[test]
fn drvd_column_is_the_derivative_two_x() {
    let tab = build_quadratic_table();

    for x in X_MIN..=X_MAX {
        let r = row_of(x);
        assert_eq!(tab.value(r, 2), Some(Value::Int(df(x))), "f'({x})");
        assert_eq!(
            tab.value(r, 2).and_then(|v| v.as_int()),
            Some(2 * x),
            "f'({x}) = 2x"
        );
    }
}

#[test]
fn derivative_matches_the_forward_difference_of_f() {
    let tab = build_quadratic_table();

    for x in X_MIN..X_MAX {
        let r = row_of(x);
        let f_x = tab.value(r, 1).unwrap().as_int().unwrap();
        let f_next = tab.value(r + 1, 1).unwrap().as_int().unwrap();
        let d_x = tab.value(r, 2).unwrap().as_int().unwrap();

        // 对抛物线有 f(x+1) - f(x) = 2x + 1 = f'(x) + 1
        assert_eq!(f_next - f_x, d_x + 1, "x = {x} 处的前向差分");
    }
}

#[test]
fn even_function_and_odd_derivative() {
    let tab = build_quadratic_table();
    let mut sum_df = 0;

    for x in X_MIN..=X_MAX {
        let r = row_of(x);
        let mirror = row_of(-x);

        assert_eq!(tab.value(r, 1), tab.value(mirror, 1), "f({x}) == f({})", -x);
        assert_eq!(
            tab.value(r, 2),
            tab.value(mirror, 2).map(|v| Value::Int(-v.as_int().unwrap())),
            "f'({x}) == -f'({})",
            -x
        );

        sum_df += tab.value(r, 2).unwrap().as_int().unwrap();
    }

    assert_eq!(sum_df, 0, "f' 是奇函数，全部取值之和为 0");
}

#[test]
fn works_behind_a_trait_object() {
    let mut tab: Box<dyn Table> = Box::new(build_quadratic_table());

    assert_eq!(tab.name(), "f");
    assert_eq!(tab.nrows(), N);
    assert_eq!(tab.arity(), 3, "supertrait 让 &dyn Table 也能给出形状");
    assert_eq!(tab.index_of("drvd"), Some(2));
    assert_eq!(tab.value(0, 1), Some(Value::Int(26)));
    assert_eq!(tab.iter_rows().count(), N);

    // 通过 trait object 写入
    let new_row = vec![Value::Int(6), Value::Int(37), Value::Int(12)];
    let idx = tab.insert(new_row).expect("insert through dyn Table");

    assert_eq!(idx, N, "新行的序号应当是原来的行数");
    assert_eq!(tab.nrows(), N + 1);
    assert_eq!(tab.value(N, 1), Some(Value::Int(37)), "f(6) = 37");
}

#[test]
fn insert_rejects_wrong_arity_without_side_effects() {
    let mut tab = build_quadratic_table();
    let before = tab.nrows();

    let err = tab
        .insert(vec![Value::Int(1), Value::Int(2)])
        .expect_err("少一列应当被拒绝");

    assert_eq!(err, DbError::ArityMismatch { expected: 3, got: 2 });
    assert_eq!(tab.nrows(), before, "失败的行不应被写入");
}

#[test]
fn out_of_bounds_operations_are_rejected() {
    let mut tab = build_quadratic_table();

    // 读越界 → None
    assert_eq!(tab.row(N), None);
    assert_eq!(tab.value(N, 0), None);
    assert_eq!(tab.value(0, 3), None);

    // 写越界 → Err
    assert_eq!(
        tab.update(N, 0, Value::Int(0)).unwrap_err(),
        DbError::RowOutOfBounds { index: N, nrows: N }
    );
    assert_eq!(
        tab.update(0, 3, Value::Int(0)).unwrap_err(),
        DbError::ColumnOutOfBounds { index: 3, arity: 3 }
    );
    assert_eq!(
        tab.delete(N).unwrap_err(),
        DbError::RowOutOfBounds { index: N, nrows: N }
    );
}

#[test]
fn update_and_delete_round_trip() {
    let mut tab = build_quadratic_table();

    tab.update(0, 1, Value::Int(99)).expect("update in place");
    assert_eq!(tab.value(0, 1), Some(Value::Int(99)));

    let was_second = tab.row(1).unwrap();
    tab.delete(0).expect("delete row 0");

    assert_eq!(tab.nrows(), N - 1);
    assert_eq!(tab.row(0), Some(was_second), "删除后后面的行整体前移");
    assert_eq!(tab.value(0, 0), Some(Value::Int(-4)));
}

#[test]
fn borrowed_accessors_avoid_copying() {
    let tab = build_quadratic_table();

    assert_eq!(tab.row_ref(0).map(<[Value]>::len), Some(3));
    assert_eq!(tab.cell_ref(0, 2), Some(&Value::Int(-10)));
    assert_eq!(tab.row_ref(N), None);
    assert_eq!(tab.cell_ref(0, 3), None);
    assert_eq!(tab.rows().len(), N);
}

#[test]
fn truncate_empties_the_table_but_keeps_the_shape() {
    let mut tab = build_quadratic_table();

    tab.truncate();
    assert_eq!(tab.nrows(), 0);
    assert_eq!(tab.row(0), None);
    assert_eq!(tab.iter_rows().count(), 0);
    assert_eq!(tab.arity(), 3, "清空的是数据，不是形状");

    tab.truncate();
    assert_eq!(tab.nrows(), 0, "truncate 是幂等的");
}

/// 一个只读视图：**只**实现必需方法，变更操作完全依赖 [`Table`] 的默认实现。
/// 未来的 `JoinTab` 就是这种形状。
struct ReadOnlyView {
    shape: ColumnsShape,
    rows: Vec<Row>,
}

impl ReadOnlyView {
    fn copy_of(tab: &PhysTab) -> Self {
        ReadOnlyView {
            shape: ColumnsShape::new(tab.columns().to_vec()),
            rows: tab.rows().to_vec(),
        }
    }
}

impl TabShape for ReadOnlyView {
    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }
}

impl Table for ReadOnlyView {
    fn name(&self) -> &str {
        "read_only_view"
    }

    fn nrows(&self) -> usize {
        self.rows.len()
    }

    fn row(&self, r: usize) -> Option<Row> {
        self.rows.get(r).cloned()
    }
}

#[test]
fn a_table_without_mutators_is_read_only_by_default() {
    let mut view = ReadOnlyView::copy_of(&build_quadratic_table());

    // 默认实现提供的读操作照常可用
    assert_eq!(view.nrows(), N);
    assert_eq!(view.arity(), 3);
    assert_eq!(view.value(0, 1), Some(Value::Int(26)));
    assert_eq!(view.iter_rows().count(), N);

    // 三个变更操作都没有被覆盖 → 一律 ReadOnly
    let read_only = DbError::ReadOnly {
        table: "read_only_view".to_string(),
    };
    assert_eq!(
        view.insert(vec![Value::Int(0), Value::Int(1), Value::Int(2)])
            .unwrap_err(),
        read_only
    );
    assert_eq!(view.update(0, 0, Value::Int(9)).unwrap_err(), read_only);
    assert_eq!(view.delete(0).unwrap_err(), read_only);
    assert_eq!(view.nrows(), N, "只读表没有被改动");
}
