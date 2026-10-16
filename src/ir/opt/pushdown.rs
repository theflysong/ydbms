//! 谓词下推:`WHERE` 的合取项**推到哪张基表**,以及顺手挑一条索引访问路径.
//!
//! 做法:把 `WHERE` 拆成顶层合取项,每一项先按**完整作用域**降级一次,看它引用到了
//! 哪些列([`Prog::columns`]):
//!
//! * 列全落在**一个** FROM 项里,而这一项可以下推 → 把下标改成"这一项自己的列"
//!   ([`Prog::rebase_columns`])并进那张表的 `Scan.filter`;
//! * 否则(引用了多张表,或者是个常量谓词)→ 留在连接之上的 `Filter` 里.
//!
//! 拉取式之下这一步的收益是**少拉行**:被推掉的行根本不进上层管道.顺带在这一步
//! 挑索引访问路径(见 [`index_path`]).
//!
//! 下推**不改变结果**,但会改变**运行期错误出现的时机**:谓词现在可能在"本来配不上
//! 对,会被连接丢掉"的行上求值(除零之类).PostgreSQL 同样不保证谓词的求值顺序,
//! 而错误是响的,不是安静的.

use pg_query::protobuf::Node;

use crate::db::table::Result;
use crate::ir::expr::{Prog, ProgBuilder};
use crate::ir::irgen::{lower_expr, lower_outer_key, Range, Scope};
use crate::ir::plan::{IndexPath, NodeId, PlanBuilder};
use crate::ir::value::{Scalar, ScalarType, Tri};

use super::{bare_column, conjuncts, equi_sides, null_rejecting};

/// 把一到多个谓词程序合成一个 `AND`;只有一个就原样返回.
fn and_all(progs: impl IntoIterator<Item = Prog>) -> Result<Prog> {
    let mut iter = progs.into_iter();
    let first = iter.next().expect(&tr!("ir.opt.pushdown.001").into_owned());
    iter.try_fold(first, |acc, prog| acc.and(prog))
}

