//! 线性计划的执行--**拉取式**.
//!
//! 靠不变量 I1(输入下标严格小于自身下标),**构建**管道就是对一个数组的正向
//! `for` 循环:走到第 `id` 个节点时,它的操作数一定已经建好了.这里没有递归,
//! 所以再深的计划也不会爆栈.
//!
//! 每个节点产出的是一个 [`RowSource`]--一行一行拉的游标,不再是物化的表.上层拉一行,
//! 下层才算一行.**只有这几处会把行攒下来**:
//!
//! * 需要**随机访问**的算子(`Sort`,`Aggregate` 必须先看完全部输入);
//! * 需要**重扫**的地方(嵌套循环连接的内侧,被多个父节点引用的节点);
//! * 哈希连接的 build 侧(抽干成 `键 → 行`);
//! * **最终结果**([`execute`] 抽干 root 的那一步).
//!
//! 索引嵌套循环**不在这张名单里**:它只建一份"键 → 行号"的索引,内侧的行按需
//! 从基表取(见 [`crate::db::index`]).
//!
//! 于是 `LIMIT` 之下不再被完整求值,连接不再为每一对候选行拼整行--详见
//! `LimitSource`,`JoinSource`(嵌套循环),`HashJoinSource` 与 `IndexJoinSource`.
//!
//! # 只从 root 出发
//!
//! 管道的构建与 `explain` 的遍历口径一致:从 root 走不到的节点**既不建源也不执行**
//! (静态校验 [`Plan::analyze`] 仍然覆盖它们).这是拉取式的直接推论,也让
//! "不可达节点"这件事与文档一致.
//!
//! # 被多个父节点引用的节点
//!
//! 计划是 DAG:一个节点可以被多个父节点引用.可重扫的游标做不到"拉到一半给别人再从头",
//! 所以这类节点会被**物化一次**,每个消费者各拿一个 `SharedCursor`.
//!
//! # 子查询:表达式层的一层求值环境
//!
//! 内层计划是一份**自足**的 `Plan`,装在 `Step::Subquery` 里;相关引用是
//! `Step::Outer`.所以这里只需要一个 `ExecCtx`:
//!
//! * 求值 `Step::Subquery` 时把"本程序面对的那一行"压进**外层行栈**
//!   (`Step::Outer` 从这里取),然后**再调一次 `build_pipeline`** 造一份内层源;
//! * 未相关(`SubPlan::correlated == false`)的内层只算一次,按 [`SubMode`] 缓存的
//!   **形态不同**(`SubCache`):`EXISTS` 只留一个真值,标量只留那个结果,`IN` 留
//!   内层那一列的值集合,其余量化才留行.相关的每行重开一份.注意**不能缓存"这个
//!   子查询的真值"**--`x IN (SELECT ...)` 的左半边逐行不同,缓存真值会安静地给错
//!   答案.
//!
//! 每个 `RowSource` 因此都带着 `Rc<ExecCtx>`,程序一律走 [`Prog::eval_with`];
//! 只有"键不读列"的索引路径还用它自己的无环境求值.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::db::index::{IndexSlot, IndexSource};
use crate::db::phys::PhysTab;
use crate::db::source::{self, RowSource, TableCursor};
use crate::db::table::{
    Column, ColumnsShape, DataType, DbError, DecimalOp, Result, Row, Table, TabShape, Value,
};

use super::expr::{apply, BinOp, Env, EvalError, Prog, SubMode, SubPlan};
use super::value::{Scalar, Tri};
use super::plan::{
    passthrough_column, AggFunc, AggSpec, Analysis, EqKey, IndexPath, JoinKind, JoinStrategy,
    NodeId, Op, Plan, SortKey,
};
use super::symbol::SymbolTable;

/// 构建拉取式的执行管道,返回 root 的行源.
///
/// 执行前会先跑一次 [`Plan::analyze`],所以坏 IR 会在任何数据被碰之前就被拒绝.
/// **拉多少行,什么时候停由调用方决定**:只要不再调 `next()`,下层就不会被拉
/// (`Limit` 之下,甚至 `Sort` 的物化都可以因此不发生).
///
/// 需要"算完整个结果"就用 [`execute`],或自己用
/// [`crate::db::source::materialize`] 抽干.
pub fn execute_source<'a>(
    plan: &'a Plan,
    symbols: &'a SymbolTable,
) -> Result<Box<dyn RowSource + 'a>> {
    let analysis = plan.analyze(symbols)?;
    let ctx = Rc::new(ExecCtx::new(symbols));
    build_pipeline(plan, &analysis, &ctx)
}

/// 造管道的本体.
///
/// 子查询也走它:内层计划是一份**自足**的 `Plan`,所以"为外层每一行重开一份内层源"
/// 就是带着同一个 [`ExecCtx`] 再调一次这里--这正是拉取式执行让相关子查询变成自然事
/// 的地方.
///
/// `'p` 是计划的生命周期(返回的源借用它),`'s` 是符号表的--两者互不相干,
/// 于是内层计划(借用自 `Step::Subquery`)与执行环境可以各用各的.
fn build_pipeline<'p, 's>(
    plan: &'p Plan,
    analysis: &Analysis,
    ctx: &Rc<ExecCtx<'s>>,
) -> Result<Box<dyn RowSource + 'p>>
where
    's: 'p,
{
    // 只从 root 出发:走不到的节点不建源,不执行.
    let reachable = reachable_from(plan);

    // 在可达集合里数每个节点被几个父节点引用:>1 的节点要物化一次给所有人共享.
    let mut consumers = vec![0usize; plan.nodes.len()];
    for (id, node) in plan.nodes.iter().enumerate() {
        if reachable[id] {
            for &input in &node.inputs {
                consumers[input] += 1;
            }
        }
    }

    let mut done: Vec<Option<Slot<'p>>> = (0..plan.nodes.len()).map(|_| None).collect();

    for (id, node) in plan.nodes.iter().enumerate() {
        if !reachable[id] {
            continue;
        }

        // schema 由 `Plan::analyze` 统一推导(透传列保留原类型),这里不再自己猜
        let schema = ColumnsShape::new(analysis.schemas[id].to_vec());

        let source: Box<dyn RowSource + 'p> = match &node.op {
            Op::SingleRow => Box::new(SingleRowSource::new()),

            // `SELECT COUNT(*) FROM t`(单张基表,没有 `WHERE` / `GROUP BY`):
            // 一行都不读,直接问表的行数(TODO 3.8).
            Op::CountRows { table, name } => {
                let relation =
                    ctx.symbols
                        .relation(table)
                        .ok_or_else(|| DbError::UnknownSymbol {
                            name: table.clone(),
                        })?;
                Box::new(CountRowsSource::new(relation, name, id)?)
            }

            Op::Scan {
                table,
                filter,
                index,
            } => {
                // `analyze` 已经查过这个名字,这里只是把表取出来
                let relation =
                    ctx.symbols
                        .relation(table)
                        .ok_or_else(|| DbError::UnknownSymbol {
                            name: table.clone(),
                        })?;
                // 访问路径:索引(按键取行号)或顺序扫描
                let base: Box<dyn RowSource + 'p> = match index {
                    Some(path) => {
                        // 目录里有这一列的索引(`CREATE INDEX` 建的那一份)就用它--
                        // 反复查询共用同一份,不重建.没有就临时建一个本地槽位:
                        // 手写计划,或者降级之后索引被删掉时会走到这里.**只是慢,
                        // 不会算错**--谓词照旧逐行复核.
                        let slot = ctx
                            .symbols
                            .index_of(table, path.column)
                            .unwrap_or_else(|| Rc::new(IndexSlot::new(path.column, false, String::new())));
                        Box::new(IndexSource::new(
                            relation,
                            slot,
                            index_keys(path, ctx, id)?,
                        ))
                    }
                    None => Box::new(TableCursor::new(relation)),
                };
                Box::new(ScanSource::new(base, filter.as_ref(), ctx.clone(), id))
            }

            Op::Filter { predicate } => Box::new(FilterSource::new(
                input(&mut done, node.inputs[0])?,
                predicate,
                ctx.clone(),
                id,
            )),

            Op::Project { progs, .. } => Box::new(ProjectSource::new(
                input(&mut done, node.inputs[0])?,
                progs,
                schema,
                ctx.clone(),
                id,
            )),

            Op::Join {
                kind,
                on,
                strategy,
            } => {
                let left = input(&mut done, node.inputs[0])?;

                match strategy {
                    JoinStrategy::NestedLoop => Box::new(JoinSource::new(
                        left,
                        input(&mut done, node.inputs[1])?,
                        *kind,
                        on.as_ref(),
                        schema,
                        ctx.clone(),
                        id,
                    )),

                    JoinStrategy::Hash { keys } => Box::new(HashJoinSource::new(
                        left,
                        input(&mut done, node.inputs[1])?,
                        *kind,
                        on.as_ref(),
                        keys,
                        schema,
                        ctx.clone(),
                        id,
                    )),

                    JoinStrategy::IndexNestedLoop { key } => {
                        // 内侧是基表 `Scan`(`analyze` 保证),索引直接建在基表上,
                        // 于是**内侧的源根本不建,不抽干**.
                        let (table, filter) = match &plan.nodes[node.inputs[1]].op {
                            Op::Scan { table, filter, .. } => (table, filter.as_ref()),
                            _ => {
                                return Err(runtime(
                                    id,
                                    tr!("ir.exec.001").into_owned(),
                                ))
                            }
                        };
                        let relation = ctx.symbols.relation(table).ok_or_else(|| {
                            DbError::UnknownSymbol {
                                name: table.clone(),
                            }
                        })?;
                        let column = passthrough_column(&key.inner).ok_or_else(|| {
                            runtime(id, tr!("ir.exec.002").into_owned())
                        })?;

                        let slot = ctx
                            .symbols
                            .index_of(table, column)
                            .unwrap_or_else(|| Rc::new(IndexSlot::new(column, false, String::new())));
                        Box::new(IndexJoinSource::new(
                            left,
                            relation,
                            slot,
                            filter,
                            key,
                            *kind,
                            on.as_ref(),
                            schema,
                            ctx.clone(),
                            id,
                        ))
                    }
                }
            }

            Op::Aggregate {
                group_by, aggs, ..
            } => Box::new(AggregateSource::new(
                input(&mut done, node.inputs[0])?,
                group_by,
                aggs,
                schema,
                ctx.clone(),
                id,
            )),

            Op::Sort { keys } => Box::new(SortSource::new(
                input(&mut done, node.inputs[0])?,
                keys,
                schema,
                ctx.clone(),
                id,
            )),

            Op::Limit { limit, offset } => Box::new(LimitSource::new(
                input(&mut done, node.inputs[0])?,
                *limit,
                *offset,
            )),

            Op::Distinct => Box::new(DistinctSource::new(input(&mut done, node.inputs[0])?)),

            Op::Union { all } => Box::new(UnionSource::new(
                input(&mut done, node.inputs[0])?,
                input(&mut done, node.inputs[1])?,
                *all,
                schema,
            )),
        };

        done[id] = Some(if consumers[id] > 1 {
            // 被多个父节点引用:重扫的代价比分发一份物化结果高,所以在这里物化**一次**.
            let mut source = source;
            Slot::Shared(Rc::new(source::materialize(&mut *source)?))
        } else {
            Slot::Source(source)
        });
    }

    match done[plan.root].take() {
        Some(Slot::Source(source)) => Ok(source),
        Some(Slot::Shared(table)) => Ok(Box::new(SharedCursor::new(table))),
        None => Err(runtime(plan.root, tr!("ir.exec.003").into_owned())),
    }
}

