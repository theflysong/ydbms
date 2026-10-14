//! 优化器：**规则式**的「怎么查」决策。
//!
//! 优化器在这里只有两件事（都不做代价估算——那是 3.4 的事）：
//!
//! * [`pushdown`] —— `WHERE` 的合取项**推到哪张基表**、要不要顺手挑一条索引
//!   访问路径；
//! * [`join`] —— 从 `ON` 里认**等值键**，据此挑嵌套循环 / 哈希 / 索引嵌套循环。
//!
//! 这一层与 [`crate::ir::irgen`] 的分工：**这里只决定，不落笔**。决策要用的
//! 作用域表示（[`Scope`]）、AST 小工具、以及「把程序并进某个 `Scan`」的动作
//! 分别来自 `irgen` 与 [`PlanBuilder`](crate::ir::PlanBuilder)。两者都在 `ir`
//! 里，所以 `opt` 直接引用 `irgen` 的 `pub(crate)` 项（模块互引，同一个 crate）。
//!
//! # 一条铁律：挑不出来就退回首选方案
//!
//! 这里所有判定都是**保守**的：认不出等值键就退还嵌套循环、认不出可下推的列就
//! 留在 `Filter` 里。正确性从不依赖这一层——`Plan::analyze` 还会把「同族键」
//! 「索引键不读列」这些不变量再挡一遍。所以挑错只是慢，不会算错。

pub(crate) mod join;
pub(crate) mod pushdown;

use pg_query::protobuf::{node, AExprKind, BoolExprType, NullTestType, Node};

use super::irgen::{ref_parts, single_string, Resolved, Scope};

// ------------------------------------------------ null-rejecting（可空侧下推）

/// 「所有被引用的列都是 NULL 时，这个表达式是 NULL」吗。
///
/// 它描述的是**严格性**（NULL 传进去、NULL 传出来）。判定保守：认不出来的写法
/// 一律 `false`（于是"不下推"，不会错）。
fn null_propagating(node: &Node) -> bool {
    match &node.node {
        // 列当然是：列是 NULL，读出来就是 NULL
        Some(node::Node::ColumnRef(c)) => !c.fields.is_empty(),
        // 常量与列无关：它是 NULL 也**不是**"列传出来的"
        Some(node::Node::AConst(_)) => false,
        Some(node::Node::AExpr(a)) => {
            let kind = AExprKind::try_from(a.kind).unwrap_or(AExprKind::Undefined);
            let (l, r) = (a.lexpr.as_deref(), a.rexpr.as_deref());
            match kind {
                // 算术：任一侧 NULL → NULL（`apply` 就是这么实现的）
                AExprKind::AexprOp
                    if matches!(
                        single_string(&a.name).as_deref(),
                        Some("+") | Some("-") | Some("*") | Some("/")
                    ) =>
                {
                    l.is_some_and(null_propagating) && r.is_some_and(null_propagating)
                }
                // 比较 / LIKE / 正则：NULL 参与就是 UNKNOWN（三值逻辑里的 NULL）
                AExprKind::AexprOp
                | AExprKind::AexprLike
                | AExprKind::AexprIlike => {
                    (l.is_some_and(null_propagating) && l.is_some_and(has_column_ref))
                        || (r.is_some_and(null_propagating) && r.is_some_and(has_column_ref))
                }
                _ => false,
            }
        }
        Some(node::Node::BoolExpr(b)) => {
            let kind = BoolExprType::try_from(b.boolop).unwrap_or(BoolExprType::Undefined);
            match kind {
                // `NULL AND FALSE = FALSE`，所以只有**全是** NULL 传下去才还是 NULL
                BoolExprType::AndExpr | BoolExprType::OrExpr => {
                    !b.args.is_empty() && b.args.iter().all(null_propagating)
                }
                BoolExprType::NotExpr => b.args.first().is_some_and(null_propagating),
                _ => false,
            }
        }
        // 文本函数与 `ROUND`/`EXTRACT` 都是严格的；`CONCAT_WS` 会跳过 NULL，不是
        Some(node::Node::FuncCall(f)) => {
            let name = f
                .funcname
                .last()
                .and_then(|n| match &n.node {
                    Some(node::Node::String(s)) => Some(s.sval.to_lowercase()),
                    _ => None,
                })
                .unwrap_or_default();
            let strict = !matches!(name.as_str(), "concat_ws" | "coalesce" | "nullif");
            strict && !f.args.is_empty() && f.args.iter().all(null_propagating)
        }
        // `x::numeric`：NULL 进去还是 NULL
        Some(node::Node::TypeCast(t)) => t.arg.as_deref().is_some_and(null_propagating),
        // `IS NULL` 恒为确定的真/假；`CASE` 能从 NULL 造出非 NULL —— 都不是
        _ => false,
    }
}

