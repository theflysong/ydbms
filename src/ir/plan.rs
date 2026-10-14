//! 线性计划 IR：一个扁平的节点数组。
//!
//! 计划**不是树**，而是 `Vec<Node>` + 一个 `root` 下标。节点之间的边是**下标**：
//! [`Node::inputs`] 里放的是更早的节点号。
//!
//! # 枢纽不变量 I1
//!
//! > 每个节点的 `inputs` 里所有下标，必须**严格小于**该节点自身的下标。
//!
//! 这一条同时给出三件事：
//!
//! 1. 图**无环**（只可能指向过去）；
//! 2. 正向扫描时，操作数一定已经算好了；
//! 3. 于是 **schema 推导与执行都是对一个数组的正向 `for` 循环**，不需要递归
//!    遍历（深计划也不会爆栈）。
//!
//! `explain` 从 `root` 顺着 `inputs` 反向走，用显式栈而不是递归。
//!
//! # 允许不可达节点
//!
//! `nodes` 里存在从 `root` 走不到的节点是**合法**的：将来的重写会把旧节点
//! 孤立在数组里。`analyze` / `execute` / `explain` 都只从 `root` 出发。

use crate::db::table::{Column, DataType, DbError, Result, TypeFamily};

use super::expr::{Prog, ScalarType, Step};
use super::symbol::SymbolTable;

/// 节点号，同时是它在 [`Plan::nodes`] 里的下标。
pub type NodeId = usize;

/// 连接类型。本版只做内连接。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// 内连接。
    Inner,
    /// 左外连接：左行的匹配行一个都没有时，右支的列全部填 NULL。
    Left,
}

impl JoinKind {
    /// 渲染用的名字。
    pub fn name(&self) -> &'static str {
        match self {
            JoinKind::Inner => "inner",
            JoinKind::Left => "left",
        }
    }
}

/// 一次等值连接用的键：两侧各一个程序，**各自面对自己那一支的 schema**。
///
/// 两个程序都是「按自己那一支编的」（外侧的下标相对左支、内侧的相对于右支），
/// 于是哈希表的两侧都能直接 `eval`，谁也不用知道对方的宽度。
///
/// **两侧必须同一大类**：`Value::Int(1)` 与 `Value::Numeric("1")` 在 `Value` 层面
/// 不是同一个值，混着当键会让哈希/索引**安静地漏掉**本该匹配的行。嵌套循环里的
/// 比较有 `widen_numeric` 兜着，键没有——所以这条由 [`Plan::analyze`] 静态强制。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqKey {
    /// 外侧（左支）的键。
    pub outer: Prog,
    /// 内侧（右支）的键。
    pub inner: Prog,
}

/// 连接的执行策略（由绑定器挑，见 [`crate::ir::lower`]）。
///
/// 三者对**输出**完全一致（行序都等于「外侧顺序 × 内侧顺序」），换的只是
/// 「内侧怎么找」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinStrategy {
    /// 朴素嵌套循环：内侧整体物化，外侧逐行去配。
    NestedLoop,
    /// 哈希连接：内侧抽干成 `键 → 行` 的哈希表，外侧流式探查。
    Hash {
        /// 等值键（至少一个）。
        keys: Vec<EqKey>,
    },
    /// 索引嵌套循环：内侧是**基表 [`Op::Scan`]**，给它建一次索引，
    /// 外侧每行按键查一次——内侧的行**不必物化**。
    ///
    /// 内侧 `Scan` 自己的 `index` 槽位这时用不上（连接建的是**连接列**上的索引），
    /// 它的 `filter` 仍要由执行器逐行复核。
    IndexNestedLoop {
        /// 等值键；`inner` 必须是**裸列**（索引只能建在一列上）。
        key: EqKey,
    },
}

impl JoinStrategy {
    /// 渲染用的名字。
    pub fn name(&self) -> &'static str {
        match self {
            JoinStrategy::NestedLoop => "nested-loop",
            JoinStrategy::Hash { .. } => "hash",
            JoinStrategy::IndexNestedLoop { .. } => "index-nested-loop",
        }
    }
}

/// 一次索引访问：走第 `column` 列上的索引，要查的键由 `key` 求值。
///
/// `key` **不能引用列**：索引是访问路径而不是算子，它没有输入行，键在空行上
/// 求值。这条由 [`Plan::analyze`] 强制。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPath {
    /// 索引建在哪一列（**这张表自己**的列下标）。
    pub column: usize,
    /// 要查的键；不读列。
    pub key: Prog,
}