// ---------------------------------------------------------- 求值环境(子查询)

/// 一次执行共享的求值环境.
///
/// 两件事:
///
/// * **外层行栈**:相关子查询的 [`Step::Outer`] 从这里取.压一行的时机就是
///   [`Step::Subquery`] 求值的那一刻(压进去的是"本程序面对的那一行",也就是
///   内层的外层行),内层跑完就弹回来;
/// * **内层的缓存**:内层计划的 [`Analysis`] 总是只算一次(分析是纯的,查询期间
///   符号表不动);**未相关**子查询的内层也只算一次--它与外层哪一行无关,而
///   相关的必须每行重开一份源.按 [`SubMode`] 缓存的**形态不同**([`SubCache`]).
///
/// 缓存按 `SubPlan` 的地址认:计划在一次执行里是稳定的,指针也就稳定.
///
/// > 注意**不能缓存"这个子查询的真值"**:`x IN (SELECT ...)` 的左半边是外层
/// > 表达式,逐行不同;把最终真值缓存下来会安静地给出错答案(这是实测踩过的坑).
/// > `IN` 缓存的是内层那一列的**值集合**--它与左半边无关,是对的.
struct ExecState {
    /// 外层行栈:`[0]` 是最外层,`last()` 是最近一层.
    outers: RefCell<Vec<Row>>,
    /// 未相关子查询算过一次之后留下的东西,按 [`SubMode`] 分形态.
    sub_cache: RefCell<HashMap<*const SubPlan, SubCache>>,
    /// 子查询内层计划的 `Analysis`(`Rc` 是为了给多个源共享).
    analysis_cache: RefCell<HashMap<*const SubPlan, Rc<Analysis>>>,
}

/// 一个未相关子查询算过一次之后留下的东西(TODO 3.9).
///
/// 老写法一律把内层抽干成 `Vec<Row>`,于是 [`combine_subquery`] 自己的短路白搭:
/// `EXISTS` 只要一行却把整张表读进来,`x IN (...)` 的每一行都在那堆行上线性扫
/// (O(外层行数 x 内层行数)),标量子查询也要两行就能判"多于一行"却全抽干.
/// 这里按 [`SubMode`] 只留真正需要的那一点.
enum SubCache {
    /// `EXISTS`:未相关时真值是**常量**,一行都不用留.
    Exists(Tri),
    /// 标量:未相关时结果就是常量(`Value::Null` = 内层没有行).
    Scalar(Scalar),
    /// `= ANY`(就是 `IN`):内层那一列的**值集合**,逐外层行 O(1).
    In(ValueSet),
    /// 其余量化(非等值比较 / `ALL`):内层的行,逐行比.
    Rows(Rc<Vec<Row>>),
}

impl SubCache {
    /// 按模式把这份缓存收成一个标量.
    ///
    /// 形态与模式由建它的人([`ExecCtx::build_cache`])一处配对,所以这里不需要
    /// `spec.mode` 的每一种;`Rows` 那一种正是给"只能逐行比"的模式准备的,
    /// 于是它照旧交给 [`combine_subquery`].
    fn answer(
        &self,
        spec: &SubPlan,
        test: Option<Scalar>,
    ) -> std::result::Result<Scalar, EvalError> {
        match self {
            SubCache::Exists(truth) => Ok(Scalar::Truth(*truth)),
            SubCache::Scalar(value) => Ok(value.clone()),
            SubCache::In(set) => set.answer(test),
            SubCache::Rows(rows) => combine_subquery(&spec.mode, test, &mut rows.iter().cloned()),
        }
    }
}

/// `x = ANY (SELECT ...)`(也就是 `IN`)的内层**值集合**.
///
/// 表里的值一个都不复制给外层用:外层每一行只需 O(1) 的一次查找.
#[derive(Default)]
struct ValueSet {
    keys: HashSet<InKey>,
    /// 内层有没有 NULL(`x IN (没命中但有 NULL 的集合)` 是 UNKNOWN,不是 FALSE).
    has_null: bool,
    /// 内层有多少行:空集与"只有 NULL"在 `ANY` 的语义下**不同**
    /// (空集是 FALSE,有一行 NULL 是 UNKNOWN).
    nrows: usize,
}

impl ValueSet {
    /// 收一个内层值.`NULL` 也是"一行",只是它不成为一个键.
    fn push(&mut self, value: Value) {
        self.nrows += 1;
        match in_key(&value) {
            Some(key) => {
                self.keys.insert(key);
            }
            None => self.has_null = true,
        }
    }

    /// `外层表达式 = ANY (这一个集合)` 的答案(三值).
    fn answer(&self, test: Option<Scalar>) -> std::result::Result<Scalar, EvalError> {
        // 空集:`ANY` 在空集上是 FALSE(SQL 标准)
        if self.nrows == 0 {
            return Ok(Scalar::Truth(Tri::False));
        }

        let test = test.unwrap_or(Scalar::Value(Value::Null));
        let value = match test {
            Scalar::Value(value) => value,
            // 真值当不了比较的一侧(`verify` 早就拦掉了;这里宁可报错)
            Scalar::Truth(_) => return Err(EvalError::MixedTypes),
        };
        // 与 NULL 比较永远是 UNKNOWN,整个 `ANY` 也是 UNKNOWN
        let Some(key) = in_key(&value) else {
            return Ok(Scalar::Truth(Tri::Unknown));
        };

        match (self.keys.contains(&key), self.has_null) {
            (true, _) => Ok(Scalar::Truth(Tri::True)),
            (false, true) => Ok(Scalar::Truth(Tri::Unknown)),
            (false, false) => Ok(Scalar::Truth(Tri::False)),
        }
    }
}

/// `IN` 的哈希键.
///
/// `INT` 与 `NUMERIC` 在 [`Value`] 层面**不是同一个值**(`Int(1)` 与
/// `Numeric("1")`),而运行时的比较会把 `Int` 放大成 `Numeric`(见 `expr::apply`
/// 与 `widen_numeric`)--直接拿 `Value` 当键就会**安静地漏掉**本该成立的那些行
/// (`x IN (SELECT n)` 里 `x` 是 `INT`,`n` 是 `NUMERIC` 就是这种写法).所以数值族
/// 统一归到**规范十进制文本**上;文本的规范化是 `Value` 的不变量(见
/// [`crate::db::table`] 的 `Numeric`),于是"文本相等"与"数值相等"是同一件事.
///
/// 别的大类原样:`Value` 的 `Hash` 与比较同族,而同族是类型检查放行的唯一一种
/// 相等(比较只放行同族,外加 `INT` ↔ `NUMERIC`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum InKey {
    /// 数值族:`Int` 与 `Numeric` 都归到规范十进制文本上.
    Number(String),
    /// 别的大类:原样.
    Value(Value),
}

/// 一个值在这个键空间里的样子;`NULL` 没有键(它由 [`ValueSet::has_null`] 记着).
fn in_key(value: &Value) -> Option<InKey> {
    match value {
        Value::Null => None,
        Value::Int(i) => Some(InKey::Number(i.to_string())),
        Value::Numeric(text) => Some(InKey::Number(text.clone())),
        other => Some(InKey::Value(other.clone())),
    }
}

/// 求值环境本体:符号表 + 共享状态.
struct ExecCtx<'s> {
    symbols: &'s SymbolTable,
    state: Rc<ExecState>,
}