/// 「所有被引用的列都是 NULL 时，这个谓词**不会是 TRUE**」吗。
///
/// 可空侧（`LEFT JOIN` 的右支）能下推的充要条件就是它：外连接补出来的那一行
/// 全是 NULL，谓词在那上面不为真，于是推下去的那些行本来也会被丢掉。
///
/// 只放行**有把握**的写法：
///
/// * `IS NOT NULL` 是（NULL → FALSE）；
/// * 比较 / `LIKE` / 正则：只要有一侧是"列传出来的 NULL"就够了——NULL 参与比较
///   的结果是 UNKNOWN，不是 TRUE；
/// * `AND` 只要**有一个**合取项是；`OR` 要**全部**是；
/// * `NOT` 只在操作数严格（NULL → NULL）时才是；
/// * `IS NULL`、`CASE`、子查询、认不出来的写法一律不是（留在 Join 之上）。
pub(crate) fn null_rejecting(node: &Node) -> bool {
    match &node.node {
        Some(node::Node::NullTest(t)) => {
            NullTestType::try_from(t.nulltesttype).unwrap_or(NullTestType::Undefined)
                == NullTestType::IsNotNull
        }
        Some(node::Node::AExpr(a)) => {
            let kind = AExprKind::try_from(a.kind).unwrap_or(AExprKind::Undefined);
            match kind {
                AExprKind::AexprOp | AExprKind::AexprLike | AExprKind::AexprIlike => {
                    // 比较/模式匹配：任一侧是列传出来的 NULL，结果就不可能是 TRUE
                    null_propagating(node)
                }
                _ => false,
            }
        }
        Some(node::Node::BoolExpr(b)) => {
            let kind = BoolExprType::try_from(b.boolop).unwrap_or(BoolExprType::Undefined);
            match kind {
                BoolExprType::AndExpr => b.args.iter().any(null_rejecting),
                BoolExprType::OrExpr => !b.args.is_empty() && b.args.iter().all(null_rejecting),
                BoolExprType::NotExpr => b.args.first().is_some_and(null_propagating),
                _ => false,
            }
        }
        _ => false,
    }
}

/// 把 `WHERE` / `ON` 拆成**顶层合取项**（`a AND b AND c` → 三项）。
///
/// 只拆 `AND`：`OR` 整体算一项（拆开就不是「都要成立」了）。嵌套的 `AND` 是递归
/// 展开的——`lower_expr` 本来就在表达式嵌套上递归，这里不引入新的失败类别。
pub(crate) fn conjuncts<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
    if let Some(node::Node::BoolExpr(b)) = &node.node {
        if BoolExprType::try_from(b.boolop).unwrap_or(BoolExprType::Undefined)
            == BoolExprType::AndExpr
        {
            for arg in &b.args {
                conjuncts(arg, out);
            }
            return;
        }
    }
    out.push(node);
}

/// 这个节点是 `l = r` 吗；是就把两侧给出来（别的比较、别的写法都返回 `None`）。
pub(crate) fn equi_sides(node: &Node) -> Option<(&Node, &Node)> {
    let a = match &node.node {
        Some(node::Node::AExpr(a)) => a,
        _ => return None,
    };
    if AExprKind::try_from(a.kind).unwrap_or(AExprKind::Undefined) != AExprKind::AexprOp {
        return None;
    }
    if single_string(&a.name).as_deref() != Some("=") {
        return None;
    }
    Some((a.lexpr.as_deref()?, a.rexpr.as_deref()?))
}

/// 这棵表达式子树里引用了列吗。
///
/// null-rejecting 的判定要问"这一侧真的从列来吗"：`x = 5` 里左侧是列，右侧是常量。
fn has_column_ref(node: &Node) -> bool {
    match &node.node {
        Some(node::Node::ColumnRef(c)) => !c.fields.is_empty(),
        Some(node::Node::AExpr(a)) => {
            a.lexpr.as_deref().is_some_and(has_column_ref)
                || a.rexpr.as_deref().is_some_and(has_column_ref)
        }
        Some(node::Node::BoolExpr(b)) => b.args.iter().any(has_column_ref),
        Some(node::Node::NullTest(t)) => t.arg.as_deref().is_some_and(has_column_ref),
        Some(node::Node::CaseExpr(c)) => {
            c.arg.as_deref().is_some_and(has_column_ref)
                || c.args.iter().any(has_column_ref)
                || c.defresult.as_deref().is_some_and(has_column_ref)
        }
        Some(node::Node::FuncCall(f)) => f.args.iter().any(has_column_ref),
        Some(node::Node::TypeCast(t)) => t.arg.as_deref().is_some_and(has_column_ref),
        // 认不出来的（子查询之类）当成"引用了列"：保守
        _ => true,
    }
}

/// 这个节点是个能解析的裸列引用吗；是就返回它在**扁平 schema** 里的下标。
pub(crate) fn bare_column(node: &Node, scope: &Scope<'_>) -> Option<usize> {
    let c = match &node.node {
        Some(node::Node::ColumnRef(c)) => c,
        _ => return None,
    };
    let parts = ref_parts(&c.fields)?;
    // 外层列不能当连接键：键是**这一支自己的行**上求值的
    match scope.resolve(&parts).ok()? {
        Resolved::Local(index) => Some(index),
        Resolved::Outer { .. } => None,
    }
}