/// 聚合函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    /// `COUNT(*)` / `COUNT(x)` / `COUNT(DISTINCT x)`
    Count,
    /// `SUM(x)`
    Sum,
    /// `AVG(x)`
    Avg,
    /// `MIN(x)`
    Min,
    /// `MAX(x)`
    Max,
}

impl AggFunc {
    /// 渲染用的名字。
    pub fn name(&self) -> &'static str {
        match self {
            AggFunc::Count => "COUNT",
            AggFunc::Sum => "SUM",
            AggFunc::Avg => "AVG",
            AggFunc::Min => "MIN",
            AggFunc::Max => "MAX",
        }
    }

    /// 结果类型。`arg` 是参数的类型（`COUNT(*)` 没有参数）。
    pub fn result_type(&self, arg: Option<ScalarType>) -> ScalarType {
        match self {
            AggFunc::Count => ScalarType::Value(TypeFamily::Int),
            // 整数求平均在 PostgreSQL 里也返回 numeric
            AggFunc::Avg => ScalarType::Value(TypeFamily::Numeric),
            AggFunc::Sum | AggFunc::Min | AggFunc::Max => {
                arg.unwrap_or(ScalarType::Value(TypeFamily::Int))
            }
        }
    }
}

/// 一个聚合：函数 + 参数（**按输入行**求值的程序）+ 是否 DISTINCT。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggSpec {
    /// 用哪个聚合函数。
    pub func: AggFunc,
    /// 参数程序。`COUNT(*)` 没有参数。
    pub arg: Option<Prog>,
    /// 是不是 `COUNT(DISTINCT x)`。
    pub distinct: bool,
}

/// 一个排序键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    /// 取值的程序。输入是 `Sort` 的输入 schema。
    pub prog: Prog,
    /// 是否降序。
    pub desc: bool,
    /// NULL 排在最前吗。
    ///
    /// 这是**绝对位置**，不受 `desc` 影响——PostgreSQL 的默认是
    /// ASC → NULLS LAST、DESC → NULLS FIRST，降级时就算好了填进来。
    pub nulls_first: bool,
}

/// 算子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// 从符号表按名字取表；`filter` 是**已经下推到存储**的谓词。
    Scan {
        /// 符号表里的名字。
        table: String,
        /// 下推到扫描的谓词；输入是这张表的列。
        filter: Option<Prog>,
        /// 索引访问路径；有它就按键取行，没有就顺序扫描。
        ///
        /// 它只是**访问路径**：`filter` 照旧逐行求值，所以路径挑错也只是慢，
        /// 不会算错。
        index: Option<IndexPath>,
    },
    /// 选择。
    Filter {
        /// 谓词；输入是下层节点的 schema。
        predicate: Prog,
    },
    /// 投影。
    Project {
        /// 每个输出列一个程序；输入是下层节点的 schema。
        progs: Vec<Prog>,
        /// 输出列名。
        names: Vec<String>,
    },
    /// 连接。
    Join {
        /// 连接类型。
        kind: JoinKind,
        /// 连接条件；输入是 **左 schema ++ 右 schema**。
        on: Option<Prog>,
        /// 怎么执行这次连接。
        strategy: JoinStrategy,
    },
    /// 分组聚合。
    ///
    /// 输出 schema = `[group_by 的列...] ++ [聚合的结果列...]`，上面接
    /// `Filter`(HAVING) 与 `Project`。`group_by` 与每个聚合的 `arg` 都是
    /// **按输入行**求值的程序；聚合结果靠**位置**被上层引用。
    Aggregate {
        /// 分组键（按输入行求值）。
        group_by: Vec<Prog>,
        /// 聚合列表。
        aggs: Vec<AggSpec>,
        /// 输出列名：先是 group_by 的，再是聚合的。
        names: Vec<String>,
    },
    /// 排序。
    Sort {
        /// 排序键。
        keys: Vec<SortKey>,
    },
    /// 截断。
    Limit {
        /// 最多取多少行；`None` 表示不限制。
        limit: Option<u64>,
        /// 先跳过多少行。
        offset: u64,
    },
    /// 单行零列的关系。`SELECT 1 + 1` 这种没有 FROM 的查询用它当底座。
    SingleRow,
}

