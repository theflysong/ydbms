//! 谓词下推：`WHERE` 的合取项**推到哪张基表**，以及顺手挑一条索引访问路径。
//!
//! 做法：把 `WHERE` 拆成顶层合取项，每一项先按**完整作用域**降级一次，看它引用到了
//! 哪些列（[`Prog::columns`]）：
//!
//! * 列全落在**一个** FROM 项里、而这一项可以下推 → 把下标改成「这一项自己的列」
//!   （[`Prog::rebase_columns`]）并进那张表的 `Scan.filter`；
//! * 否则（引用了多张表、或者是个常量谓词）→ 留在连接之上的 `Filter` 里。
//!
//! 拉取式之下这一步的收益是**少拉行**：被推掉的行根本不进上层管道。顺带在这一步
//! 挑索引访问路径（见 [`index_path`]）。
//!
//! 下推**不改变结果**，但会改变**运行期错误出现的时机**：谓词现在可能在「本来配不上
//! 对、会被连接丢掉」的行上求值（除零之类）。PostgreSQL 同样不保证谓词的求值顺序，
//! 而错误是响的、不是安静的。

use pg_query::protobuf::Node;

use crate::db::table::Result;
use crate::ir::expr::{Prog, ProgBuilder};
use crate::ir::irgen::{lower_expr, Range, Scope};
use crate::ir::plan::{IndexPath, NodeId, PlanBuilder};
use crate::ir::value::ScalarType;

use super::{bare_column, conjuncts, equi_sides, null_rejecting};

/// 把一到多个谓词程序合成一个 `AND`；只有一个就原样返回。
fn and_all(progs: impl IntoIterator<Item = Prog>) -> Result<Prog> {
    let mut iter = progs.into_iter();
    let first = iter.next().expect("调用方保证至少有一个谓词");
    iter.try_fold(first, |acc, prog| acc.and(prog))
}

/// 降级 `WHERE`：**能推的推到基表**，推不动的合成一个 `Filter`。
pub(crate) fn plan_where(
    expr: &Node,
    from_id: NodeId,
    scope: &Scope<'_>,
    b: &mut PlanBuilder,
) -> Result<NodeId> {
    let mut items = Vec::new();
    conjuncts(expr, &mut items);

    // 每个 FROM 项一个列表；块内顺序都保持 WHERE 里的书写顺序
    let mut pushed: Vec<Vec<(&Node, Prog)>> = vec![Vec::new(); scope.ranges.len()];
    let mut kept: Vec<Prog> = Vec::new();

    for conjunct in items {
        let mut p = ProgBuilder::new();
        let reg = lower_expr(conjunct, scope, &mut p)?;
        let prog = p.finish(reg);

        // 含子查询 / 外层引用的合取项**不下推**：它面对的行必须是降级时那个 schema
        // （子查询的外层引用与 `Step::Outer` 的下标都是按它编的），推给单表 `Scan`
        // 就换了那一行——`Plan::analyze` 里的 `outers[0] == program_input` 会当场
        // 把这种计划判成非法，所以这里的闸门是**必须**的。
        let pushable = !prog.has_subquery() && !prog.has_outer();
        // 可空侧（`LEFT JOIN` 右支）只有 null-rejecting 的谓词能推：补出来的 NULL
        // 满足 `b.y IS NULL` 那一类写法，推下去会让那些行凭空消失
        let allow_nullable = pushable && null_rejecting(conjunct);

        match scope.pushable_range(&prog.columns(), allow_nullable) {
            Some(ri) if pushable => {
                let offset = scope.ranges[ri].offset;
                let rebased = prog.rebase_columns(offset)?;
                pushed[ri].push((conjunct, rebased));
                // 可空侧（`LEFT JOIN` 的右支）：推下去的那一份只能**少给连接送行**，
                // 原来那一份**必须留在连接之上**。原因是外连接补出来的 NULL 行根本
                // 没被那张表的 `Scan` 扫过——它们由连接自己补出来，只有上面的谓词
                // 丢得掉它们。`WHERE b.y = 5` 里被补出来的那一行 `b.y` 是 NULL，
                // 下面推下去不会拦住它，上面那一份才拦得住。
                //
                // （PostgreSQL 走的是另一条路：把这种 `LEFT JOIN` **简化成内连接**，
                // 于是谓词可以彻底搬下去、上面不留东西。本版不做连接类型重写——那要
                // 动计划的形状，收益与风险不成比例。）
                if scope.ranges[ri].nullable {
                    kept.push(prog);
                }
            }
            _ => kept.push(prog),
        }
    }

    for (ri, items) in pushed.into_iter().enumerate() {
        if items.is_empty() {
            continue;
        }

        let range = &scope.ranges[ri];
        // 可下推的项一定有基表 `Scan`（`pushable_range` 保证）
        let scan = range.scan.expect("可下推的项一定有基表 Scan");
        let index = index_path(&items, range, scope)?;
        let filter = and_all(items.into_iter().map(|(_, prog)| prog))?;

        b.add_scan_filter(scan, filter)?;
        if let Some(path) = index {
            b.set_scan_index(scan, path)?;
        }
    }

    match kept.is_empty() {
        true => Ok(from_id),
        false => b.filter(from_id, and_all(kept)?),
    }
}

