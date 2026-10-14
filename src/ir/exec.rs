//! 线性计划的执行——**拉取式**。
//!
//! 靠不变量 I1（输入下标严格小于自身下标），**构建**管道就是对一个数组的正向
//! `for` 循环：走到第 `id` 个节点时，它的操作数一定已经建好了。这里没有递归，
//! 所以再深的计划也不会爆栈。
//!
//! 每个节点产出的是一个 [`RowSource`]——一行一行拉的游标，不再是物化的表。上层拉一行，
//! 下层才算一行。**只有三处会把行攒下来**：
//!
//! * 需要**随机访问**的算子（`Sort`、`Aggregate` 必须先看完全部输入）；
//! * 需要**重扫**的地方（嵌套循环连接的内侧、被多个父节点引用的节点）；
//! * **最终结果**（[`execute`] 抽干 root 的那一步）。
//!
//! 于是 `LIMIT` 之下不再被完整求值、连接不再为每一对候选行拼整行——详见
//! `LimitSource` 与 `JoinSource`。
//!
//! # 只从 root 出发
//!
//! 管道的构建与 `explain` 的遍历口径一致：从 root 走不到的节点**既不建源也不执行**
//! （静态校验 [`Plan::analyze`] 仍然覆盖它们）。这是拉取式的直接推论，也让
//! 「不可达节点」这件事与文档一致。
//!
//! # 被多个父节点引用的节点
//!
//! 计划是 DAG：一个节点可以被多个父节点引用。可重扫的游标做不到「拉到一半给别人再从头」，
//! 所以这类节点会被**物化一次**，每个消费者各拿一个 `SharedCursor`。

use std::cmp::Ordering;
use std::rc::Rc;

use crate::db::phys::PhysTab;
use crate::db::source::{self, RowSource, TableCursor};
use crate::db::table::{
    Column, ColumnsShape, DbError, DecimalOp, Result, Row, Table, TabShape, Value,
};

use super::expr::{Prog, Scalar};
use super::plan::{AggFunc, AggSpec, JoinKind, NodeId, Op, Plan, SortKey};
use super::symbol::SymbolTable;

/// 构建拉取式的执行管道，返回 root 的行源。
///
/// 执行前会先跑一次 [`Plan::analyze`]，所以坏 IR 会在任何数据被碰之前就被拒绝。
/// **拉多少行、什么时候停由调用方决定**：只要不再调 `next()`，下层就不会被拉
/// （`Limit` 之下、甚至 `Sort` 的物化都可以因此不发生）。
///
/// 需要"算完整个结果"就用 [`execute`]，或自己用
/// [`crate::db::source::materialize`] 抽干。
pub fn execute_source<'a>(
    plan: &'a Plan,
    symbols: &'a SymbolTable,
) -> Result<Box<dyn RowSource + 'a>> {
    let analysis = plan.analyze(symbols)?;

    // 只从 root 出发：走不到的节点不建源、不执行。
    let reachable = reachable_from(plan);

    // 在可达集合里数每个节点被几个父节点引用：>1 的节点要物化一次给所有人共享。
    let mut consumers = vec![0usize; plan.nodes.len()];
    for (id, node) in plan.nodes.iter().enumerate() {
        if reachable[id] {
            for &input in &node.inputs {
                consumers[input] += 1;
            }
        }
    }

    let mut done: Vec<Option<Slot<'a>>> = (0..plan.nodes.len()).map(|_| None).collect();

    for (id, node) in plan.nodes.iter().enumerate() {
        if !reachable[id] {
            continue;
        }

        // schema 由 `Plan::analyze` 统一推导（透传列保留原类型），这里不再自己猜
        let schema = ColumnsShape::new(analysis.schemas[id].to_vec());

        let source: Box<dyn RowSource + 'a> = match &node.op {
            Op::SingleRow => Box::new(SingleRowSource::new()),

            Op::Scan { table, filter } => {
                // `analyze` 已经查过这个名字，这里只是把表取出来
                let relation =
                    symbols
                        .relation(table)
                        .ok_or_else(|| DbError::UnknownSymbol {
                            name: table.clone(),
                        })?;
                Box::new(ScanSource::new(relation, filter.as_ref(), id))
            }

            Op::Filter { predicate } => Box::new(FilterSource::new(
                input(&mut done, node.inputs[0])?,
                predicate,
                id,
            )),

            Op::Project { progs, .. } => Box::new(ProjectSource::new(
                input(&mut done, node.inputs[0])?,
                progs,
                schema,
                id,
            )),

            Op::Join { kind, on } => Box::new(JoinSource::new(
                input(&mut done, node.inputs[0])?,
                input(&mut done, node.inputs[1])?,
                *kind,
                on.as_ref(),
                schema,
                id,
            )),

            Op::Aggregate {
                group_by, aggs, ..
            } => Box::new(AggregateSource::new(
                input(&mut done, node.inputs[0])?,
                group_by,
                aggs,
                schema,
                id,
            )),

            Op::Sort { keys } => Box::new(SortSource::new(
                input(&mut done, node.inputs[0])?,
                keys,
                schema,
                id,
            )),

            Op::Limit { limit, offset } => Box::new(LimitSource::new(
                input(&mut done, node.inputs[0])?,
                *limit,
                *offset,
            )),
        };

        done[id] = Some(if consumers[id] > 1 {
            // 被多个父节点引用：重扫的代价比分发一份物化结果高，所以在这里物化**一次**。
            let mut source = source;
            Slot::Shared(Rc::new(source::materialize(&mut *source)?))
        } else {
            Slot::Source(source)
        });
    }

    match done[plan.root].take() {
        Some(Slot::Source(source)) => Ok(source),
        Some(Slot::Shared(table)) => Ok(Box::new(SharedCursor::new(table))),
        None => Err(runtime(plan.root, "root 没有产出结果".to_string())),
    }
}