/// 降级 `WHERE`:**能推的推到基表**,推不动的合成一个 `Filter`.
///
/// 合取项先**全部降级一次**,再决定推到哪(降级顺手做了常量折叠,见
/// [`ProgBuilder::finish`]),于是两类**常量谓词**在这里就能认出来(TODO 3.7):
///
/// * **恒真**(折成 `Truth(True)`):直接丢掉 -- 它不影响哪些行留下;
/// * **恒不成立**(折成 `Truth(False)` / `Truth(Unknown)`):它是 `AND` 链上的
///   终点 -- 它**右边**的项一个都不会被求值,整条 `WHERE` 剩下的就是
///   "它左边那些 + 它自己",一行都不留.谓词因此是一个**常量**(它左边什么都没有
///   时),`exec` 的 `FilterSource` 见到恒不成立的常量谓词一行都不拉;而那张表
///   **仍旧留在计划里**(哪怕只剩一个 `Filter`)--权限检查问的是
///   `Plan::used_columns`,把表从计划里摘掉就等于一条 `WHERE 1 = 0` 绕开了
///   `SELECT` 权限.
///
/// 前缀**要留着**不是多余的:`AND` 是**从左到右**求值的(`ir_lazy.rs` 钉着这条
/// 口径:顺序就是书写顺序,降级不重排),`(1/0 = 1) AND false` 左边那一项的错
/// 照旧要报出来.
pub(crate) fn plan_where(
    expr: &Node,
    from_id: NodeId,
    scope: &Scope<'_>,
    b: &mut PlanBuilder,
) -> Result<NodeId> {
    let mut items = Vec::new();
    conjuncts(expr, &mut items);

    // 每一项先降级一次;恒真的丢掉,遇到第一个恒不成立的就收工(它右边的全是死项)
    let mut dead = false;
    let mut lowered: Vec<(&Node, Prog)> = Vec::with_capacity(items.len());
    for conjunct in items {
        let mut p = ProgBuilder::new();
        let reg = lower_expr(conjunct, scope, &mut p)?;
        let prog = p.finish(reg);

        match prog.as_const() {
            Some(Scalar::Truth(Tri::True)) => continue,
            Some(Scalar::Truth(_)) => {
                lowered.push((conjunct, prog));
                dead = true;
                break;
            }
            _ => {}
        }
        lowered.push((conjunct, prog));
    }

    if dead {
        // 恒不成立:整条 `WHERE` 只剩"前缀 + 它自己".前缀一项都不推给 `Scan.filter`
        // -- 推下去就是"拉几行来算一遍再全丢掉",不如让上面那个 `Filter` 自己算
        // (它本来就在逐行复核).
        let predicate = and_all(lowered.into_iter().map(|(_, prog)| prog))?;
        return b.filter(from_id, predicate);
    }

    // 每个 FROM 项一个列表;块内顺序都保持 WHERE 里的书写顺序
    let mut pushed: Vec<Vec<(&Node, Prog)>> = vec![Vec::new(); scope.ranges.len()];
    let mut kept: Vec<Prog> = Vec::new();
    // **只当索引探测键**的合取项(见下面那段):它们进不了 `Scan.filter`,
    // 但可以拿来挑一条索引探测的路径.
    let mut probe_only: Vec<Vec<(&Node, Prog)>> = vec![Vec::new(); scope.ranges.len()];

    for (conjunct, prog) in lowered {
        // 含子查询 / 外层引用的合取项**不推给 `Scan.filter`**:`Scan.filter` 面对
        // 的行必须是这张表自己的 schema(子查询的外层引用与 `Step::Outer` 的下标
        // 都是按降级时那个 schema 编的),推给单表 `Scan` 就换了那一行--而且
        // `Scan.filter` 是在**没有外层行**的前提下求值的(外层行是空的),
        // 读 `Step::Outer` 只会报"没有求值环境".所以这两类一律留在上面的 `Filter`
        // 里(那一个的外层行是齐的).
        let pushable = !prog.has_subquery() && !prog.has_outer();
        // 可空侧(`LEFT JOIN` 右支)只有 null-rejecting 的谓词能推:补出来的 NULL
        // 满足 `b.y IS NULL` 那一类写法,推下去会让那些行凭空消失
        let allow_nullable = pushable && null_rejecting(conjunct);

        match scope.pushable_range(&prog.columns(), allow_nullable) {
            Some(ri) if pushable => {
                let offset = scope.ranges[ri].offset;
                let rebased = prog.rebase_columns(offset)?;
                pushed[ri].push((conjunct, rebased));
                // 可空侧(`LEFT JOIN` 的右支):推下去的那一份只能**少给连接送行**,
                // 原来那一份**必须留在连接之上**.原因是外连接补出来的 NULL 行根本
                // 没被那张表的 `Scan` 扫过--它们由连接自己补出来,只有上面的谓词
                // 丢得掉它们.`WHERE b.y = 5` 里被补出来的那一行 `b.y` 是 NULL,
                // 下面推下去不会拦住它,上面那一份才拦得住.
                //
                // (PostgreSQL 走的是另一条路:把这种 `LEFT JOIN` **简化成内连接**,
                // 于是谓词可以彻底搬下去,上面不留东西.本版不做连接类型重写--那要
                // 动计划的形状,收益与风险不成比例.)
                if scope.ranges[ri].nullable {
                    kept.push(prog);
                }
            }
            // **相关子查询里的等值谓词**(TODO 8.7):`c.tconst = b.tconst` 每一行
            // 的键都不同,推给 `Scan.filter` 不成立(它读外层行),但这一列上有
            // 索引时它可以当**探测**用--内层一次只读命中那几行,而不是每来一行
            // 扫一遍整张表(`Test 10.3`).谓词本身**照旧留在上面的 `Filter` 里**:
            // 索引只是访问路径,挑不出来(没索引 / 不是等值)就还是顺序扫描,
            // 一分语义都不改.
            Some(ri) => {
                probe_only[ri].push((conjunct, prog.clone()));
                kept.push(prog);
            }
            None => kept.push(prog),
        }
    }

    for (ri, items) in pushed.into_iter().enumerate() {
        let range = &scope.ranges[ri];
        // 可下推的项一定有基表 `Scan`(`pushable_range` 保证)
        let scan = match range.scan {
            Some(scan) => scan,
            None => continue,
        };

        let index = index_path(&items, range, scope)?;

        if !items.is_empty() {
            b.add_scan_filter(scan, and_all(items.into_iter().map(|(_, prog)| prog))?)?;
        }
        // 下推的那些挑不出索引,再看只当探测的那些
        let probes = std::mem::take(&mut probe_only[ri]);
        let index = match index {
            Some(path) => Some(path),
            None => index_path(&probes, range, scope)?,
        };
        if let Some(path) = index {
            b.set_scan_index(scan, path)?;
        }
    }
    match kept.is_empty() {
        true => Ok(from_id),
        false => b.filter(from_id, and_all(kept)?),
    }
}