impl<'s> ExecCtx<'s> {
    fn new(symbols: &'s SymbolTable) -> Self {
        ExecCtx {
            symbols,
            state: Rc::new(ExecState {
                outers: RefCell::new(Vec::new()),
                sub_cache: RefCell::new(HashMap::new()),
                analysis_cache: RefCell::new(HashMap::new()),
            }),
        }
    }

    /// 子查询内层计划的 `Analysis`(算过一次就留着).
    ///
    /// 用**浅层**分析:内层计划的深层校验(外层链对不对)在外层 `analyze` 里做过,
    /// 那里才知道它压着哪几层行.
    fn analysis_of(&self, spec: &SubPlan) -> Result<Rc<Analysis>> {
        let key = spec as *const SubPlan;
        if let Some(analysis) = self.state.analysis_cache.borrow().get(&key) {
            return Ok(analysis.clone());
        }
        let analysis = Rc::new(spec.plan.analyze_shallow(self.symbols)?);
        self.state
            .analysis_cache
            .borrow_mut()
            .insert(key, analysis.clone());
        Ok(analysis)
    }

    /// 求值一个内层子查询:压一行外层行 → 求值 → 弹回来.
    fn run_subquery(
        &self,
        spec: &SubPlan,
        row: &[Value],
    ) -> std::result::Result<Scalar, EvalError> {
        self.state.outers.borrow_mut().push(row.to_vec());
        let outcome = self.evaluate(spec, row);
        self.state.outers.borrow_mut().pop();
        outcome
    }

    /// 按模式把内层结果收成一个标量.
    fn evaluate(
        &self,
        spec: &SubPlan,
        row: &[Value],
    ) -> std::result::Result<Scalar, EvalError> {
        // `IN` / `= ANY` / `= ALL` 的左半边是**外层**表达式,按当前行求值
        let test = match &spec.mode {
            SubMode::Quantified { test, .. } => Some(test.eval_with(row, &[], self)?),
            _ => None,
        };

        let analysis = self
            .analysis_of(spec)
            .map_err(|e| EvalError::Nested(Box::new(e)))?;

        // 未相关:内层与外面哪一行无关,算一次,按模式缓存起来(TODO 3.9)
        if !spec.correlated {
            let key = spec as *const SubPlan;
            {
                let cached = self.state.sub_cache.borrow();
                if let Some(cache) = cached.get(&key) {
                    return cache.answer(spec, test);
                }
            }
            // 建缓存失败(内层报错 / 标量多于一行)就**不入缓存**:下一行照旧报同一个错
            let cache = self.build_cache(spec, &analysis)?;
            let answer = cache.answer(spec, test)?;
            self.state.sub_cache.borrow_mut().insert(key, cache);
            return Ok(answer);
        }

        // 相关:内层每行重开一份源(拉取式之下只是多调一次构建)
        let inner_ctx = Rc::new(ExecCtx {
            symbols: self.symbols,
            state: self.state.clone(),
        });
        let mut source = build_pipeline(&spec.plan, &analysis, &inner_ctx)
            .map_err(|e| EvalError::Nested(Box::new(e)))?;

        let value = {
            let mut rows = std::iter::from_fn(|| source.next());
            combine_subquery(&spec.mode, test, &mut rows)?
        };

        // 拉完(或提前收手)之后再看一眼:`None` 也可能是出错
        match source.error() {
            Some(e) => Err(EvalError::Nested(Box::new(e.clone()))),
            None => Ok(value),
        }
    }

    /// 把一个未相关子查询算一次,按 [`SubMode`] 收成最省的形态(TODO 3.9).
    ///
    /// 各模式**只取它真正需要的那一点**:
    ///
    /// * `EXISTS` -- 一行就够(真值是常量);
    /// * 标量 -- 最多两行(第二行就是"多于一行"的错,与 PostgreSQL 一样在第二行收手);
    /// * `= ANY`(`IN`)-- 内层那一列的值(`spec.arity == 1` 由降级保证);
    /// * 其余量化 -- 内层的行,没别的办法.
    fn build_cache(
        &self,
        spec: &SubPlan,
        analysis: &Analysis,
    ) -> std::result::Result<SubCache, EvalError> {
        let inner_ctx = Rc::new(ExecCtx {
            symbols: self.symbols,
            state: self.state.clone(),
        });
        let mut source = build_pipeline(&spec.plan, analysis, &inner_ctx)
            .map_err(|e| EvalError::Nested(Box::new(e)))?;

        let cache = match &spec.mode {
            SubMode::Exists => {
                let some = source.next().is_some();
                SubCache::Exists(if some { Tri::True } else { Tri::False })
            }

            SubMode::Scalar => {
                let first = source.next();
                // 多于一行是**运行期**错误(SQL 要求标量子查询最多给一行)
                if first.is_some() && source.next().is_some() {
                    return Err(EvalError::ScalarSubqueryRows { rows: 2 });
                }
                let value = first
                    .and_then(|row| row.into_iter().next())
                    .map(Scalar::Value)
                    .unwrap_or(Scalar::Value(Value::Null));
                SubCache::Scalar(value)
            }

            // `IN` 与 `= ANY` 是同一件事(`lower_sublink` 把两者都编成
            // `op: Eq, all: false`);别的比较与 `ALL` 走下面那一支.
            SubMode::Quantified {
                op: BinOp::Eq,
                all: false,
                ..
            } => {
                let mut set = ValueSet::default();
                while let Some(row) = source.next() {
                    set.push(row.into_iter().next().unwrap_or(Value::Null));
                }
                SubCache::In(set)
            }

            SubMode::Quantified { .. } => {
                let mut rows = Vec::new();
                while let Some(row) = source.next() {
                    rows.push(row);
                }
                SubCache::Rows(Rc::new(rows))
            }
        };

        // 提前收手也可能是出错:`None` 有两个意思(正常结束 / 出错)
        match source.error() {
            Some(e) => Err(EvalError::Nested(Box::new(e.clone()))),
            None => Ok(cache),
        }
    }
}

/// 按 [`SubMode`] 把内层的结果行收成一个标量.
///
/// 空集的语义与 SQL 标准一致:标量 → NULL,`EXISTS` → 假,`ALL` → **真**,
/// `ANY` → **假**.`EXISTS` 与量化比较**见到能定胜负的行就停**(不在这一层拉干内层).
fn combine_subquery(
    mode: &SubMode,
    test: Option<Scalar>,
    rows: &mut dyn Iterator<Item = Row>,
) -> std::result::Result<Scalar, EvalError> {
    match mode {
        SubMode::Scalar => {
            let first = rows.next();
            // 多于一行 → 运行期错误(SQL 要求标量子查询最多给一行)
            if first.is_some() && rows.next().is_some() {
                return Err(EvalError::ScalarSubqueryRows { rows: 2 });
            }
            Ok(first
                .and_then(|row| row.into_iter().next())
                .map(Scalar::Value)
                .unwrap_or(Scalar::Value(Value::Null)))
        }

        SubMode::Exists => Ok(Scalar::Truth(if rows.next().is_some() {
            Tri::True
        } else {
            Tri::False
        })),

        SubMode::Quantified { op, all, .. } => {
            let test = test.unwrap_or(Scalar::Value(Value::Null));
            let mut truth = if *all { Tri::True } else { Tri::False };

            for row in rows {
                let value = row
                    .into_iter()
                    .next()
                    .map(Scalar::Value)
                    .unwrap_or(Scalar::Value(Value::Null));
                let step = apply(*op, test.clone(), value)?
                    .as_truth()
                    .ok_or(EvalError::NotABool)?;
                truth = if *all { truth.and(step) } else { truth.or(step) };

                // 定胜负就收工(`ALL` 见到假,`ANY` 见到真)
                if (*all && truth == Tri::False) || (!*all && truth == Tri::True) {
                    break;
                }
            }

            Ok(Scalar::Truth(truth))
        }
    }
}

impl Env for ExecCtx<'_> {
    fn outer(&self, depth: u16, index: usize) -> std::result::Result<Scalar, EvalError> {
        let stack = self.state.outers.borrow();
        let at = depth
            .checked_sub(1)
            .and_then(|d| stack.len().checked_sub(d as usize + 1))
            .ok_or(EvalError::NoEnv {
                what: tr!("ir.exec.004").into_owned(),
            })?;
        let row = &stack[at];
        row.get(index)
            .cloned()
            .map(Scalar::Value)
            .ok_or(EvalError::ColumnOutOfRange {
                index,
                arity: row.len(),
            })
    }

    fn subquery(&self, spec: &SubPlan, row: &[Value]) -> std::result::Result<Scalar, EvalError> {
        self.run_subquery(spec, row)
    }
}

/// 执行一个线性计划,抽干 root 的行源,返回结果表.
///
/// 这是"算完整个结果"的入口;想自己控制拉多少行,什么时候停,用 [`execute_source`].
pub fn execute(plan: &Plan, symbols: &SymbolTable) -> Result<PhysTab> {
    let mut source = execute_source(plan, symbols)?;
    source::materialize(&mut *source)
}

/// 一个节点的产物.
///
/// 通常就是一个源(只被一个父节点消费);被多个父节点引用时,在这里物化一次,
/// 每个消费者各拿一个 [`SharedCursor`].
enum Slot<'a> {
    /// 还没被消费的行源.
    Source(Box<dyn RowSource + 'a>),
    /// 已经物化好的共享结果.
    Shared(Rc<PhysTab>),
}