/// 执行一个线性计划，抽干 root 的行源，返回结果表。
///
/// 这是"算完整个结果"的入口；想自己控制拉多少行、什么时候停，用 [`execute_source`]。
pub fn execute(plan: &Plan, symbols: &SymbolTable) -> Result<PhysTab> {
    let mut source = execute_source(plan, symbols)?;
    source::materialize(&mut *source)
}

/// 一个节点的产物。
///
/// 通常就是一个源（只被一个父节点消费）；被多个父节点引用时，在这里物化一次，
/// 每个消费者各拿一个 [`SharedCursor`]。
enum Slot<'a> {
    /// 还没被消费的行源。
    Source(Box<dyn RowSource + 'a>),
    /// 已经物化好的共享结果。
    Shared(Rc<PhysTab>),
}

/// 取一个节点的输出当输入用。
///
/// `Shared` 是**克隆 `Rc`**（几个消费者都要读同一份），`Source` 是**取走**（一个源只
/// 能被消费一次）。
fn input<'a>(done: &mut [Option<Slot<'a>>], id: NodeId) -> Result<Box<dyn RowSource + 'a>> {
    let slot = done
        .get_mut(id)
        .ok_or_else(|| runtime(id, format!("节点 {} 不存在", id)))?;

    if let Some(Slot::Shared(table)) = slot {
        return Ok(Box::new(SharedCursor::new(Rc::clone(table))));
    }

    match slot.take() {
        Some(Slot::Source(source)) => Ok(source),
        // 构造上不可达：`analyze` 保证输入下标严格小于自身下标。
        _ => Err(runtime(id, format!("节点 {} 的输出还不存在", id))),
    }
}

/// 从 root 出发、顺着 `inputs` 能走到的节点。
///
/// 显式栈、不递归。I1 保证输入只指向过去，所以一定会停。
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

/// 共享物化结果的上游游标：每个消费者一个，各自从第 0 行开始。
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

/// 一张 1 行 0 列的关系。`SELECT 1 + 1` 这种没有 FROM 的查询拿它当底座。
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

/// 扫描：基表按名字取出来，逐行跑已下推的谓词。
struct ScanSource<'a> {
    cursor: TableCursor<'a>,
    filter: Option<&'a Prog>,
    node: NodeId,
    error: Option<DbError>,
}

impl<'a> ScanSource<'a> {
    fn new(relation: &'a dyn Table, filter: Option<&'a Prog>, node: NodeId) -> Self {
        ScanSource {
            cursor: TableCursor::new(relation),
            filter,
            node,
            error: None,
        }
    }
}