/// 给 `UPDATE ... WHERE` 挑一条索引访问路径(TODO 3.16).
///
/// `UPDATE` 只有**一张表**,而它的 `WHERE` 面对的就是这张表的扁平 schema--于是
/// 挑路径那件事与 `SELECT` 完全一样(同一份 [`index_path`],同一套合取项拆分),
/// 只是这里**不建计划**:`UPDATE` 逐行改,要的是**行号**,不是行(见
/// [`crate::ir::stmt::Stmt::Update`] 的 `index` 字段).
///
/// 谓词**照旧**整个留在 `Stmt::Update::filter` 里逐行复核:索引只是访问路径,
/// 挑不出来(没索引 / 不是等值 / 键读列)或者挑错都只是慢,不会算错.
///
/// `scope` 必须是**没有外层链**的单表作用域(`Scope::from_range` 造的那种):
/// 外层键(相关子查询那一种)在 `UPDATE` 里没有意义,也不会出现.
pub(crate) fn update_index_path(
    expr: &Node,
    range: &Range,
    scope: &Scope<'_>,
) -> Result<Option<IndexPath>> {
    let mut conjuncts_of = Vec::new();
    conjuncts(expr, &mut conjuncts_of);

    let mut lowered = Vec::with_capacity(conjuncts_of.len());
    for conjunct in conjuncts_of {
        let mut p = ProgBuilder::new();
        let reg = lower_expr(conjunct, scope, &mut p)?;
        lowered.push((conjunct, p.finish(reg)));
    }
    index_path(&lowered, range, scope)
}

/// 从推到某一个 FROM 项的合取项里挑一条索引访问路径.
///
/// 两种键,各有一条路:
///
/// * **常量键**:`<这一项的裸列> = <不读列的表达式>`(两侧反过来也行).索引就是
///   这一列的"键 → 行号",所以键必须是个**值**而不是列;
/// * **外层键**(TODO 8.7):`<这一项的裸列> = <只读外层行的表达式>`.这是**相关
///   子查询**里唯一能指望索引的形状:`EXISTS (SELECT 1 FROM c WHERE c.tconst =
///   b.tconst)` 的内层只有一张表,`opt::join` 的索引嵌套循环挂不上去(那是给连接
///   用的),于是没有这条路的话,内层**每来一行都要把整张表扫一遍**--实测
///   `tests.sql` 的 Test 10.3 因此在 499999 行上跑不完.
///
/// 键与索引列还必须同族,否则索引会安静地漏行(`Plan::analyze` 还会再挡一次).
///
/// 一项最多挑一条(多列索引不在本版范围内).挑不出来就顺序扫描:慢,但不会错.
///
/// **只有目录里真有这条索引时才挑**(`CREATE INDEX` 建的,见
/// [`crate::ir::symbol::SymbolTable::index_of`]).本版不为了"也许有用"在建计划时
/// 先扫一遍表--那条路径比顺序扫描还贵,收益只在反复查找的连接内侧.
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

        // 哪一侧是"这一项的裸列"都行
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
            // 索引的键在**没有输入行**的前提下求值(`index_key` 用 `eval`,环境是空的),
            // 所以不能读本查询的列,也不能有子查询
            if !key.columns().is_empty() || key.has_subquery() {
                continue;
            }

            // **只读外层行**的键(相关子查询的等值探测,见函数头那段):
            // 降级时它在本层一个列都不读,`columns()` 因此是空的.
            if !key.columns().is_empty() {
                continue;
            }
            if key.has_outer() {
                let Some(rebased) = lower_outer_key(key_node, scope) else {
                    continue;
                };

                // 重排出来的键读的是**这张表里同名的列**,于是它的类型就是那一列的
                // 类型:同族由"这个名字在两边的列类型一致"直接保证(重排时核过).
                // 这里只把"那一列上真有索引"钉住.
                let column = index - range.offset;
                // 重排之后那个键必须**只读索引这一列**(它就是被搬过来的那一列)
                if rebased.columns() != [column].into_iter().collect() {
                    continue;
                }
                if scope.symbols().index_of(&range.name, column).is_none() {
                    continue;
                }
                return Ok(Some(IndexPath {
                    column,
                    key: rebased,
                    outer: true,
                }));
            }

            let column_family = match scope.column_type_at(index) {
                Some(ty) => ty.family(),
                None => continue,
            };
            let key_family = match key.verify(&range.columns)? {
                ScalarType::Value(family) => family,
                // 无类型 NULL(`x = NULL` 本来就是 UNKNOWN)与真值都不走索引
                _ => continue,
            };
            if key_family != column_family {
                continue;
            }

            let column = index - range.offset;
            // 没有索引就走顺序扫描:路径不是"猜"出来的,是目录里查出来的
            if scope.symbols().index_of(&range.name, column).is_none() {
                continue;
            }

            return Ok(Some(IndexPath {
                column,
                key,
                outer: false,
            }));
        }
    }

    Ok(None)
}