impl Op {
    /// 这个算子需要几个输入。
    pub fn input_arity(&self) -> usize {
        match self {
            Op::Scan { .. } | Op::SingleRow => 0,
            Op::Filter { .. }
            | Op::Project { .. }
            | Op::Sort { .. }
            | Op::Limit { .. }
            | Op::Aggregate { .. } => 1,
            Op::Join { .. } => 2,
        }
    }

    /// 渲染用的名字。
    pub fn name(&self) -> &'static str {
        match self {
            Op::Scan { .. } => "Scan",
            Op::Filter { .. } => "Filter",
            Op::Project { .. } => "Project",
            Op::Aggregate { .. } => "Aggregate",
            Op::Join { .. } => "Join",
            Op::Sort { .. } => "Sort",
            Op::Limit { .. } => "Limit",
            Op::SingleRow => "SingleRow",
        }
    }
}

/// 计划里的一个节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// 算子。
    pub op: Op,
    /// 输入节点的下标。**不变量 I1**：全部严格小于本节点自身的下标。
    pub inputs: Vec<NodeId>,
}

/// 线性计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// 扁平节点数组，下标即 [`NodeId`]。
    pub nodes: Vec<Node>,
    /// 结果所在节点的下标。
    pub root: NodeId,
}

/// [`Plan::analyze`] 的产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// 每个节点的输出 schema，下标即 [`NodeId`]。
    pub schemas: Vec<Vec<Column>>,
    /// 每个节点上「程序所面对」的输入 schema。
    ///
    /// `Scan` 是它自己那张表的列，`Join` 是左 ++ 右，`Filter`/`Project`/`Sort`
    /// 是下层节点的 schema，`Limit`/`SingleRow` 没有程序所以是 `None`。
    pub program_inputs: Vec<Option<Vec<Column>>>,
}

impl Analysis {
    /// 某个节点上程序所面对的输入 schema；没有程序则返回空切片。
    pub fn program_input(&self, id: NodeId) -> &[Column] {
        self.program_inputs
            .get(id)
            .and_then(|s| s.as_deref())
            .unwrap_or(&[])
    }
}