/// 取一个节点的输出当输入用.
///
/// `Shared` 是**克隆 `Rc`**(几个消费者都要读同一份),`Source` 是**取走**(一个源只
/// 能被消费一次).
fn input<'a>(done: &mut [Option<Slot<'a>>], id: NodeId) -> Result<Box<dyn RowSource + 'a>> {
    let slot = done
        .get_mut(id)
        .ok_or_else(|| runtime(id, format!("{}", tr!("ir.exec.005", arg0 =  id))))?;

    if let Some(Slot::Shared(table)) = slot {
        return Ok(Box::new(SharedCursor::new(Rc::clone(table))));
    }

    match slot.take() {
        Some(Slot::Source(source)) => Ok(source),
        // 构造上不可达:`analyze` 保证输入下标严格小于自身下标.
        _ => Err(runtime(id, format!("{}", tr!("ir.exec.006", arg0 =  id)))),
    }
}

/// 从 root 出发,顺着 `inputs` 能走到的节点.
///
/// 显式栈,不递归.I1 保证输入只指向过去,所以一定会停.
fn reachable_from(plan: &Plan) -> Vec<bool> {
    let mut reachable = vec![false; plan.nodes.len()];
    let mut stack = vec![plan.root];

    while let Some(id) = stack.pop() {
        if std::mem::replace(&mut reachable[id], true) {
            continue;
        }
        stack.extend(plan.nodes[id].inputs.iter().copied());
    }

    reachable
}

/// 共享物化结果的上游游标:每个消费者一个,各自从第 0 行开始.
struct SharedCursor {
    table: Rc<PhysTab>,
    at: usize,
}

impl SharedCursor {
    fn new(table: Rc<PhysTab>) -> Self {
        SharedCursor { table, at: 0 }
    }
}

impl RowSource for SharedCursor {
    fn name(&self) -> &str {
        self.table.name()
    }

    fn columns(&self) -> &[Column] {
        self.table.columns()
    }

    fn next(&mut self) -> Option<Row> {
        let row = self.table.row(self.at)?;
        self.at += 1;
        Some(row)
    }
}

/// 一张 1 行 0 列的关系.`SELECT 1 + 1` 这种没有 FROM 的查询拿它当底座.
struct SingleRowSource {
    done: bool,
}

impl SingleRowSource {
    fn new() -> Self {
        SingleRowSource { done: false }
    }
}

impl RowSource for SingleRowSource {
    fn name(&self) -> &str {
        "<single row>"
    }

    fn columns(&self) -> &[Column] {
        &[]
    }

    fn next(&mut self) -> Option<Row> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(Vec::new())
    }
}

/// 一张表的**行数**,一行都不读(TODO 3.8).
///
/// `SELECT COUNT(*) FROM t`(单张基表,没有 `WHERE` / `GROUP BY`)走它:
/// `Table::nrows()` 是现成的事实,为了一行答案把整张表拉一遍再数一遍是白干.
/// 取行数不读任何一行(`nrows` 是 O(1) 的),所以这里没有"拉"这个动作.
struct CountRowsSource<'a> {
    /// 那一张表:只为 [`RowSource::name`] 与错误报文留着.
    table: &'a dyn Table,
    /// 输出 schema:一列 `INT`,名字与 `Aggregate` 那条路一样(`count`).
    columns: Vec<Column>,
    /// 那一行答案;吐过一次就没有了.
    row: Option<Row>,
}

impl<'a> CountRowsSource<'a> {
    /// `relation.nrows()` 装不进 `INT` 时报错(与 `Aggregate` 那条路同一句:
    /// [`AggFunc::Count`](crate::ir::plan::AggFunc) 也是 `i32::try_from`).
    fn new(relation: &'a dyn Table, name: &str, node: NodeId) -> Result<Self> {
        let count = i32::try_from(relation.nrows())
            .map_err(|_| runtime(node, tr!("ir.exec.021").into_owned()))?;
        Ok(CountRowsSource {
            table: relation,
            columns: vec![Column::new(name, DataType::Int)],
            row: Some(vec![Value::Int(count)]),
        })
    }
}

impl RowSource for CountRowsSource<'_> {
    fn name(&self) -> &str {
        self.table.name()
    }

    fn columns(&self) -> &[Column] {
        &self.columns
    }

    fn next(&mut self) -> Option<Row> {
        self.row.take()
    }
}

/// 扫描:**访问路径**(顺序游标或索引)逐行给行,再逐行跑下推的谓词.
///
/// 索引只是访问路径:谓词照旧逐行求值,所以路径挑错也只是慢,不会算错.
struct ScanSource<'a> {
    /// 访问路径:[`TableCursor`] (顺序)或 [`IndexSource`] (索引).
    base: Box<dyn RowSource + 'a>,
    filter: Option<&'a Prog>,
    /// 求值环境(外层行栈 + 子查询缓存).
    ctx: Rc<ExecCtx<'a>>,
    node: NodeId,
    error: Option<DbError>,
}

impl<'a> ScanSource<'a> {
    /// `base` 给出的是**这张表自己的行**.
    fn new(
        base: Box<dyn RowSource + 'a>,
        filter: Option<&'a Prog>,
        ctx: Rc<ExecCtx<'a>>,
        node: NodeId,
    ) -> Self {
        ScanSource {
            base,
            filter,
            ctx,
            node,
            error: None,
        }
    }
}

impl RowSource for ScanSource<'_> {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn columns(&self) -> &[Column] {
        self.base.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        // 没有下推的谓词:直接拉一行(借一眼没人判,白花一次 `row_ref`)
        if self.filter.is_none() {
            return self.base.next();
        }

        loop {
            // 先借一眼(TODO 3.19):谓词在借来的值上跑,被丢掉的行**一个值都不克隆**
            if let Some(borrowed) = self.base.peek() {
                match self.keep(borrowed) {
                    Ok(true) => return self.base.next(),
                    Ok(false) => {
                        self.base.skip();
                        continue;
                    }
                    Err(e) => {
                        self.error = Some(e);
                        return None;
                    }
                }
            }

            // 借不出来(索引路径,磁盘基表):退回"先整行克隆,再求值"那条老路
            let row = self.base.next()?;
            match self.keep(&row) {
                Ok(true) => return Some(row),
                Ok(false) => {}
                Err(e) => {
                    self.error = Some(e);
                    return None;
                }
            }
        }
    }

    fn peek(&self) -> Option<&[Value]> {
        // 自己能借的那一条:只有**没有自己的谓词**时才等于"下一行"--
        // 有谓词就得逐行判,那要看下一行,借不出来
        match self.filter {
            None => self.base.peek(),
            Some(_) => None,
        }
    }

    fn skip(&mut self) {
        // 没有自己的谓词时,丢掉一行的意思就是"让下面挪一格"(一个值都不克隆);
        // 有谓词时下面那条路要看一眼才知道留不留,退回 `next`
        match self.filter {
            None => self.base.skip(),
            Some(_) => {
                let _ = self.next();
            }
        }
    }

    fn error(&self) -> Option<&DbError> {
        // 访问路径的错误(短读,建索引失败)在 `base` 里,谓词的错误在这里
        self.error.as_ref().or_else(|| self.base.error())
    }
}

impl ScanSource<'_> {
    /// 这一行留不留(`None` 表示没有下推的谓词 = 都留).
    fn keep(&self, row: &[Value]) -> std::result::Result<bool, DbError> {
        let Some(prog) = self.filter else {
            return Ok(true);
        };
        truth_of(prog, row, &[], &self.ctx)
            .map(|truth| truth.is_true())
            .map_err(|e| e.into_db_error(Some(self.node)))
    }
}

/// 选择.
struct FilterSource<'a> {
    input: Box<dyn RowSource + 'a>,
    predicate: &'a Prog,
    /// 谓词是**恒不成立**的常量吗(`Filter([false])` / `Filter([NULL])`).
    ///
    /// 是就一行都不拉:恒假谓词丢得掉每一行,而"每一行都要来一趟再被丢掉"正是
    /// `WHERE 1 = 0` 白干的地方(TODO 3.7).只认常量--`Step::And` 的惰性仍然
    /// 由求值自己保证,这里不替它做决定.
    empty: bool,
    /// 求值环境(外层行栈 + 子查询缓存).
    ctx: Rc<ExecCtx<'a>>,
    node: NodeId,
    error: Option<DbError>,
}

impl<'a> FilterSource<'a> {
    fn new(
        input: Box<dyn RowSource + 'a>,
        predicate: &'a Prog,
        ctx: Rc<ExecCtx<'a>>,
        node: NodeId,
    ) -> Self {
        let empty = matches!(predicate.as_const(), Some(Scalar::Truth(t)) if !t.is_true());
        FilterSource {
            input,
            predicate,
            empty,
            ctx,
            node,
            error: None,
        }
    }
}

impl RowSource for FilterSource<'_> {
    fn name(&self) -> &str {
        "<filter>"
    }

    fn columns(&self) -> &[Column] {
        // 选择不改变 schema
        self.input.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() || self.empty {
            return None;
        }

        loop {
            // 与 `ScanSource` 同一条口径:借得到就先判,丢掉的行不克隆(TODO 3.19)
            if let Some(borrowed) = self.input.peek() {
                match self.keep(borrowed) {
                    Ok(true) => return self.input.next(),
                    Ok(false) => {
                        self.input.skip();
                        continue;
                    }
                    Err(e) => {
                        self.error = Some(e);
                        return None;
                    }
                }
            }

            let row = self.input.next()?;
            match self.keep(&row) {
                Ok(true) => return Some(row),
                Ok(_) => {}
                Err(e) => {
                    self.error = Some(e);
                    return None;
                }
            }
        }
    }

    fn error(&self) -> Option<&DbError> {
        self.error.as_ref().or_else(|| self.input.error())
    }
}