impl RowSource for ScanSource<'_> {
    fn name(&self) -> &str {
        self.cursor.name()
    }

    fn columns(&self) -> &[Column] {
        self.cursor.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        loop {
            let row = self.cursor.next()?;
            let keep = match self.filter {
                Some(prog) => match prog.eval_truth(&row) {
                    Ok(truth) => truth.is_true(),
                    Err(e) => {
                        self.error = Some(e.into_db_error(Some(self.node)));
                        return None;
                    }
                },
                None => true,
            };
            if keep {
                return Some(row);
            }
        }
    }

    fn error(&self) -> Option<&DbError> {
        // 基表短读的错误在游标里，谓词的错误在这里
        self.error.as_ref().or_else(|| self.cursor.error())
    }
}

/// 选择。
struct FilterSource<'a> {
    input: Box<dyn RowSource + 'a>,
    predicate: &'a Prog,
    node: NodeId,
    error: Option<DbError>,
}

impl<'a> FilterSource<'a> {
    fn new(input: Box<dyn RowSource + 'a>, predicate: &'a Prog, node: NodeId) -> Self {
        FilterSource {
            input,
            predicate,
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
        if self.error.is_some() {
            return None;
        }

        loop {
            let row = self.input.next()?;
            match self.predicate.eval_truth(&row) {
                // 只有确定的真才留：UNKNOWN 与 FALSE 一样被丢掉
                Ok(truth) if truth.is_true() => return Some(row),
                Ok(_) => {}
                Err(e) => {
                    self.error = Some(e.into_db_error(Some(self.node)));
                    return None;
                }
            }
        }
    }

    fn error(&self) -> Option<&DbError> {
        self.error.as_ref().or_else(|| self.input.error())
    }
}

/// 投影。
struct ProjectSource<'a> {
    input: Box<dyn RowSource + 'a>,
    progs: &'a [Prog],
    shape: ColumnsShape,
    node: NodeId,
    error: Option<DbError>,
}

impl<'a> ProjectSource<'a> {
    fn new(
        input: Box<dyn RowSource + 'a>,
        progs: &'a [Prog],
        shape: ColumnsShape,
        node: NodeId,
    ) -> Self {
        ProjectSource {
            input,
            progs,
            shape,
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
            let scalar = match prog.eval(&row) {
                Ok(scalar) => scalar,
                Err(e) => {
                    self.error = Some(e.into_db_error(Some(self.node)));
                    return None;
                }
            };
            let value = match scalar.to_value() {
                Some(value) => value,
                // 静态校验已经挡住了真值作结果列，这里只是不让它变成长得一样的结果
                None => {
                    self.error = Some(runtime(
                        self.node,
                        format!("结果列不能是真值，实际是 {}", scalar),
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

/// 嵌套循环连接。
///
/// 输出行是 `左行 ++ 右行`（与 `Join` 的 schema 规则严格一致），但**只在命中时才拼**：
/// `ON` 用 [`Prog::eval_truth_split`] 在 `(左行, 右行)` 上直接求值，不为每一对候选行
/// 拼一整行。
///
/// 内侧（右支）必须能为每个左行重扫一遍，而游标只能往前——所以它在第一次拉取时被
/// **抽干物化一次**；左支是流式的。
struct JoinSource<'a> {
    left: Box<dyn RowSource + 'a>,
    /// 内侧（右支）。第一次拉取时被抽干，之后丢掉它的缓冲由 `inner` 接管。
    right: Option<Box<dyn RowSource + 'a>>,
    kind: JoinKind,
    on: Option<&'a Prog>,
    shape: ColumnsShape,
    node: NodeId,
    error: Option<DbError>,
    /// 物化好的内侧；`None` 表示还没物化。
    inner: Option<PhysTab>,
    /// 内侧的列数（左外连接补 NULL 时要用）。
    right_arity: usize,
    /// 正在配对的左行。
    current: Option<Row>,
    /// 内侧扫到第几行。
    at: usize,
    /// 当前左行匹配上了没有。
    matched: bool,
}

impl<'a> JoinSource<'a> {
    fn new(
        left: Box<dyn RowSource + 'a>,
        right: Box<dyn RowSource + 'a>,
        kind: JoinKind,
        on: Option<&'a Prog>,
        shape: ColumnsShape,
        node: NodeId,
    ) -> Self {
        JoinSource {
            left,
            right: Some(right),
            kind,
            on,
            shape,
            node,
            error: None,
            inner: None,
            right_arity: 0,
            current: None,
            at: 0,
            matched: false,
        }
    }

    /// 抽干内侧。失败记进 `self.error`。
    fn fill(&mut self) -> Result<()> {
        let mut right = self
            .right
            .take()
            .ok_or_else(|| runtime(self.node, "内侧已经被抽干过".to_string()))?;

        self.right_arity = right.columns().len();
        let table = source::materialize(right.as_mut())?;
        // `right` 在这里被丢掉：它的缓冲已经搬进 `inner`，不该再留一份
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
                            format!("读取内侧第 {} 行失败", index),
                        ));
                        return None;
                    }
                };

                let left_row = self.current.as_ref().expect("上面刚填过");
                let keep = match self.on {
                    Some(prog) => match prog.eval_truth_split(left_row, &right_row) {
                        Ok(truth) => truth.is_true(),
                        Err(e) => {
                            self.error = Some(e.into_db_error(Some(self.node)));
                            return None;
                        }
                    },
                    // 没有 ON 就是笛卡尔积
                    None => true,
                };

                if keep {
                    self.matched = true;
                    let mut combined = left_row.clone();
                    combined.extend(right_row);
                    return Some(combined);
                }
                continue;
            }

            // 内侧试完了：左外连接且一个都没配上，就把右支的列全填 NULL
            if self.kind == JoinKind::Left && !self.matched {
                let mut combined = self.current.take().expect("上面刚填过");
                combined.extend(std::iter::repeat(Value::Null).take(self.right_arity));
                return Some(combined);
            }

            // 换下一个左行
            self.current = None;
        }
    }