impl Plan {
    /// 一次正向扫描，把该检查的全检查了。
    ///
    /// 顺序读一遍 `nodes`（靠 I1，操作数一定在前）：
    ///
    /// * 输入个数与算子匹配、输入下标严格小于自身下标；
    /// * 每个程序 [`Prog::verify`]（寄存器先定义后使用、列下标不越界、类型正确）；
    /// * 谓词必须是 `BOOL`、排序键与结果列必须是 `INT`；
    /// * 推导出每个节点的输出 schema。
    pub fn analyze(&self, symbols: &SymbolTable) -> Result<Analysis> {
        if self.nodes.is_empty() {
            return Err(DbError::IrInvalid {
                node: None,
                message: "计划里没有任何节点".to_string(),
            });
        }
        if self.root >= self.nodes.len() {
            return Err(DbError::IrInvalid {
                node: None,
                message: format!("root {} 越界（共 {} 个节点）", self.root, self.nodes.len()),
            });
        }

        let mut schemas: Vec<Vec<Column>> = Vec::with_capacity(self.nodes.len());
        let mut program_inputs: Vec<Option<Vec<Column>>> = Vec::with_capacity(self.nodes.len());

        for (id, node) in self.nodes.iter().enumerate() {
            let want = node.op.input_arity();
            if node.inputs.len() != want {
                return Err(invalid(id, format!(
                    "{} 需要 {} 个输入，实际给了 {}",
                    node.op.name(),
                    want,
                    node.inputs.len()
                )));
            }
            for &input in &node.inputs {
                if input >= id {
                    return Err(invalid(id, format!(
                        "输入下标 {} 必须严格小于自身下标 {}（不变量 I1：拓扑序）",
                        input, id
                    )));
                }
            }

            let (schema, program_input) = match &node.op {
                Op::SingleRow => (Vec::new(), None),

                Op::Scan {
                    table,
                    filter,
                    index,
                } => {
                    let columns = symbols
                        .columns(table)
                        .ok_or_else(|| DbError::UnknownSymbol {
                            name: table.clone(),
                        })?
                        .to_vec();
                    if let Some(prog) = filter {
                        let ty = prog.verify(&columns).map_err(|e| at(id, e))?;
                        expect(ty, ScalarType::Bool, id, "Scan.filter")?;
                    }
                    if let Some(path) = index {
                        verify_index_path(path, table, &columns, id)?;
                    }
                    (columns.clone(), Some(columns))
                }

                Op::Filter { predicate } => {
                    let input = schemas[node.inputs[0]].clone();
                    let ty = predicate.verify(&input).map_err(|e| at(id, e))?;
                    expect(ty, ScalarType::Bool, id, "Filter.predicate")?;
                    (input.clone(), Some(input))
                }

                Op::Project { progs, names } => {
                    let input = schemas[node.inputs[0]].clone();
                    if progs.is_empty() {
                        return Err(invalid(id, "Project 至少要有 1 个输出列".to_string()));
                    }
                    if progs.len() != names.len() {
                        return Err(invalid(id, format!(
                            "Project 有 {} 个表达式，却有 {} 个名字",
                            progs.len(),
                            names.len()
                        )));
                    }
                    let mut columns = Vec::with_capacity(progs.len());
                    for (prog, name) in progs.iter().zip(names.iter()) {
                        columns.push(project_column(prog, name, &input).map_err(|e| at(id, e))?);
                    }
                    (columns.clone(), Some(input))
                }

                Op::Aggregate {
                    group_by,
                    aggs,
                    names,
                } => {
                    let input = schemas[node.inputs[0]].clone();

                    if names.len() != group_by.len() + aggs.len() {
                        return Err(invalid(id, format!(
                            "Aggregate 的 {} 个输出列对不上 {} 个分组键 + {} 个聚合",
                            names.len(),
                            group_by.len(),
                            aggs.len()
                        )));
                    }

                    let mut columns = Vec::with_capacity(names.len());

                    for (prog, name) in group_by.iter().zip(names.iter()) {
                        columns.push(project_column(prog, name, &input).map_err(|e| at(id, e))?);
                    }

                    for (spec, name) in aggs.iter().zip(names.iter().skip(group_by.len())) {
                        let arg = match &spec.arg {
                            Some(prog) => Some(prog.verify(&input).map_err(|e| at(id, e))?),
                            None => None,
                        };
                        let ty = spec.func.result_type(arg);
                        columns.push(Column::new(
                            name.clone(),
                            scalar_default_type(ty, id, &format!("{}({})", spec.func.name(), name))?,
                        ));
                    }

                    (columns.clone(), Some(input))
                }

                Op::Join {
                    kind,
                    on,
                    strategy,
                } => {
                    let left = schemas[node.inputs[0]].clone();
                    let mut right = schemas[node.inputs[1]].clone();

                    // 左外连接里，没匹配上的行会让**右支所有列**变成 NULL。
                    // 所以右支的 NOT NULL 不能带过来——带过来就会在真正产生
                    // 外连接 NULL 的时候报"列声明了 NOT NULL"，把合法结果判成错。
                    if *kind == JoinKind::Left {
                        for column in &mut right {
                            column.not_null = false;
                        }
                    }

                    // 策略的守卫先用**分开的**左/右 schema 校验（键是两侧各一份程序）
                    let inner = &self.nodes[node.inputs[1]].op;
                    verify_join_strategy(strategy, &left, &right, on.as_ref(), inner, symbols, id)?;

                    let mut input = left;
                    input.extend(right);
                    if let Some(prog) = on {
                        let ty = prog.verify(&input).map_err(|e| at(id, e))?;
                        expect(ty, ScalarType::Bool, id, "Join.on")?;
                    }
                    (input.clone(), Some(input))
                }

                Op::Sort { keys } => {
                    let input = schemas[node.inputs[0]].clone();
                    if keys.is_empty() {
                        return Err(invalid(id, "Sort 至少要有 1 个排序键".to_string()));
                    }
                    for key in keys {
                        let ty = key.prog.verify(&input).map_err(|e| at(id, e))?;
                        // 排序键可以是任何**可存储**类型（文本按字典序、时间戳按
                        // 定宽文本、NUMERIC 按手写的定点比较）；只有布尔不行。
                        if matches!(ty, ScalarType::Bool) {
                            return Err(invalid(
                                id,
                                format!("排序键必须是可存储的值，实际是 {}", ty),
                            ));
                        }
                    }
                    (input.clone(), Some(input))
                }

                Op::Limit { .. } => (schemas[node.inputs[0]].clone(), None),
            };

            schemas.push(schema);
            program_inputs.push(program_input);
        }

        Ok(Analysis {
            schemas,
            program_inputs,
        })
    }