impl FilterSource<'_> {
    /// 这一行留不留.
    fn keep(&self, row: &[Value]) -> std::result::Result<bool, DbError> {
        truth_of(self.predicate, row, &[], &self.ctx)
            .map(|truth| truth.is_true())
            .map_err(|e| e.into_db_error(Some(self.node)))
    }
}

/// 投影.
struct ProjectSource<'a> {
    input: Box<dyn RowSource + 'a>,
    progs: &'a [Prog],
    shape: ColumnsShape,
    /// 求值环境(外层行栈 + 子查询缓存).
    ctx: Rc<ExecCtx<'a>>,
    node: NodeId,
    error: Option<DbError>,
}

impl<'a> ProjectSource<'a> {
    fn new(
        input: Box<dyn RowSource + 'a>,
        progs: &'a [Prog],
        shape: ColumnsShape,
        ctx: Rc<ExecCtx<'a>>,
        node: NodeId,
    ) -> Self {
        ProjectSource {
            input,
            progs,
            shape,
            ctx,
            node,
            error: None,
        }
    }
}

impl RowSource for ProjectSource<'_> {
    fn name(&self) -> &str {
        "<project>"
    }

    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        let row = self.input.next()?;
        let mut out = Row::with_capacity(self.progs.len());

        for prog in self.progs {
            let scalar = match prog.eval_with(&row, &[], &*self.ctx) {
                Ok(scalar) => scalar,
                Err(e) => {
                    self.error = Some(e.into_db_error(Some(self.node)));
                    return None;
                }
            };
            let value = match scalar.to_value() {
                Some(value) => value,
                // 静态校验已经挡住了真值作结果列,这里只是不让它变成长得一样的结果
                None => {
                    self.error = Some(runtime(
                        self.node,
                        format!("{}", tr!("ir.exec.007", arg0 =  scalar)),
                    ));
                    return None;
                }
            };
            out.push(value);
        }

        Some(out)
    }

    fn error(&self) -> Option<&DbError> {
        self.error.as_ref().or_else(|| self.input.error())
    }
}

/// 嵌套循环连接.
///
/// 输出行是 `左行 ++ 右行`(与 `Join` 的 schema 规则严格一致),但**只在命中时才拼**:
/// `ON` 用 [`Prog::eval_truth_split`] 在 `(左行, 右行)` 上直接求值,不为每一对候选行
/// 拼一整行.
///
/// 内侧(右支)必须能为每个左行重扫一遍,而游标只能往前--所以它在第一次拉取时被
/// **抽干物化一次**;左支是流式的.
struct JoinSource<'a> {
    left: Box<dyn RowSource + 'a>,
    /// 内侧(右支).第一次拉取时被抽干,之后丢掉它的缓冲由 `inner` 接管.
    right: Option<Box<dyn RowSource + 'a>>,
    kind: JoinKind,
    on: Option<&'a Prog>,
    shape: ColumnsShape,
    /// 求值环境(外层行栈 + 子查询缓存).
    ctx: Rc<ExecCtx<'a>>,
    node: NodeId,
    error: Option<DbError>,
    /// 物化好的内侧;`None` 表示还没物化.
    inner: Option<PhysTab>,
    /// 内侧的列数(左外连接补 NULL 时要用).
    right_arity: usize,
    /// 正在配对的左行.
    current: Option<Row>,
    /// 内侧扫到第几行.
    at: usize,
    /// 当前左行匹配上了没有.
    matched: bool,
}

impl<'a> JoinSource<'a> {
    fn new(
        left: Box<dyn RowSource + 'a>,
        right: Box<dyn RowSource + 'a>,
        kind: JoinKind,
        on: Option<&'a Prog>,
        shape: ColumnsShape,
        ctx: Rc<ExecCtx<'a>>,
        node: NodeId,
    ) -> Self {
        JoinSource {
            left,
            right: Some(right),
            kind,
            on,
            shape,
            ctx,
            node,
            error: None,
            inner: None,
            right_arity: 0,
            current: None,
            at: 0,
            matched: false,
        }
    }

    /// 抽干内侧.失败记进 `self.error`.
    fn fill(&mut self) -> Result<()> {
        let mut right = self
            .right
            .take()
            .ok_or_else(|| runtime(self.node, tr!("ir.exec.008").into_owned()))?;

        self.right_arity = right.columns().len();
        let table = source::materialize(right.as_mut())?;
        // `right` 在这里被丢掉:它的缓冲已经搬进 `inner`,不该再留一份
        self.inner = Some(table);
        Ok(())
    }
}

impl RowSource for JoinSource<'_> {
    fn name(&self) -> &str {
        "<join>"
    }

    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        if self.inner.is_none() {
            if let Err(e) = self.fill() {
                self.error = Some(e);
                return None;
            }
        }

        loop {
            // 手里得先有一个左行
            if self.current.is_none() {
                let row = self.left.next()?;
                self.current = Some(row);
                self.at = 0;
                self.matched = false;
            }

            let inner_len = self.inner.as_ref().map_or(0, |table| table.nrows());

            if self.at < inner_len {
                let index = self.at;
                self.at += 1;

                let right_row = match self.inner.as_ref().and_then(|table| table.row(index)) {
                    Some(row) => row,
                    None => {
                        self.error = Some(runtime(
                            self.node,
                            format!("{}", tr!("ir.exec.009", arg0 =  index)),
                        ));
                        return None;
                    }
                };

                let left_row = self.current.as_ref().expect(&tr!("ir.exec.010").into_owned());
                match on_matches(self.on, left_row, &right_row, &self.ctx, self.node) {
                    Err(e) => {
                        self.error = Some(e);
                        return None;
                    }
                    Ok(true) => {
                        self.matched = true;
                        return Some(combine(left_row, &right_row));
                    }
                    // 没有 ON 就是笛卡尔积;ON 不成立就试下一对
                    Ok(false) => {}
                }
                continue;
            }

            // 内侧试完了:左外连接且一个都没配上,就把右支的列全填 NULL
            if self.kind == JoinKind::Left && !self.matched {
                let left_row = self.current.take().expect(&tr!("ir.exec.011").into_owned());
                return Some(pad_right(left_row, self.right_arity));
            }

            // 换下一个左行
            self.current = None;
        }
    }

    fn error(&self) -> Option<&DbError> {
        // 内侧的错误在物化时已经收进 `self.error`,所以只往下问左支
        self.error.as_ref().or_else(|| self.left.error())
    }
}

// --------------------------------------------------------------- 连接:共用零件

/// 连接输出的一行:`左 ++ 右`(与 `Join` 的 schema 规则严格一致).
fn combine(left: &[Value], right: &[Value]) -> Row {
    let mut row = left.to_vec();
    row.extend_from_slice(right);
    row
}

/// 左外连接没配上对:把右支的列全填 NULL.
fn pad_right(left: Row, right_arity: usize) -> Row {
    let mut row = left;
    row.extend(std::iter::repeat(Value::Null).take(right_arity));
    row
}

/// `ON` 在这一对行上成立吗.`on` 是 `None` 表示无条件(笛卡尔积).
///
/// 程序里的列下标按 **左 ++ 右** 编,用 [`Prog::eval_truth_split`] 直接在两半上求值,
/// 不为每一对候选行拼整行.
fn on_matches<'s>(
    on: Option<&Prog>,
    left: &[Value],
    right: &[Value],
    ctx: &ExecCtx<'s>,
    node: NodeId,
) -> Result<bool> {
    match on {
        Some(prog) => truth_of(prog, left, right, ctx)
            .map(|truth| truth.is_true())
            .map_err(|e| e.into_db_error(Some(node))),
        None => Ok(true),
    }
}

/// 把一个程序求值成三值真值.
///
/// 所有"程序面对一整行 / 面对左右两半"的地方都走它,于是 [`crate::ir::exec`] 里
/// 只有一处知道怎么把 [`ExecCtx`] 交给 [`Prog::eval_with`].
fn truth_of<'s>(
    prog: &Prog,
    left: &[Value],
    right: &[Value],
    ctx: &ExecCtx<'s>,
) -> std::result::Result<Tri, EvalError> {
    prog.eval_with(left, right, ctx)?
        .as_truth()
        .ok_or(EvalError::NotABool)
}

/// 用 `keys` 里的一侧程序,在**那一支自己的行**上算出键值.
///
/// `pick` 决定取哪一半(`outer` 还是 `inner`)--两个程序各自面对自己那一支的
/// schema,所以直接 `eval`,不必知道对方的宽度.
fn key_of<'k, 's>(
    keys: &'k [EqKey],
    pick: impl Fn(&'k EqKey) -> &'k Prog,
    row: &[Value],
    ctx: &ExecCtx<'s>,
    node: NodeId,
) -> Result<Vec<Value>> {
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        let value = pick(key)
            .eval_with(row, &[], ctx)
            .map_err(|e| e.into_db_error(Some(node)))?
            .to_value()
            .ok_or_else(|| runtime(node, tr!("ir.exec.012").into_owned()))?;
        values.push(value);
    }
    Ok(values)
}