    fn error(&self) -> Option<&DbError> {
        // 内侧的错误在物化时已经收进 `self.error`，所以只往下问左支
        self.error.as_ref().or_else(|| self.left.error())
    }
}

/// 一个分组的累加器。
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

    /// 喂一行。`value` 为 `None` 表示 `COUNT(*)`（无条件计数）。
    ///
    /// **NULL 不进累加器**（`COUNT(x)` 跳过 NULL、`SUM`/`AVG`/`MIN`/`MAX` 忽略它），
    /// 这正是 SQL 的语义。
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
                i32::try_from(*count).map_err(|_| runtime(node, "COUNT 溢出 i32".to_string()))?,
            ),
            Accum::CountDistinct(set) => Value::Int(
                i32::try_from(set.len())
                    .map_err(|_| runtime(node, "COUNT(DISTINCT) 溢出 i32".to_string()))?,
            ),
            Accum::Sum(v) => v.clone().unwrap_or(Value::Null),
            Accum::Avg { sum, count } => match (sum, count) {
                (Some(total), count) if *count > 0 => total
                    .decimal(DecimalOp::Div, &Value::Int(*count as i32))
                    .map_err(|reason| runtime(node, reason))?,
                // 一组全是 NULL（或没有行）时 AVG 是 NULL
                _ => Value::Null,
            },
            Accum::Min(v) | Accum::Max(v) => v.clone().unwrap_or(Value::Null),
        })
    }
}

/// `SUM` 用的加法：两个 INT 相加仍是 INT（不变成 NUMERIC），其余走定点十进制。
fn add_values(acc: &Value, value: &Value, node: NodeId) -> Result<Value> {
    if let (Value::Int(a), Value::Int(b)) = (acc, value) {
        return a
            .checked_add(*b)
            .map(Value::Int)
            .ok_or_else(|| runtime(node, "SUM 的整数溢出".to_string()));
    }
    acc.decimal(DecimalOp::Add, value)
        .map_err(|reason| runtime(node, reason))
}

/// 分组聚合：输入要先看完（组得等齐），所以第一次拉取时**抽干**输入。
///
/// 输出顺序 = 分组键在输入里**首次出现的顺序**（`groups` 是个 Vec，`BTreeMap`
/// 只用来查"这个键见过没有"）。没有 `ORDER BY` 时 SQL 本来就不保证顺序，但这样
/// 至少是**完全确定的**，测试可以稳定断言。
///
/// 分组键里的 NULL 归成一组，与 SQL 一致。
struct AggregateSource<'a> {
    input: Box<dyn RowSource + 'a>,
    group_by: &'a [Prog],
    aggs: &'a [AggSpec],
    shape: ColumnsShape,
    node: NodeId,
    error: Option<DbError>,
    /// 攒好的输出行；`None` 表示还没算过。
    spool: Option<PhysTab>,
    at: usize,
}