    /// root 的输出 schema。
    pub fn schema(&self, symbols: &SymbolTable) -> Result<Vec<Column>> {
        let analysis = self.analyze(symbols)?;
        Ok(analysis.schemas[self.root].clone())
    }

    /// 把计划渲染成一棵可读的树（用显式栈，不递归）。
    pub fn explain(&self, symbols: &SymbolTable) -> Result<String> {
        let analysis = self.analyze(symbols)?;
        let mut out = String::new();
        let mut stack: Vec<(NodeId, usize)> = vec![(self.root, 0)];

        while let Some((id, depth)) = stack.pop() {
            let node = &self.nodes[id];
            out.push_str(&"  ".repeat(depth));
            out.push_str(&render(id, node, &analysis));
            out.push('\n');
            // 倒着压栈，弹出顺序才与 inputs 的顺序一致
            for &child in node.inputs.iter().rev() {
                stack.push((child, depth + 1));
            }
        }

        Ok(out)
    }
}

/// 增量构建 [`Plan`]，顺手挡住坏结构。
///
/// 每个 `push` 都校验输入下标严格小于新节点下标（不变量 I1）；`finish` 校验
/// `root` 不越界。输入个数本来就由各方法固定，所以那一条在这里不可能出错。
#[derive(Debug, Default)]
pub struct PlanBuilder {
    nodes: Vec<Node>,
}

impl PlanBuilder {
    /// 空构建器。
    pub fn new() -> Self {
        PlanBuilder { nodes: Vec::new() }
    }

    /// 当前已有多少节点（也就是下一个节点的下标）。
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// 构建器里还没有节点。
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 压入一个节点，返回它的下标。
    pub fn push(&mut self, op: Op, inputs: Vec<NodeId>) -> Result<NodeId> {
        let id = self.nodes.len();
        if inputs.len() != op.input_arity() {
            return Err(invalid(id, format!(
                "{} 需要 {} 个输入，实际给了 {}",
                op.name(),
                op.input_arity(),
                inputs.len()
            )));
        }
        for &input in &inputs {
            if input >= id {
                return Err(invalid(id, format!(
                    "输入下标 {} 必须严格小于新节点下标 {}（不变量 I1：拓扑序）",
                    input, id
                )));
            }
        }
        self.nodes.push(Node { op, inputs });
        Ok(id)
    }

    /// 扫描一张表。
    pub fn scan(&mut self, table: &str) -> NodeId {
        self.nodes.push(Node {
            op: Op::Scan {
                table: table.to_string(),
                filter: None,
                index: None,
            },
            inputs: Vec::new(),
        });
        self.nodes.len() - 1
    }

    /// 扫描一张表，并把谓词下推到存储。
    pub fn scan_with_filter(&mut self, table: &str, filter: Prog) -> NodeId {
        self.nodes.push(Node {
            op: Op::Scan {
                table: table.to_string(),
                filter: Some(filter),
                index: None,
            },
            inputs: Vec::new(),
        });
        self.nodes.len() - 1
    }

    /// 把谓词**并进**某个 `Scan` 已经有的 `filter`（谓词下推的落点）。
    ///
    /// 本来没有 filter 就直接挂上；已经有一个就 `AND` 上去（两个合取项仍然是一个
    /// 程序，扫一遍就够）。
    pub fn add_scan_filter(&mut self, id: NodeId, filter: Prog) -> Result<()> {
        match self.nodes.get_mut(id).map(|node| &mut node.op) {
            Some(Op::Scan { filter: slot, .. }) => {
                *slot = Some(match slot.take() {
                    None => filter,
                    Some(existing) => existing.and(filter)?,
                });
                Ok(())
            }
            _ => Err(invalid(id, "add_scan_filter 只对 Scan 节点有意义".to_string())),
        }
    }

    /// 给某个 `Scan` 挂上索引访问路径。
    pub fn set_scan_index(&mut self, id: NodeId, path: IndexPath) -> Result<()> {
        match self.nodes.get_mut(id).map(|node| &mut node.op) {
            Some(Op::Scan { index, .. }) => {
                *index = Some(path);
                Ok(())
            }
            _ => Err(invalid(id, "set_scan_index 只对 Scan 节点有意义".to_string())),
        }
    }