/// 索引路径的一组键:在**没有输入行**的前提下求值(`analyze` 保证它们不读列).
///
/// 两种键:
///
/// * 常量键(平常那种):空环境求值,一次算好--`col IN (常量, ...)` 就是好几个
///   常量键(TODO 3.14(a)),各键的桶由 [`IndexSource`] 合并;
/// * **外层键**(`path.outer`,相关子查询的等值探测,TODO 8.7):键读的是外层行,
///   所以从执行环境的外层行栈里取那一行当输入求值.这个源是在"为外层这一行重开
///   一份管道"时建的,栈顶**就是**当前外层行--于是一次探测一个键,正好.
fn index_keys(path: &IndexPath, ctx: &ExecCtx<'_>, node: NodeId) -> Result<Vec<Value>> {
    let mut values = Vec::with_capacity(path.keys.len());
    for key in &path.keys {
        let scalar = match path.outer {
            false => key.eval(&[]),
            true => {
                // 栈空说明计划坏了(外层键却不在子查询里);`analyze` 的
                // `check_subqueries` 本该挡住,这里不 panic,报一声就完事.
                let outers = ctx.state.outers.borrow();
                let row = outers
                    .last()
                    .cloned()
                    .ok_or_else(|| runtime(node, tr!("ir.exec.026").into_owned()))?;
                key.eval_with(&row, &[], ctx)
            }
        };
        values.push(
            scalar
                .map_err(|e| e.into_db_error(Some(node)))?
                .to_value()
                .ok_or_else(|| runtime(node, tr!("ir.exec.013").into_owned()))?,
        );
    }
    Ok(values)
}

// ------------------------------------------------------------- 连接:哈希

/// 哈希连接.
///
/// **build 侧 = 内侧(右支)**:第一次拉取时把它抽干成 `键 → 行`(键取内侧的
/// 等值键程序).**probe 侧 = 外侧(左支)**,全程流式:每个左行算出键,查桶,
/// 桶里的每一行再**复核整条 `ON`**--等值键只用来缩小候选集,正确性由复核兜底.
///
/// 行序 = 外侧顺序 × 桶内插入顺序(桶是 `Vec`),于是与嵌套循环**逐行一致**;
/// 哈希表本身从不被遍历,所以它的迭代顺序不影响结果.
struct HashJoinSource<'a> {
    left: Box<dyn RowSource + 'a>,
    /// 内侧(右支).第一次拉取时抽干成 `table`.
    right: Option<Box<dyn RowSource + 'a>>,
    kind: JoinKind,
    on: Option<&'a Prog>,
    keys: &'a [EqKey],
    shape: ColumnsShape,
    /// 求值环境(外层行栈 + 子查询缓存).
    ctx: Rc<ExecCtx<'a>>,
    node: NodeId,
    error: Option<DbError>,
    /// 建好的哈希表;`None` 表示还没建.
    table: Option<HashMap<Vec<Value>, Vec<Row>>>,
    /// 内侧的列数(左外连接补 NULL 要用).
    right_arity: usize,
    /// 正在配对的左行.
    current: Option<Row>,
    /// 当前左行的候选行.
    candidates: Vec<Row>,
    /// 候选行扫到第几行.
    at: usize,
    /// 当前左行匹配上了没有.
    matched: bool,
}

impl<'a> HashJoinSource<'a> {
    fn new(
        left: Box<dyn RowSource + 'a>,
        right: Box<dyn RowSource + 'a>,
        kind: JoinKind,
        on: Option<&'a Prog>,
        keys: &'a [EqKey],
        shape: ColumnsShape,
        ctx: Rc<ExecCtx<'a>>,
        node: NodeId,
    ) -> Self {
        HashJoinSource {
            left,
            right: Some(right),
            kind,
            on,
            keys,
            shape,
            ctx,
            node,
            error: None,
            table: None,
            right_arity: 0,
            current: None,
            candidates: Vec::new(),
            at: 0,
            matched: false,
        }
    }

    /// 抽干内侧,建哈希表.失败记进 `self.error`.
    fn fill(&mut self) -> Result<()> {
        let mut right = self
            .right
            .take()
            .ok_or_else(|| runtime(self.node, tr!("ir.exec.014").into_owned()))?;

        self.right_arity = right.columns().len();
        let mut table: HashMap<Vec<Value>, Vec<Row>> = HashMap::new();

        while let Some(row) = right.next() {
            let key = key_of(self.keys, |k| &k.inner, &row, &self.ctx, self.node)?;
            table.entry(key).or_default().push(row);
        }
        if let Some(e) = right.error() {
            return Err(e.clone());
        }

        self.table = Some(table);
        Ok(())
    }
}

impl RowSource for HashJoinSource<'_> {
    fn name(&self) -> &str {
        "<hash join>"
    }

    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        if self.table.is_none() {
            if let Err(e) = self.fill() {
                self.error = Some(e);
                return None;
            }
        }

        loop {
            // 手里得先有一个左行,并把它要看的候选行捞出来
            if self.current.is_none() {
                let row = self.left.next()?;

                let key = match key_of(self.keys, |k| &k.outer, &row, &self.ctx, self.node) {
                    Ok(key) => key,
                    Err(e) => {
                        self.error = Some(e);
                        return None;
                    }
                };

                // 键里有 NULL 就一个都配不上(`NULL = x` 是 UNKNOWN)
                let candidates = if key.iter().any(Value::is_null) {
                    Vec::new()
                } else {
                    self.table
                        .as_ref()
                        .and_then(|table| table.get(&key))
                        .cloned()
                        .unwrap_or_default()
                };

                self.candidates = candidates;
                self.at = 0;
                self.matched = false;
                self.current = Some(row);
            }

            if self.at < self.candidates.len() {
                let right_row = self.candidates[self.at].clone();
                self.at += 1;

                let left_row = self.current.as_ref().expect(&tr!("ir.exec.015").into_owned());
                match on_matches(self.on, left_row, &right_row, &self.ctx, self.node) {
                    Err(e) => {
                        self.error = Some(e);
                        return None;
                    }
                    Ok(true) => {
                        self.matched = true;
                        return Some(combine(left_row, &right_row));
                    }
                    Ok(false) => {}
                }
                continue;
            }

            // 候选试完了:左外连接且一个都没配上,就补 NULL
            if self.kind == JoinKind::Left && !self.matched {
                let left_row = self.current.take().expect(&tr!("ir.exec.016").into_owned());
                return Some(pad_right(left_row, self.right_arity));
            }

            self.current = None;
        }
    }

    fn error(&self) -> Option<&DbError> {
        // 内侧的错误在 build 时已经收进 `self.error`,所以只往下问左支
        self.error.as_ref().or_else(|| self.left.error())
    }
}

// --------------------------------------------------------- 连接:索引嵌套循环

/// 索引嵌套循环.
///
/// 内侧是**基表 `Scan`**:给它建一次"键 → 行号"的索引(见 [`Index`]),外侧
/// 每行按键查一次,只把命中的行从基表取出来.于是内侧**不物化**--这是它与哈希
/// 连接真正的区别(哈希要复制整行,索引只存键与行号).
///
/// 内侧 `Scan` 自己的 `filter` 由这里逐行复核(它的源没被建过);`ON` 照旧整条复核.
/// 索引桶里的行号是升序的,所以行序仍与嵌套循环一致.
struct IndexJoinSource<'a> {
    left: Box<dyn RowSource + 'a>,
    /// 内侧的基表.
    table: &'a dyn Table,
    /// 内侧那一列上的索引槽位(目录里的,或者执行器临时建的一个).
    slot: Rc<IndexSlot>,
    /// 内侧 `Scan` 下推的谓词(取到行之后复核).
    filter: Option<&'a Prog>,
    /// 等值键;只用它的 `outer`(内侧那一半就是上面那一列).
    key: &'a EqKey,
    kind: JoinKind,
    on: Option<&'a Prog>,
    shape: ColumnsShape,
    /// 求值环境(外层行栈 + 子查询缓存).
    ctx: Rc<ExecCtx<'a>>,
    node: NodeId,
    error: Option<DbError>,
    /// 内侧的列数(左外连接补 NULL 要用).
    right_arity: usize,
    /// 正在配对的左行.
    current: Option<Row>,
    /// 当前左行命中内侧的行号.
    rids: Vec<usize>,
    /// 行号扫到第几个.
    at: usize,
    /// 当前左行匹配上了没有.
    matched: bool,
}

impl<'a> IndexJoinSource<'a> {
    fn new(
        left: Box<dyn RowSource + 'a>,
        table: &'a dyn Table,
        slot: Rc<IndexSlot>,
        filter: Option<&'a Prog>,
        key: &'a EqKey,
        kind: JoinKind,
        on: Option<&'a Prog>,
        shape: ColumnsShape,
        ctx: Rc<ExecCtx<'a>>,
        node: NodeId,
    ) -> Self {
        IndexJoinSource {
            left,
            table,
            slot,
            filter,
            key,
            kind,
            on,
            shape,
            ctx,
            node,
            error: None,
            // 内侧就是这张表的 `Scan`,所以它的列数就是表的元数
            right_arity: table.arity(),
            current: None,
            rids: Vec::new(),
            at: 0,
            matched: false,
        }
    }

