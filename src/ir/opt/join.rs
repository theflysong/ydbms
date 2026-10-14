//! 连接：认**等值键**、挑执行策略。
//!
//! 只认「裸列 = 裸列」，且两侧分别落在连接的两支上（谁在等号左边都行）。两侧还
//! 必须**同一大类**：`Value::Int(1)` 与 `Value::Numeric("1")` 在哈希表里不是同一个
//! 键，当键用会**安静地漏掉**本该匹配的行——嵌套循环里的比较有 `widen_numeric`
//! 兜着，键没有。所以宁可不挑（退还嵌套循环），也不给一个安静的错答案。
//!
//! 策略的守卫（键同族、内侧是基表 `Scan`、内键是裸列）由
//! [`Plan::analyze`](crate::ir::Plan::analyze) 再强制一遍。

use pg_query::protobuf::Node;

use crate::ir::expr::{Prog, ProgBuilder};
use crate::ir::irgen::Scope;
use crate::ir::plan::{passthrough_column, EqKey, JoinStrategy, Op};
use crate::ir::symbol::SymbolTable;

use super::{bare_column, equi_sides};

/// 一条「原样取第 `index` 列」的程序。
fn col_prog(index: usize) -> Prog {
    let mut p = ProgBuilder::new();
    let reg = p.col(index);
    p.finish(reg)
}

/// 从 `ON` 的合取项里挑**等值键**。
///
/// 挑不出来就不挑：连接退化成嵌套循环，正确性不变。
pub(crate) fn equi_keys(items: &[&Node], scope: &Scope<'_>, left_width: usize) -> Vec<EqKey> {
    let mut keys = Vec::new();

    for item in items {
        let (lhs, rhs) = match equi_sides(item) {
            Some(sides) => sides,
            None => continue,
        };
        let (Some(left), Some(right)) = (bare_column(lhs, scope), bare_column(rhs, scope)) else {
            continue;
        };

        // 下标 < left_width 的在左支
        let (outer_flat, inner_flat) = match (left < left_width, right < left_width) {
            (true, false) => (left, right),
            (false, true) => (right, left),
            // 两侧都在同一支（或者就是同一个下标）——不是连接键
            _ => continue,
        };

        // 同族才敢当键
        let (Some(outer_ty), Some(inner_ty)) = (
            scope.column_type_at(outer_flat),
            scope.column_type_at(inner_flat),
        ) else {
            continue;
        };
        if outer_ty.family() != inner_ty.family() {
            continue;
        }

        keys.push(EqKey {
            outer: col_prog(outer_flat),
            inner: col_prog(inner_flat - left_width),
        });
    }

    keys
}

/// 挑连接策略。这里不做代价估算（那是 3.4 的事），规则是确定的：
///
/// * 挑不出等值键 → 嵌套循环；
/// * 恰好一个键、内侧是**基表 `Scan`**、内键是裸列、且那一列**目录里有索引**
///   → 索引嵌套循环（内侧的行不必物化）；
/// * 其余 → 哈希连接。
///
/// 「目录里有索引」这一条是 3.6 的口径：不再由执行器临时扫一遍表建索引（那比
/// 顺序扫描还贵），只有 `CREATE INDEX` 建过的列才走这条路径。
pub(crate) fn join_strategy(keys: Vec<EqKey>, inner: &Op, symbols: &SymbolTable) -> JoinStrategy {
    if keys.is_empty() {
        return JoinStrategy::NestedLoop;
    }

    if keys.len() == 1 {
        let inner_column = passthrough_column(&keys[0].inner);
        if let (Op::Scan { table, .. }, Some(column)) = (inner, inner_column) {
            if symbols.index_of(table, column).is_some() {
                return JoinStrategy::IndexNestedLoop {
                    key: keys.into_iter().next().expect("只有一个键"),
                };
            }
        }
    }

    JoinStrategy::Hash { keys }
}