    /// 看一个已经压进去的节点的算子（绑定器挑连接策略时要看内侧是不是基表 `Scan`）。
    pub fn op(&self, id: NodeId) -> Option<&Op> {
        self.nodes.get(id).map(|node| &node.op)
    }

    /// 单行零列。
    pub fn single_row(&mut self) -> NodeId {
        self.nodes.push(Node {
            op: Op::SingleRow,
            inputs: Vec::new(),
        });
        self.nodes.len() - 1
    }

    /// 选择。
    pub fn filter(&mut self, input: NodeId, predicate: Prog) -> Result<NodeId> {
        self.push(Op::Filter { predicate }, vec![input])
    }

    /// 投影。
    pub fn project(
        &mut self,
        input: NodeId,
        progs: Vec<Prog>,
        names: Vec<String>,
    ) -> Result<NodeId> {
        self.push(Op::Project { progs, names }, vec![input])
    }

    /// 连接。
    pub fn join(
        &mut self,
        left: NodeId,
        right: NodeId,
        kind: JoinKind,
        on: Option<Prog>,
        strategy: JoinStrategy,
    ) -> Result<NodeId> {
        self.push(
            Op::Join {
                kind,
                on,
                strategy,
            },
            vec![left, right],
        )
    }

    /// 排序。
    pub fn sort(&mut self, input: NodeId, keys: Vec<SortKey>) -> Result<NodeId> {
        self.push(Op::Sort { keys }, vec![input])
    }

    /// 截断。
    pub fn limit(&mut self, input: NodeId, limit: Option<u64>, offset: u64) -> Result<NodeId> {
        self.push(Op::Limit { limit, offset }, vec![input])
    }

    /// 收尾。
    pub fn finish(self, root: NodeId) -> Result<Plan> {
        if self.nodes.is_empty() {
            return Err(DbError::IrInvalid {
                node: None,
                message: "计划里没有任何节点".to_string(),
            });
        }
        if root >= self.nodes.len() {
            return Err(DbError::IrInvalid {
                node: None,
                message: format!("root {} 越界（共 {} 个节点）", root, self.nodes.len()),
            });
        }
        Ok(Plan {
            nodes: self.nodes,
            root,
        })
    }
}

/// 推导一个投影输出列。
///
/// * **原样透传的列引用**（program 恰好只有一条 `Col(i)` 指令）→ 保留那一列自己
///   的类型，**含 typmod**。所以 `SELECT name FROM t` 出来的 `name` 还是
///   `VARCHAR(30)`，而不是退化成 `TEXT`。
/// * 计算出来的表达式 → 退化成它大类的默认类型。
///
/// `lower` 里给 `ORDER BY` 建输出作用域时也用它，两处必须一致。
pub fn project_column(prog: &Prog, name: &str, input: &[Column]) -> Result<Column> {
    let ty = prog.verify(input)?;
    let family = match ty {
        ScalarType::Value(family) => family,
        // 无类型 NULL 投影出来就是 TEXT（与 `SELECT NULL` 在 PostgreSQL 里一致）
        ScalarType::Null => TypeFamily::Text,
        ScalarType::Bool => {
            return Err(DbError::IrInvalid {
                node: None,
                message: format!("结果列 `{}` 是 {}，布尔不能作为结果列", name, ty),
            })
        }
    };

    let data_type = match passthrough_column(prog) {
        // 透传列：保留原类型（也就是保留 typmod）
        Some(index) => input
            .get(index)
            .map(|column| column.ty)
            .unwrap_or_else(|| family.default_type()),
        None => family.default_type(),
    };

    Ok(Column::new(name, data_type))
}

/// 标量类型 → 一个具体的默认类型（用来给计算出来的列贴标签）。
fn scalar_default_type(ty: ScalarType, node: NodeId, what: &str) -> Result<DataType> {
    match ty {
        ScalarType::Value(family) => Ok(family.default_type()),
        ScalarType::Null => Ok(TypeFamily::Text.default_type()),
        ScalarType::Bool => Err(invalid(node, format!("{} 不能是布尔", what))),
    }
}