impl<'a> AggregateSource<'a> {
    fn new(
        input: Box<dyn RowSource + 'a>,
        group_by: &'a [Prog],
        aggs: &'a [AggSpec],
        shape: ColumnsShape,
        node: NodeId,
    ) -> Self {
        AggregateSource {
            input,
            group_by,
            aggs,
            shape,
            node,
            error: None,
            spool: None,
            at: 0,
        }
    }

    /// 抽干输入、分组、物化输出。失败记进 `self.error`。
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
                let scalar = prog.eval(&row).map_err(|e| e.into_db_error(Some(self.node)))?;
                key.push(
                    scalar
                        .to_value()
                        .ok_or_else(|| runtime(self.node, "分组键不能是真值".to_string()))?,
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
                let value = match &spec.arg {
                    // COUNT(*)：不看值，来一行算一个
                    None => None,
                    Some(prog) => Some(
                        prog.eval(&row)
                            .map_err(|e| e.into_db_error(Some(self.node)))?
                            .to_value()
                            .ok_or_else(|| {
                                runtime(self.node, "聚合的参数不能是真值".to_string())
                            })?,
                    ),
                };
                groups[index].accums[slot].feed(value, self.node)?;
            }
        }

        if let Some(e) = self.input.error() {
            return Err(e.clone());
        }

        // 没有 GROUP BY 又一行输入都没有：标准要求**仍然产出一行**
        // （COUNT(*) = 0、AVG = NULL）
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

/// 比较一个排序键。
///
/// **NULL 的位置是绝对的**（由 `nulls_first` 决定），不跟着 `desc` 翻转——
/// `ORDER BY x DESC` 在 PostgreSQL 里是 NULLS FIRST，不是"把 ASC 的结果整个倒过来"。
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

/// 排序：要随机访问，所以第一次拉取时把输入**抽干**、算出排序键、稳定排序，再一行行吐。
///
/// 上面的 `LIMIT` 拉够行就停，但排序本身仍要把输入看完——这是算法性质。
struct SortSource<'a> {
    input: Box<dyn RowSource + 'a>,
    keys: &'a [SortKey],
    shape: ColumnsShape,
    node: NodeId,
    error: Option<DbError>,
    /// 排好序的行；`None` 表示还没算过。
    spool: Option<PhysTab>,
    at: usize,
}

impl<'a> SortSource<'a> {
    fn new(
        input: Box<dyn RowSource + 'a>,
        keys: &'a [SortKey],
        shape: ColumnsShape,
        node: NodeId,
    ) -> Self {
        SortSource {
            input,
            keys,
            shape,
            node,
            error: None,
            spool: None,
            at: 0,
        }
    }

    /// 抽干输入、排序、物化。失败记进 `self.error`。
    fn fill(&mut self) -> Result<()> {
        let mut keyed: Vec<(Vec<Scalar>, Row)> = Vec::new();

        while let Some(row) = self.input.next() {
            let mut values = Vec::with_capacity(self.keys.len());
            for key in self.keys {
                values.push(
                    key.prog
                        .eval(&row)
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

/// 截断：先拉掉 `offset` 行，再吐最多 `limit` 行。
///
/// 这是拉取式最直接的收益：**上面停手，下面就不再被拉**，`LIMIT` 之下那一大段
/// 根本不会被求值（那里的运行期错误也就不会冒出来——与 PostgreSQL 一致）。
struct LimitSource<'a> {
    input: Box<dyn RowSource + 'a>,
    limit: Option<u64>,
    offset: u64,
    emitted: u64,
    /// `offset` 只跳一次（后续的 `next` 不能重复跳）。
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
        // 先看限额再看 `offset`：`LIMIT 0` 一行都不该拉（连 `offset` 都不跳）
        if let Some(limit) = self.limit {
            if self.emitted >= limit {
                return None;
            }
        }

        if !self.skipped {
            self.skipped = true;
            let mut skipped = 0;
            while skipped < self.offset {
                // 输入不够跳就到此为止（不是错误）
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

fn runtime(node: NodeId, message: String) -> DbError {
    DbError::IrRuntime {
        node: Some(node),
        message,
    }
}