    /// 当前左行在内侧命中的行号.
    ///
    /// 索引由槽位按需建(第一次查找时建一份,之后复用);`UPDATE` 之后它会失效,
    /// 于是这里重建一次,仍然只重建一次.
    fn lookup(&self, left_row: &[Value]) -> Result<Vec<usize>> {
        let key = self
            .key
            .outer
            .eval_with(left_row, &[], &*self.ctx)
            .map_err(|e| e.into_db_error(Some(self.node)))?
            .to_value()
            .ok_or_else(|| runtime(self.node, tr!("ir.exec.017").into_owned()))?;

        // 键是 NULL 就一个都配不上(`NULL = x` 是 UNKNOWN)
        if key.is_null() {
            return Ok(Vec::new());
        }

        self.slot.lookup(self.table, &key)
    }
}

impl RowSource for IndexJoinSource<'_> {
    fn name(&self) -> &str {
        "<index join>"
    }

    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        loop {
            if self.current.is_none() {
                let row = self.left.next()?;
                match self.lookup(&row) {
                    Ok(rids) => self.rids = rids,
                    Err(e) => {
                        self.error = Some(e);
                        return None;
                    }
                }
                self.at = 0;
                self.matched = false;
                self.current = Some(row);
            }

            if self.at < self.rids.len() {
                let rid = self.rids[self.at];
                self.at += 1;

                let right_row = match self.table.row(rid) {
                    Some(row) => row,
                    None => {
                        self.error = Some(runtime(
                            self.node,
                            format!("{}", tr!("ir.exec.018", arg0 =  rid)),
                        ));
                        return None;
                    }
                };

                // 内侧 Scan 自己的谓词照旧要过
                if let Some(filter) = self.filter {
                    match truth_of(filter, &right_row, &[], &self.ctx) {
                        Ok(truth) if truth.is_true() => {}
                        Ok(_) => continue,
                        Err(e) => {
                            self.error = Some(e.into_db_error(Some(self.node)));
                            return None;
                        }
                    }
                }

                let left_row = self.current.as_ref().expect(&tr!("ir.exec.019").into_owned());
                match on_matches(self.on, left_row, &right_row, &self.ctx, self.node) {
                    Err(e) => {
                        self.error = Some(e);
                        return None;
                    }
                    Ok(true) => {
                        self.matched = true;
                        return Some(combine(left_row, &right_row));
                    }
                    Ok(false) => {}
                }
                continue;
            }

            if self.kind == JoinKind::Left && !self.matched {
                let left_row = self.current.take().expect(&tr!("ir.exec.020").into_owned());
                return Some(pad_right(left_row, self.right_arity));
            }

            self.current = None;
        }
    }

    fn error(&self) -> Option<&DbError> {
        // 建索引与取行的错误都在 `self.error`,所以只往下问左支
        self.error.as_ref().or_else(|| self.left.error())
    }
}

/// 一个分组的累加器.
enum Accum {
    Count(u64),
    CountDistinct(std::collections::BTreeSet<Value>),
    Sum(Option<Value>),
    Avg { sum: Option<Value>, count: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

impl Accum {
    fn new(spec: &AggSpec) -> Self {
        match spec.func {
            AggFunc::Count => {
                if spec.distinct {
                    Accum::CountDistinct(std::collections::BTreeSet::new())
                } else {
                    Accum::Count(0)
                }
            }
            AggFunc::Sum => Accum::Sum(None),
            AggFunc::Avg => Accum::Avg {
                sum: None,
                count: 0,
            },
            AggFunc::Min => Accum::Min(None),
            AggFunc::Max => Accum::Max(None),
        }
    }

    /// 喂一行.`value` 为 `None` 表示 `COUNT(*)`(无条件计数).
    ///
    /// **NULL 不进累加器**(`COUNT(x)` 跳过 NULL,`SUM`/`AVG`/`MIN`/`MAX` 忽略它),
    /// 这正是 SQL 的语义.
    fn feed(&mut self, value: Option<Value>, node: NodeId) -> Result<()> {
        let value = match value {
            Some(v) if v.is_null() => return Ok(()),
            Some(v) => v,
            None => {
                if let Accum::Count(count) = self {
                    *count += 1;
                }
                return Ok(());
            }
        };

        match self {
            Accum::Count(count) => *count += 1,
            Accum::CountDistinct(set) => {
                set.insert(value);
            }
            Accum::Sum(slot) => {
                *slot = Some(match slot.take() {
                    None => value,
                    Some(acc) => add_values(&acc, &value, node)?,
                });
            }
            Accum::Avg { sum, count } => {
                *count += 1;
                *sum = Some(match sum.take() {
                    None => value,
                    Some(acc) => add_values(&acc, &value, node)?,
                });
            }
            Accum::Min(slot) => {
                if slot.as_ref().is_none_or(|m| value < *m) {
                    *slot = Some(value);
                }
            }
            Accum::Max(slot) => {
                if slot.as_ref().is_none_or(|m| value > *m) {
                    *slot = Some(value);
                }
            }
        }
        Ok(())
    }

    fn finish(&self, node: NodeId) -> Result<Value> {
        Ok(match self {
            Accum::Count(count) => Value::Int(
                i32::try_from(*count).map_err(|_| runtime(node, tr!("ir.exec.021").into_owned()))?,
            ),
            Accum::CountDistinct(set) => Value::Int(
                i32::try_from(set.len())
                    .map_err(|_| runtime(node, tr!("ir.exec.022").into_owned()))?,
            ),
            Accum::Sum(v) => v.clone().unwrap_or(Value::Null),
            Accum::Avg { sum, count } => match (sum, count) {
                (Some(total), count) if *count > 0 => total
                    .decimal(DecimalOp::Div, &Value::Int(*count as i32))
                    .map_err(|reason| runtime(node, reason))?,
                // 一组全是 NULL(或没有行)时 AVG 是 NULL
                _ => Value::Null,
            },
            Accum::Min(v) | Accum::Max(v) => v.clone().unwrap_or(Value::Null),
        })
    }
}

/// `SUM` 用的加法:两个 INT 相加仍是 INT(不变成 NUMERIC),其余走定点十进制.
fn add_values(acc: &Value, value: &Value, node: NodeId) -> Result<Value> {
    if let (Value::Int(a), Value::Int(b)) = (acc, value) {
        return a
            .checked_add(*b)
            .map(Value::Int)
            .ok_or_else(|| runtime(node, tr!("ir.exec.023").into_owned()));
    }
    acc.decimal(DecimalOp::Add, value)
        .map_err(|reason| runtime(node, reason))
}

/// 分组聚合:输入要先看完(组得等齐),所以第一次拉取时**抽干**输入.
///
/// 输出顺序 = 分组键在输入里**首次出现的顺序**(`groups` 是个 Vec,`BTreeMap`
/// 只用来查"这个键见过没有").没有 `ORDER BY` 时 SQL 本来就不保证顺序,但这样
/// 至少是**完全确定的**,测试可以稳定断言.
///
/// 分组键里的 NULL 归成一组,与 SQL 一致.
struct AggregateSource<'a> {
    input: Box<dyn RowSource + 'a>,
    group_by: &'a [Prog],
    aggs: &'a [AggSpec],
    shape: ColumnsShape,
    /// 求值环境(外层行栈 + 子查询缓存).
    ctx: Rc<ExecCtx<'a>>,
    node: NodeId,
    error: Option<DbError>,
    /// 攒好的输出行;`None` 表示还没算过.
    spool: Option<PhysTab>,
    at: usize,
}

impl<'a> AggregateSource<'a> {
    fn new(
        input: Box<dyn RowSource + 'a>,
        group_by: &'a [Prog],
        aggs: &'a [AggSpec],
        shape: ColumnsShape,
        ctx: Rc<ExecCtx<'a>>,
        node: NodeId,
    ) -> Self {
        AggregateSource {
            input,
            group_by,
            aggs,
            shape,
            ctx,
            node,
            error: None,
            spool: None,
            at: 0,
        }
    }

    /// 抽干输入,分组,物化输出.失败记进 `self.error`.
    fn fill(&mut self) -> Result<()> {
        struct Group {
            keys: Vec<Value>,
            accums: Vec<Accum>,
        }

        let mut index_of: std::collections::BTreeMap<Vec<Value>, usize> =
            std::collections::BTreeMap::new();
        let mut groups: Vec<Group> = Vec::new();

        while let Some(row) = self.input.next() {
            let mut key = Vec::with_capacity(self.group_by.len());
            for prog in self.group_by {
                let scalar = prog.eval_with(&row, &[], &*self.ctx).map_err(|e| e.into_db_error(Some(self.node)))?;
                key.push(
                    scalar
                        .to_value()
                        .ok_or_else(|| runtime(self.node, tr!("ir.exec.024").into_owned()))?,
                );
            }

            let index = match index_of.get(&key) {
                Some(&index) => index,
                None => {
                    let index = groups.len();
                    index_of.insert(key.clone(), index);
                    groups.push(Group {
                        keys: key,
                        accums: self.aggs.iter().map(Accum::new).collect(),
                    });
                    index
                }
            };

            for (slot, spec) in self.aggs.iter().enumerate() {
                // `FILTER (WHERE ...)`:只有确定真的行才进累加器
                if let Some(filter) = &spec.filter {
                    let keep = truth_of(filter, &row, &[], &self.ctx)
                        .map_err(|e| e.into_db_error(Some(self.node)))?;
                    if !keep.is_true() {
                        continue;
                    }
                }

                let value = match &spec.arg {
                    // COUNT(*):不看值,来一行算一个
                    None => None,
                    Some(prog) => Some(
                        prog.eval_with(&row, &[], &*self.ctx)
                            .map_err(|e| e.into_db_error(Some(self.node)))?
                            .to_value()
                            .ok_or_else(|| {
                                runtime(self.node, tr!("ir.exec.025").into_owned())
                            })?,
                    ),
                };
                groups[index].accums[slot].feed(value, self.node)?;
            }
        }

        if let Some(e) = self.input.error() {
            return Err(e.clone());
        }

        // 没有 GROUP BY 又一行输入都没有:标准要求**仍然产出一行**
        // (COUNT(*) = 0,AVG = NULL)
        if groups.is_empty() && self.group_by.is_empty() {
            groups.push(Group {
                keys: Vec::new(),
                accums: self.aggs.iter().map(Accum::new).collect(),
            });
        }

        let mut table = PhysTab::new("<aggregate>", &self.shape);
        for group in groups {
            let mut row = group.keys;
            for accum in &group.accums {
                row.push(accum.finish(self.node)?);
            }
            table.insert(row)?;
        }
        self.spool = Some(table);
        Ok(())
    }
}

impl RowSource for AggregateSource<'_> {
    fn name(&self) -> &str {
        "<aggregate>"
    }

    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        if self.spool.is_none() {
            if let Err(e) = self.fill() {
                self.error = Some(e);
                return None;
            }
        }