/// 这个程序是不是「原样取第 i 列」。
pub fn passthrough_column(prog: &Prog) -> Option<usize> {
    match prog.steps() {
        [Step::Col { dst: 0, index }] if prog.result() == 0 => Some(*index),
        _ => None,
    }
}

fn at(node: NodeId, e: DbError) -> DbError {
    match e {
        DbError::IrInvalid { message, .. } => DbError::IrInvalid {
            node: Some(node),
            message,
        },
        other => other,
    }
}

/// 校验一条索引访问路径。
///
/// 两条检查都是为了**不安静地漏行**：
///
/// * 键不能读列——索引是访问路径而不是算子，键在没有输入行的情况下求值；
/// * 键与索引列必须**同一大类**——`Value::Int(1)` 与 `Value::Numeric("1")` 在值
///   层面不是同一个键，混着用会漏掉本该匹配的行。
fn verify_index_path(path: &IndexPath, table: &str, columns: &[Column], id: NodeId) -> Result<()> {
    let column = columns.get(path.column).ok_or_else(|| {
        invalid(
            id,
            format!(
                "索引列 {} 越界（`{}` 只有 {} 列）",
                path.column,
                table,
                columns.len()
            ),
        )
    })?;

    let ty = path.key.verify(columns).map_err(|e| at(id, e))?;
    if !path.key.columns().is_empty() {
        return Err(invalid(
            id,
            "索引的键不能引用列：它在没有输入行的情况下求值".to_string(),
        ));
    }

    match ty {
        ScalarType::Value(family) if family == column.ty.family() => Ok(()),
        other => Err(invalid(
            id,
            format!(
                "索引键的类型 {} 与列 `{}` 的类型 {} 不是同一大类——跨大类在值层面是不同的键，\
                 用索引会安静地漏掉本该匹配的行",
                other, column.name, column.ty
            ),
        )),
    }
}

/// 校验一个等值键：两侧各自面对自己那一支的 schema，且**同一大类**。
fn verify_eq_key(key: &EqKey, left: &[Column], right: &[Column], id: NodeId, what: &str) -> Result<()> {
    let outer = key.outer.verify(left).map_err(|e| at(id, e))?;
    let inner = key.inner.verify(right).map_err(|e| at(id, e))?;

    match (outer, inner) {
        (ScalarType::Value(a), ScalarType::Value(b)) if a == b => Ok(()),
        _ => Err(invalid(
            id,
            format!(
                "{} 的两侧必须同一大类，实际是 {} 与 {}（跨大类的值在哈希表里不是同一个键）",
                what, outer, inner
            ),
        )),
    }
}

/// 校验连接策略：与 `ON` 配不配套、键同不同族、内侧能不能建索引。
///
/// 这些都为了让**手写的不一致计划**直接报错，而不是跑出一个奇怪的结果。
fn verify_join_strategy(
    strategy: &JoinStrategy,
    left: &[Column],
    right: &[Column],
    on: Option<&Prog>,
    inner: &Op,
    symbols: &SymbolTable,
    id: NodeId,
) -> Result<()> {
    match strategy {
        JoinStrategy::NestedLoop => Ok(()),

        JoinStrategy::Hash { keys } => {
            if on.is_none() {
                return Err(invalid(id, "哈希连接要求有 ON".to_string()));
            }
            if keys.is_empty() {
                return Err(invalid(id, "哈希连接至少要有一个等值键".to_string()));
            }
            for key in keys {
                verify_eq_key(key, left, right, id, "哈希连接的等值键")?;
            }
            Ok(())
        }

        JoinStrategy::IndexNestedLoop { key } => {
            if on.is_none() {
                return Err(invalid(id, "索引嵌套循环要求有 ON".to_string()));
            }

            let table = match inner {
                Op::Scan { table, .. } => table,
                _ => {
                    return Err(invalid(
                        id,
                        "索引嵌套循环要求内侧是一个基表 Scan（索引建在基表上）".to_string(),
                    ))
                }
            };

            let index_column = passthrough_column(&key.inner).ok_or_else(|| {
                invalid(
                    id,
                    "索引嵌套循环的内侧键必须是裸列（索引只能建在一列上）".to_string(),
                )
            })?;

            let columns = symbols.columns(table).ok_or_else(|| DbError::UnknownSymbol {
                name: table.clone(),
            })?;
            let column = columns.get(index_column).ok_or_else(|| {
                invalid(
                    id,
                    format!("索引列 {} 越界（`{}` 只有 {} 列）", index_column, table, columns.len()),
                )
            })?;

            let outer = key.outer.verify(left).map_err(|e| at(id, e))?;
            match outer {
                ScalarType::Value(family) if family == column.ty.family() => Ok(()),
                other => Err(invalid(
                    id,
                    format!(
                        "索引嵌套循环的键两侧必须同一大类，实际是 {} 与 `{}.{}` 的 {}",
                        other, table, column.name, column.ty
                    ),
                )),
            }
        }
    }
}