/// 从推到某一个 FROM 项的合取项里挑一条索引访问路径。
///
/// 只认 `<这一项的裸列> = <不读列的表达式>`（两侧反过来也行）：索引就是这一列的
/// 「键 → 行号」，所以键必须是个**值**而不是列。键与索引列还必须同族，否则索引会
/// 安静地漏行（`Plan::analyze` 还会再挡一次）。
///
/// 一项最多挑一条（多列索引不在本版范围内）。挑不出来就顺序扫描：慢，但不会错。
///
/// **只有目录里真有这条索引时才挑**（`CREATE INDEX` 建的，见
/// [`crate::ir::symbol::SymbolTable::index_of`]）。本版不为了"也许有用"在建计划时
/// 先扫一遍表——那条路径比顺序扫描还贵，收益只在反复查找的连接内侧。
fn index_path(
    items: &[(&Node, Prog)],
    range: &Range,
    scope: &Scope<'_>,
) -> Result<Option<IndexPath>> {
    for (ast, _) in items {
        let (lhs, rhs) = match equi_sides(ast) {
            Some(sides) => sides,
            None => continue,
        };

        // 哪一侧是「这一项的裸列」都行
        for (column_node, key_node) in [(lhs, rhs), (rhs, lhs)] {
            let Some(index) = bare_column(column_node, scope) else {
                continue;
            };
            if !range.contains(index) {
                continue;
            }

            let mut p = ProgBuilder::new();
            let reg = lower_expr(key_node, scope, &mut p)?;
            let key = p.finish(reg);
            // 索引的键在**没有输入行**的前提下求值（`index_key` 用 `eval`，环境是空的），
            // 所以既不能读列，也不能有子查询 / 外层引用
            if !key.columns().is_empty() || key.has_subquery() || key.has_outer() {
                continue;
            }

            let column_family = match scope.column_type_at(index) {
                Some(ty) => ty.family(),
                None => continue,
            };
            let key_family = match key.verify(&range.columns)? {
                ScalarType::Value(family) => family,
                // 无类型 NULL（`x = NULL` 本来就是 UNKNOWN）与真值都不走索引
                _ => continue,
            };
            if key_family != column_family {
                continue;
            }

            let column = index - range.offset;
            // 没有索引就走顺序扫描：路径不是"猜"出来的，是目录里查出来的
            if scope.symbols().index_of(&range.name, column).is_none() {
                continue;
            }

            return Ok(Some(IndexPath { column, key }));
        }
    }

    Ok(None)
}