        let row = self.spool.as_ref().and_then(|table| table.row(self.at))?;
        self.at += 1;
        Some(row)
    }

    fn error(&self) -> Option<&DbError> {
        self.error.as_ref().or_else(|| self.input.error())
    }
}

/// 比较一个排序键.
///
/// **NULL 的位置是绝对的**(由 `nulls_first` 决定),不跟着 `desc` 翻转--
/// `ORDER BY x DESC` 在 PostgreSQL 里是 NULLS FIRST,不是"把 ASC 的结果整个倒过来".
fn compare_key(a: &Scalar, b: &Scalar, desc: bool, nulls_first: bool) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let ord = a.cmp(b);
            if desc {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// 排序:要随机访问,所以第一次拉取时把输入**抽干**,算出排序键,稳定排序,再一行行吐.
///
/// 上面的 `LIMIT` 拉够行就停,但排序本身仍要把输入看完--这是算法性质.
struct SortSource<'a> {
    input: Box<dyn RowSource + 'a>,
    keys: &'a [SortKey],
    shape: ColumnsShape,
    /// 求值环境(外层行栈 + 子查询缓存).
    ctx: Rc<ExecCtx<'a>>,
    node: NodeId,
    error: Option<DbError>,
    /// 排好序的行;`None` 表示还没算过.
    spool: Option<PhysTab>,
    at: usize,
}

impl<'a> SortSource<'a> {
    fn new(
        input: Box<dyn RowSource + 'a>,
        keys: &'a [SortKey],
        shape: ColumnsShape,
        ctx: Rc<ExecCtx<'a>>,
        node: NodeId,
    ) -> Self {
        SortSource {
            input,
            keys,
            shape,
            ctx,
            node,
            error: None,
            spool: None,
            at: 0,
        }
    }

    /// 抽干输入,排序,物化.失败记进 `self.error`.
    fn fill(&mut self) -> Result<()> {
        let mut keyed: Vec<(Vec<Scalar>, Row)> = Vec::new();

        while let Some(row) = self.input.next() {
            let mut values = Vec::with_capacity(self.keys.len());
            for key in self.keys {
                values.push(
                    key.prog
                        .eval_with(&row, &[], &*self.ctx)
                        .map_err(|e| e.into_db_error(Some(self.node)))?,
                );
            }
            keyed.push((values, row));
        }

        if let Some(e) = self.input.error() {
            return Err(e.clone());
        }

        // sort_by 是稳定排序
        keyed.sort_by(|a, b| {
            for (i, key) in self.keys.iter().enumerate() {
                let ord = compare_key(&a.0[i], &b.0[i], key.desc, key.nulls_first);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });

        let mut table = PhysTab::new("<sort>", &self.shape);
        for (_, row) in keyed {
            table.insert(row)?;
        }
        self.spool = Some(table);
        Ok(())
    }
}

impl RowSource for SortSource<'_> {
    fn name(&self) -> &str {
        "<sort>"
    }

    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        if self.spool.is_none() {
            if let Err(e) = self.fill() {
                self.error = Some(e);
                return None;
            }
        }

        let row = self.spool.as_ref().and_then(|table| table.row(self.at))?;
        self.at += 1;
        Some(row)
    }

    fn error(&self) -> Option<&DbError> {
        self.error.as_ref().or_else(|| self.input.error())
    }
}

/// 截断:先拉掉 `offset` 行,再吐最多 `limit` 行.
///
/// 这是拉取式最直接的收益:**上面停手,下面就不再被拉**,`LIMIT` 之下那一大段
/// 根本不会被求值(那里的运行期错误也就不会冒出来--与 PostgreSQL 一致).
struct LimitSource<'a> {
    input: Box<dyn RowSource + 'a>,
    limit: Option<u64>,
    offset: u64,
    emitted: u64,
    /// `offset` 只跳一次(后续的 `next` 不能重复跳).
    skipped: bool,
}

impl<'a> LimitSource<'a> {
    fn new(input: Box<dyn RowSource + 'a>, limit: Option<u64>, offset: u64) -> Self {
        LimitSource {
            input,
            limit,
            offset,
            emitted: 0,
            skipped: false,
        }
    }
}

impl RowSource for LimitSource<'_> {
    fn name(&self) -> &str {
        "<limit>"
    }

    fn columns(&self) -> &[Column] {
        // 截断不改变 schema
        self.input.columns()
    }

    fn next(&mut self) -> Option<Row> {
        // 先看限额再看 `offset`:`LIMIT 0` 一行都不该拉(连 `offset` 都不跳)
        if let Some(limit) = self.limit {
            if self.emitted >= limit {
                return None;
            }
        }

        if !self.skipped {
            self.skipped = true;
            let mut skipped = 0;
            while skipped < self.offset {
                // 输入不够跳就到此为止(不是错误)
                self.input.next()?;
                skipped += 1;
            }
        }

        let row = self.input.next()?;
        self.emitted += 1;
        Some(row)
    }

    fn error(&self) -> Option<&DbError> {
        self.input.error()
    }
}

/// 去重:流式的 `SELECT DISTINCT`.
///
/// 一个哈希集合记住见过的行,来一行放一行--**既不用先 `Sort`,也不用物化**.
/// 行是 `Vec<Value>`,而 `Value` 的 `Hash` 与 `Eq` 一致(见 `db::table`),
/// 所以直接拿整行当键.
struct DistinctSource<'a> {
    input: Box<dyn RowSource + 'a>,
    seen: HashSet<Row>,
}

impl<'a> DistinctSource<'a> {
    fn new(input: Box<dyn RowSource + 'a>) -> Self {
        DistinctSource {
            input,
            seen: HashSet::new(),
        }
    }
}

impl RowSource for DistinctSource<'_> {
    fn name(&self) -> &str {
        "<distinct>"
    }

    fn columns(&self) -> &[Column] {
        // 去重不改变 schema
        self.input.columns()
    }

    fn next(&mut self) -> Option<Row> {
        loop {
            let row = self.input.next()?;
            if self.seen.insert(row.clone()) {
                return Some(row);
            }
        }
    }

    fn error(&self) -> Option<&DbError> {
        self.input.error()
    }
}

fn runtime(node: NodeId, message: String) -> DbError {
    DbError::IrRuntime {
        node: Some(node),
        message,
    }
}

/// `UNION [ALL]`:先左边,再右边.
///
/// `UNION ALL` 就是两条流的接续;`UNION`(没写 `ALL`)多一个集合,而且是**两支合起来**
/// 去重--与 PostgreSQL 一样,重复行要跨两支看.输出列名与类型取左支(`analyze` 保证
/// 两支逐列同族).
struct UnionSource<'a> {
    left: Box<dyn RowSource + 'a>,
    right: Box<dyn RowSource + 'a>,
    /// 去重集合;`UNION ALL` 是 `None`(不去重).
    seen: Option<HashSet<Row>>,
    /// 左支拉完了吗.
    on_right: bool,
    shape: ColumnsShape,
    error: Option<DbError>,
}

impl<'a> UnionSource<'a> {
    fn new(
        left: Box<dyn RowSource + 'a>,
        right: Box<dyn RowSource + 'a>,
        all: bool,
        shape: ColumnsShape,
    ) -> Self {
        UnionSource {
            left,
            right,
            seen: if all { None } else { Some(HashSet::new()) },
            on_right: false,
            shape,
            error: None,
        }
    }
}

impl RowSource for UnionSource<'_> {
    fn name(&self) -> &str {
        "<union>"
    }

    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        loop {
            let side = if self.on_right {
                &mut self.right
            } else {
                &mut self.left
            };

            match side.next() {
                Some(row) => {
                    let fresh = match self.seen.as_mut() {
                        Some(seen) => seen.insert(row.clone()),
                        None => true,
                    };
                    if fresh {
                        return Some(row);
                    }
                }
                None => {
                    if let Some(e) = side.error().cloned() {
                        self.error = Some(e);
                        return None;
                    }
                    if self.on_right {
                        return None;
                    }
                    // 左支走完,接着拉右支
                    self.on_right = true;
                }
            }
        }
    }

    fn error(&self) -> Option<&DbError> {
        self.error
            .as_ref()
            .or_else(|| self.left.error())
            .or_else(|| self.right.error())
    }
}