fn invalid(node: NodeId, message: String) -> DbError {
    DbError::IrInvalid {
        node: Some(node),
        message,
    }
}

fn expect(actual: ScalarType, want: ScalarType, node: NodeId, what: &str) -> Result<()> {
    if actual == want {
        Ok(())
    } else {
        Err(invalid(
            node,
            format!("{} 必须是 {}，实际是 {}", what, want, actual),
        ))
    }
}

fn render(id: NodeId, node: &Node, analysis: &Analysis) -> String {
    let input = analysis.program_input(id);
    match &node.op {
        Op::SingleRow => "SingleRow".to_string(),
        Op::Scan {
            table,
            filter,
            index,
        } => {
            let path = match index {
                Some(path) => format!(
                    "index=[{}], ",
                    input
                        .get(path.column)
                        .map(|column| column.name.as_str())
                        .unwrap_or("?")
                ),
                None => String::new(),
            };
            match filter {
                Some(prog) => format!(
                    "Scan({}, {path}filter=[{}])",
                    table,
                    prog.explain(input)
                ),
                None => format!("Scan({}, {path}filter=[])", table),
            }
        }
        Op::Filter { predicate } => format!("Filter([{}])", predicate.explain(input)),
        Op::Project { progs, names } => {
            let exprs: Vec<String> = progs.iter().map(|p| p.explain(input)).collect();
            format!(
                "Project(names=[{}], exprs=[{}])",
                names.join(", "),
                exprs.join(", ")
            )
        }
        Op::Join {
            kind,
            on,
            strategy,
        } => {
            // 等值键的两个程序各按自己那一支的 schema 渲染
            let left = analysis.program_input(node.inputs[0]);
            let right = analysis.program_input(node.inputs[1]);
            let one_key = |key: &EqKey| {
                format!(
                    "{} = {}",
                    key.outer.explain(left),
                    key.inner.explain(right)
                )
            };
            let picked = match strategy {
                JoinStrategy::NestedLoop => String::new(),
                JoinStrategy::Hash { keys } => {
                    let rendered: Vec<String> = keys.iter().map(one_key).collect();
                    format!("hash=[{}], ", rendered.join(", "))
                }
                JoinStrategy::IndexNestedLoop { key } => {
                    format!("index={}, ", key.inner.explain(right))
                }
            };
            match on {
                Some(prog) => format!(
                    "Join({}, {}on=[{}])",
                    kind.name(),
                    picked,
                    prog.explain(input)
                ),
                None => format!("Join({}, {}on=[])", kind.name(), picked),
            }
        }
        Op::Aggregate {
            group_by,
            aggs,
            names,
        } => {
            let groups: Vec<String> = group_by.iter().map(|p| p.explain(input)).collect();
            let list: Vec<String> = aggs
                .iter()
                .map(|spec| match (&spec.arg, spec.distinct) {
                    (None, _) => format!("{}(*)", spec.func.name()),
                    (Some(prog), true) => {
                        format!("{}(DISTINCT {})", spec.func.name(), prog.explain(input))
                    }
                    (Some(prog), false) => {
                        format!("{}({})", spec.func.name(), prog.explain(input))
                    }
                })
                .collect();
            format!(
                "Aggregate(names=[{}], group_by=[{}], aggs=[{}])",
                names.join(", "),
                groups.join(", "),
                list.join(", ")
            )
        }
        Op::Sort { keys } => {
            let rendered: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{} {}",
                        k.prog.explain(input),
                        if k.desc { "DESC" } else { "ASC" }
                    )
                })
                .collect();
            format!("Sort(keys=[{}])", rendered.join(", "))
        }
        Op::Limit { limit, offset } => match limit {
            Some(limit) => format!("Limit(limit={}, offset={})", limit, offset),
            None => format!("Limit(limit=none, offset={})", offset),
        },
    }
}
