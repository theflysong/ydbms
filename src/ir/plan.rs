//! 线性计划 IR:一个扁平的节点数组.
//!
//! 计划**不是树**,而是 `Vec<Node>` + 一个 `root` 下标.节点之间的边是**下标**:
//! [`Node::inputs`] 里放的是更早的节点号.
//!
//! # 枢纽不变量 I1
//!
//! > 每个节点的 `inputs` 里所有下标,必须**严格小于**该节点自身的下标.
//!
//! 这一条同时给出三件事:
//!
//! 1. 图**无环**(只可能指向过去);
//! 2. 正向扫描时,操作数一定已经算好了;
//! 3. 于是 **schema 推导与执行都是对一个数组的正向 `for` 循环**,不需要递归
//!    遍历(深计划也不会爆栈).
//!
//! `explain` 从 `root` 顺着 `inputs` 反向走,用显式栈而不是递归.
//!
//! # 允许不可达节点
//!
//! `nodes` 里存在从 `root` 走不到的节点是**合法**的:将来的重写会把旧节点
//! 孤立在数组里.`analyze` / `execute` / `explain` 都只从 `root` 出发.
//!
//! # 子查询的校验分两处
//!
//! [`Prog::verify`] 只看 step 自己的类型(它连符号表都没有),所以
//! [`Plan::analyze`] 在正向扫完节点之后还会走一遍私有的 `check_subqueries`:
//! 内层计划自己也要过浅层校验,`SubPlan::outers` 还要与"执行时压着的那几层外层行"
//! 逐层对上,`Step::Outer` 的下标与类型也在这里核对.绑定器算错外层下标只有这一关
//! 抓得住--见 `crate::ir::exec` 的 `ExecCtx`.
//!
//! # 派生表怎么进来
//!
//! [`PlanBuilder::splice`] 把一个计划(派生表)的节点整体接进另一个数组,只改
//! `inputs` 的偏移--**没有"派生表"这个算子**,它在外层看来就是一段子计划.
//! `UNION [ALL]` 则有自己的 [`Op::Union`];`SELECT DISTINCT` 是 [`Op::Distinct`].

use std::collections::{BTreeMap, BTreeSet};

use crate::db::table::{Column, DataType, DbError, Result, TypeFamily};

use super::expr::{Prog, Step, SubMode};
use super::value::ScalarType;
use super::symbol::SymbolTable;

/// 节点号,同时是它在 [`Plan::nodes`] 里的下标.
pub type NodeId = usize;

/// 连接类型.本版只做内连接.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// 内连接.
    Inner,
    /// 左外连接:左行的匹配行一个都没有时,右支的列全部填 NULL.
    Left,
}

impl JoinKind {
    /// 渲染用的名字.
    pub fn name(&self) -> &'static str {
        match self {
            JoinKind::Inner => "inner",
            JoinKind::Left => "left",
        }
    }
}

/// 一次等值连接用的键:两侧各一个程序,**各自面对自己那一支的 schema**.
///
/// 两个程序都是"按自己那一支编的"(外侧的下标相对左支,内侧的相对于右支),
/// 于是哈希表的两侧都能直接 `eval`,谁也不用知道对方的宽度.
///
/// **两侧必须同一大类**:`Value::Int(1)` 与 `Value::Numeric("1")` 在 `Value` 层面
/// 不是同一个值,混着当键会让哈希/索引**安静地漏掉**本该匹配的行.嵌套循环里的
/// 比较有 `widen_numeric` 兜着,键没有--所以这条由 [`Plan::analyze`] 静态强制.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqKey {
    /// 外侧(左支)的键.
    pub outer: Prog,
    /// 内侧(右支)的键.
    pub inner: Prog,
}

/// 连接的执行策略(由绑定器挑,见 [`crate::ir::irgen`]).
///
/// 三者对**输出**完全一致(行序都等于"外侧顺序 × 内侧顺序"),换的只是
/// "内侧怎么找".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinStrategy {
    /// 朴素嵌套循环:内侧整体物化,外侧逐行去配.
    NestedLoop,
    /// 哈希连接:内侧抽干成 `键 → 行` 的哈希表,外侧流式探查.
    Hash {
        /// 等值键(至少一个).
        keys: Vec<EqKey>,
    },
    /// 索引嵌套循环:内侧是**基表 [`Op::Scan`]**,在它的连接列上查目录里的索引,
    /// 外侧每行按键查一次--内侧的行**不必物化**.
    ///
    /// 索引是**目录对象**([`crate::ir::SymbolTable::index_of`]):只有 `CREATE INDEX`
    /// 建过的列才挑得出这条策略(见 `ir::opt`).索引还没建时,
    /// [`crate::db::index::IndexSlot`] 会在第一次查找时按需建一份.
    ///
    /// 内侧 `Scan` 自己的 `index` 槽位这时用不上(连接用的是**连接列**上的索引),
    /// 它的 `filter` 仍要由执行器逐行复核.
    IndexNestedLoop {
        /// 等值键;`inner` 必须是**裸列**(索引只能建在一列上).
        key: EqKey,
    },
}

impl JoinStrategy {
    /// 渲染用的名字.
    pub fn name(&self) -> &'static str {
        match self {
            JoinStrategy::NestedLoop => "nested-loop",
            JoinStrategy::Hash { .. } => "hash",
            JoinStrategy::IndexNestedLoop { .. } => "index-nested-loop",
        }
    }
}

/// 一次索引访问:走第 `column` 列上的索引,要查的键由 `keys` 求值.
///
/// `keys` **不能引用本查询的列**(索引是访问路径而不是算子,它没有输入行,键在
/// 空的输入行上求值)--这条由 [`Plan::analyze`] 强制.
///
/// 键是**一组**而不是一个:`col IN (常量, ...)`(TODO 3.14(a))是一个键一项,
/// 执行时把各键的桶合并起来就是命中的行号;等值那种只写一个.这一组**至少有一项**
/// --`IN` 的空列表在降级期就被拒了(`ir.irgen.188`),别的写法也挑不出空的一组.
///
/// # `outer`:相关子查询里作索引探测(TODO 8.7)
///
/// 相关子查询的内层查询**只有一张表**,于是 `c.tconst = b.tconst` 这种谓词既
/// 没有别的 FROM 项可连接(`opt::join` 的索引嵌套循环挂不上去),又不是常量
/// (对不上"键不读列"那条快路),只能退化成 `Scan` 之后的逐行 `Filter`--
/// 内层每来一行都要把整张表扫一遍.
///
/// `outer` 为 `true` 时 `keys[0]` 读的是**外层行**(`Step::Outer`),于是这一列上
/// 的索引可以当**探测**用:`index=[tconst]` 加一个外层键,内层一次只读命中那几行.
/// 这一种**恰好一个键**(一次探测一个值),`analyze` 会挡下更多的那一种.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPath {
    /// 索引建在哪一列(**这张表自己**的列下标).
    pub column: usize,
    /// 要查的键(至少一项);不读本查询的列,但 `outer` 时**可以**读外层行.
    pub keys: Vec<Prog>,
    /// 键读的是**外层行**吗(相关子查询的等值探测).
    pub outer: bool,
}

/// 聚合函数.
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
    /// 渲染用的名字.
    pub fn name(&self) -> &'static str {
        match self {
            AggFunc::Count => "COUNT",
            AggFunc::Sum => "SUM",
            AggFunc::Avg => "AVG",
            AggFunc::Min => "MIN",
            AggFunc::Max => "MAX",
        }
    }

    /// 结果类型.`arg` 是参数的类型(`COUNT(*)` 没有参数).
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

/// 一个聚合:函数 + 参数(**按输入行**求值的程序)+ 是否 DISTINCT + `FILTER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggSpec {
    /// 用哪个聚合函数.
    pub func: AggFunc,
    /// 参数程序.`COUNT(*)` 没有参数.
    pub arg: Option<Prog>,
    /// 是不是 `COUNT(DISTINCT x)`.
    pub distinct: bool,
    /// `FILTER (WHERE ...)`:只有它为 `TRUE` 的行才进累加器.
    ///
    /// 谓词按**输入行**求值(与 [`AggSpec::arg`] 同一个 schema),三值逻辑里
    /// `Unknown` 与 `FALSE` 一样被丢掉.
    pub filter: Option<Prog>,
}

/// 一个排序键.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    /// 取值的程序.输入是 `Sort` 的输入 schema.
    pub prog: Prog,
    /// 是否降序.
    pub desc: bool,
    /// NULL 排在最前吗.
    ///
    /// 这是**绝对位置**,不受 `desc` 影响--PostgreSQL 的默认是
    /// ASC → NULLS LAST,DESC → NULLS FIRST,降级时就算好了填进来.
    pub nulls_first: bool,
}

/// 算子.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// 从符号表按名字取表;`filter` 是**已经下推到存储**的谓词.
    Scan {
        /// 符号表里的名字.
        table: String,
        /// 下推到扫描的谓词;输入是这张表的列.
        filter: Option<Prog>,
        /// 索引访问路径;有它就按键取行,没有就顺序扫描.
        ///
        /// 它只是**访问路径**:`filter` 照旧逐行求值,所以路径挑错也只是慢,
        /// 不会算错.`IndexPath::outer` 那一份(相关子查询的等值探测)同理--
        /// 谓词本身留在上面的 `Filter` 里,索引只是让它少读几行.
        index: Option<IndexPath>,
    },
    /// 选择.
    Filter {
        /// 谓词;输入是下层节点的 schema.
        predicate: Prog,
    },
    /// 投影.
    Project {
        /// 每个输出列一个程序;输入是下层节点的 schema.
        progs: Vec<Prog>,
        /// 输出列名.
        names: Vec<String>,
    },
    /// 连接.
    Join {
        /// 连接类型.
        kind: JoinKind,
        /// 连接条件;输入是 **左 schema ++ 右 schema**.
        on: Option<Prog>,
        /// 怎么执行这次连接.
        strategy: JoinStrategy,
    },
    /// 分组聚合.
    ///
    /// 输出 schema = `[group_by 的列...] ++ [聚合的结果列...]`,上面接
    /// `Filter`(HAVING) 与 `Project`.`group_by` 与每个聚合的 `arg` 都是
    /// **按输入行**求值的程序;聚合结果靠**位置**被上层引用.
    Aggregate {
        /// 分组键(按输入行求值).
        group_by: Vec<Prog>,
        /// 聚合列表.
        aggs: Vec<AggSpec>,
        /// 输出列名:先是 group_by 的,再是聚合的.
        names: Vec<String>,
    },
    /// 排序.
    Sort {
        /// 排序键.
        keys: Vec<SortKey>,
    },
    /// 去重(`SELECT DISTINCT`):流式过滤,见过的行不再往下放.
    ///
    /// 输出 schema 与输入**完全一样**--它只丢行,不改行.
    Distinct,
    /// 集合运算 `UNION [ALL]`.
    ///
    /// 两个输入,输出 schema 就是**左支**的(列名也取左支的).`all == false` 时
    /// 去重--`UNION` 的语义,`UNION ALL` 保留重复.两支逐列必须同族,否则报
    /// `IrInvalid`(跨大类的值放在同一列里只会给出安静的错答案).
    Union {
        /// 是 `UNION ALL` 吗(否则去重).
        all: bool,
    },
    /// 截断.
    Limit {
        /// 最多取多少行;`None` 表示不限制.
        limit: Option<u64>,
        /// 先跳过多少行.
        offset: u64,
    },
    /// 数一张基表的行数,**一行都不读**(TODO 3.8).
    ///
    /// `SELECT COUNT(*) FROM t`(单张基表,没有 `WHERE`,没有 `GROUP BY`,聚合只有
    /// 一个 `COUNT(*)`)走它:答案就是 [`Table::nrows`](crate::db::table::Table::nrows),
    /// 那是现成的事实,不必把整张表拉一遍再数一遍.输出 schema 与"一个 `COUNT(*)`
    /// 的 [`Op::Aggregate`]"**逐字相同**(一列 `INT`),所以它上面那半段
    /// (`HAVING` / `ORDER BY` / `Project` / `LIMIT`)一个字都不用改.
    ///
    /// 表名与 `Scan` 一样是**符号表里的名字**(`search_path` 还没展开),
    /// 所以它也照样被 [`Plan::used_columns`] 报给权限检查--`COUNT(*)` 的
    /// "一列都没读"是**空集合**,不是"没有这张表".
    CountRows {
        /// 符号表里的名字.
        table: String,
        /// 输出列名(与 `Aggregate` 那条路一样,取自聚合函数名).
        name: String,
    },
    /// 单行零列的关系.`SELECT 1 + 1` 这种没有 FROM 的查询用它当底座.
    SingleRow,
}

impl Op {
    /// 这个算子需要几个输入.
    pub fn input_arity(&self) -> usize {
        match self {
            Op::Scan { .. } | Op::SingleRow | Op::CountRows { .. } => 0,
            Op::Filter { .. }
            | Op::Project { .. }
            | Op::Sort { .. }
            | Op::Limit { .. }
            | Op::Distinct
            | Op::Aggregate { .. } => 1,
            Op::Join { .. } | Op::Union { .. } => 2,
        }
    }

    /// 渲染用的名字.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Scan { .. } => "Scan",
            Op::Filter { .. } => "Filter",
            Op::Project { .. } => "Project",
            Op::Aggregate { .. } => "Aggregate",
            Op::CountRows { .. } => "CountRows",
            Op::Join { .. } => "Join",
            Op::Sort { .. } => "Sort",
            Op::Distinct => "Distinct",
            Op::Union { .. } => "Union",
            Op::Limit { .. } => "Limit",
            Op::SingleRow => "SingleRow",
        }
    }
}

/// 计划里的一个节点.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// 算子.
    pub op: Op,
    /// 输入节点的下标.**不变量 I1**:全部严格小于本节点自身的下标.
    pub inputs: Vec<NodeId>,
}

/// 线性计划.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// 扁平节点数组,下标即 [`NodeId`].
    pub nodes: Vec<Node>,
    /// 结果所在节点的下标.
    pub root: NodeId,
}

/// [`Plan::analyze`] 的产物.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// 每个节点的输出 schema,下标即 [`NodeId`].
    pub schemas: Vec<Vec<Column>>,
    /// 每个节点上"程序所面对"的输入 schema.
    ///
    /// `Scan` 是它自己那张表的列,`Join` 是左 ++ 右,`Filter`/`Project`/`Sort`
    /// 是下层节点的 schema,`Limit`/`SingleRow` 没有程序所以是 `None`.
    pub program_inputs: Vec<Option<Vec<Column>>>,
}

impl Analysis {
    /// 某个节点上程序所面对的输入 schema;没有程序则返回空切片.
    pub fn program_input(&self, id: NodeId) -> &[Column] {
        self.program_inputs
            .get(id)
            .and_then(|s| s.as_deref())
            .unwrap_or(&[])
    }
}

impl Plan {
    /// 一次正向扫描,把该检查的全检查了.
    ///
    /// 顺序读一遍 `nodes`(靠 I1,操作数一定在前):
    ///
    /// * 输入个数与算子匹配,输入下标严格小于自身下标;
    /// * 每个程序 [`Prog::verify`] (寄存器先定义后使用,列下标不越界,类型正确);
    /// * 谓词必须是 `BOOL`,排序键与结果列必须是 `INT`;
    /// * 推导出每个节点的输出 schema.
    ///
    /// 之后还会走一遍**子查询校验**(`check_subqueries`):内层计划自己也要过这一关,
    /// 而且 `SubPlan::outers` 必须与"执行时压着的那些外层行"逐层对上--绑定期算错外层
    /// 下标是这里唯一抓得住它的地方([`Prog::verify`] 拿不到符号表).
    pub fn analyze(&self, symbols: &SymbolTable) -> Result<Analysis> {
        let analysis = self.analyze_shallow(symbols)?;
        check_subqueries(self, &analysis, symbols, &[])?;
        Ok(analysis)
    }

    /// 只做"正向扫一遍"的那一半(不递归进子计划).
    ///
    /// 绑定器要"问一个(还没接进任何计划的)计划的 schema"时用它:那时候它还没
    /// 有外层链,深一层的校验也还做不了.
    pub(crate) fn analyze_shallow(&self, symbols: &SymbolTable) -> Result<Analysis> {
        if self.nodes.is_empty() {
            return Err(DbError::IrInvalid {
                node: None,
                message: tr!("ir.plan.001").into_owned(),
            });
        }
        if self.root >= self.nodes.len() {
            return Err(DbError::IrInvalid {
                node: None,
                message: format!("{}", tr!("ir.plan.002", arg0 =  self.root, arg1 =  self.nodes.len())),
            });
        }

        let mut schemas: Vec<Vec<Column>> = Vec::with_capacity(self.nodes.len());
        let mut program_inputs: Vec<Option<Vec<Column>>> = Vec::with_capacity(self.nodes.len());

        for (id, node) in self.nodes.iter().enumerate() {
            let want = node.op.input_arity();
            if node.inputs.len() != want {
                return Err(invalid(id, format!("{}", tr!("ir.plan.003", arg0 = 
                    node.op.name(), arg1 = 
                    want, arg2 = 
                    node.inputs.len()
                ))));
            }
            for &input in &node.inputs {
                if input >= id {
                    return Err(invalid(id, format!("{}", tr!("ir.plan.004", arg0 = 
                        input, arg1 =  id
                    ))));
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

                // 数行数:输出只有一列 `INT`,名字由绑定器给(与 `Aggregate` 那条路
                // 一样取自聚合函数名).表名要认得出,否则 `COUNT(*)` 会把错字瞒过去.
                Op::CountRows { table, name } => {
                    if symbols.columns(table).is_none() {
                        return Err(DbError::UnknownSymbol {
                            name: table.clone(),
                        });
                    }
                    (
                        vec![Column::new(name.clone(), DataType::Int)],
                        None,
                    )
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
                        return Err(invalid(id, tr!("ir.plan.005").into_owned()));
                    }
                    if progs.len() != names.len() {
                        return Err(invalid(id, format!("{}", tr!("ir.plan.006", arg0 = 
                            progs.len(), arg1 = 
                            names.len()
                        ))));
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
                        return Err(invalid(id, format!("{}", tr!("ir.plan.007", arg0 = 
                            names.len(), arg1 = 
                            group_by.len(), arg2 = 
                            aggs.len()
                        ))));
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
                        // `FILTER (WHERE ...)` 是个按输入行求值的谓词
                        if let Some(filter) = &spec.filter {
                            let ty = filter.verify(&input).map_err(|e| at(id, e))?;
                            expect(ty, ScalarType::Bool, id, &tr!("ir.plan.008").into_owned())?;
                        }
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

                    // 左外连接里,没匹配上的行会让**右支所有列**变成 NULL.
                    // 所以右支的 NOT NULL 不能带过来--带过来就会在真正产生
                    // 外连接 NULL 的时候报"列声明了 NOT NULL",把合法结果判成错.
                    if *kind == JoinKind::Left {
                        for column in &mut right {
                            column.not_null = false;
                        }
                    }

                    // 策略的守卫先用**分开的**左/右 schema 校验(键是两侧各一份程序)
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
                        return Err(invalid(id, tr!("ir.plan.009").into_owned()));
                    }
                    for key in keys {
                        let ty = key.prog.verify(&input).map_err(|e| at(id, e))?;
                        // 排序键可以是任何**可存储**类型(文本按字典序,时间戳按
                        // 定宽文本,NUMERIC 按手写的定点比较);只有布尔不行.
                        if matches!(ty, ScalarType::Bool) {
                            return Err(invalid(
                                id,
                                format!("{}", tr!("ir.plan.010", arg0 =  ty)),
                            ));
                        }
                    }
                    (input.clone(), Some(input))
                }

                Op::Limit { .. } => (schemas[node.inputs[0]].clone(), None),

                // 去重不改 schema:它只丢行
                Op::Distinct => (schemas[node.inputs[0]].clone(), None),

                Op::Union { .. } => {
                    let left = schemas[node.inputs[0]].clone();
                    let right = schemas[node.inputs[1]].clone();

                    if left.len() != right.len() {
                        return Err(invalid(id, format!("{}", tr!("ir.plan.011", arg0 = 
                            left.len(), arg1 = 
                            right.len()
                        ))));
                    }
                    for (l, r) in left.iter().zip(right.iter()) {
                        // 同族才敢放在同一列里:`Value::Int(1)` 与
                        // `Value::Numeric("1")` 在值层面不是同一个东西
                        if l.ty.family() != r.ty.family() {
                            return Err(invalid(id, format!("{}", tr!("ir.plan.012", arg0 = 
                                l.name, arg1 = 
                                l.ty, arg2 = 
                                r.ty
                            ))));
                        }
                    }

                    (left, None)
                }            };

            schemas.push(schema);
            program_inputs.push(program_input);
        }

        Ok(Analysis {
            schemas,
            program_inputs,
        })
    }

    /// 这个计划(含子计划)**读到了**每张被扫到的表的哪些列.
    ///
    /// 返回值是 `生效表名 → 用到的列集合`:
    ///
    /// * `Some(集合)` -- 具体读到的那几列(可能是空集:`SELECT 1 FROM t` 一列都没读);
    /// * `None` -- **这张表的全部列**(保守分支:去重,`UNION` 去重,相关子查询那几处
    ///   算不准,就按"整表"算).
    ///
    /// 权限检查用它(`Stmt::Query` 要 `SELECT`):**方向永远是"宁可多要"**--
    /// 少要一次就是一次安静的越权读,多要一次只是多一句"没权限".
    ///
    /// 怎么算:从 root 往回推.`Plan::analyze` 已经给出每个节点的输出 schema 与
    /// 程序输入 schema,`for_each_program` 给出每个节点的程序,`Prog::columns()`
    /// 给出程序引用到的输入列(含子程序).**I1(输入的下标一定更小)**让"从后往前
    /// 扫一趟"就够:轮到某个节点时,所有消费它的节点的需求都已经推进来了.
    pub fn used_columns(
        &self,
        symbols: &SymbolTable,
    ) -> Result<BTreeMap<String, Option<BTreeSet<String>>>> {
        let mut raw: BTreeMap<String, Option<BTreeSet<usize>>> = BTreeMap::new();
        self.collect_used_columns(symbols, &mut raw)?;

        // 列号 → 列名(取不到那张表就按"全部"算:保守那一边)
        let mut named = BTreeMap::new();
        for (table, columns) in raw {
            let names = match (columns, symbols.columns(&table)) {
                (Some(indices), Some(all)) => Some(
                    indices
                        .iter()
                        .filter_map(|index| all.get(*index))
                        .map(|column| column.name.clone())
                        .collect::<BTreeSet<String>>(),
                ),
                _ => None,
            };
            named.insert(table, names);
        }
        Ok(named)
    }

    /// [`Plan::used_columns`] 的递归实现:这一棵计划扫过的表都并进 `out`.
    fn collect_used_columns(
        &self,
        symbols: &SymbolTable,
        out: &mut BTreeMap<String, Option<BTreeSet<usize>>>,
    ) -> Result<()> {
        // **`analyze_shallow`,不是 `analyze`**:子计划是"挂在别人身上"的计划,它的
        // `Step::Outer` 要拿**外层链**才校得动,而外层链是绑定器与 `check_subqueries`
        // 的事.这里只要 schema 与"程序面对哪份输入",浅层那一半就够--用 `analyze`
        // 反而会把一个合法的相关子计划当成"没有外层行"报错.
        let analysis = self.analyze_shallow(symbols)?;
        let mut used: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); self.nodes.len()];
        let mut all = vec![false; self.nodes.len()];
        if !self.nodes.is_empty() {
            all[self.root] = true;
        }

        // 从后往前:轮到某个节点时,消费它的那些节点(下标更大)都已经把需求放进来了
        for id in (0..self.nodes.len()).rev() {
            if all[id] {
                used[id] = (0..analysis.schemas[id].len()).collect();
            }

            // 先把需求算出来,再往输入上推(免得一边借 self.nodes 一边改 used)
            let mut wants: Vec<(NodeId, BTreeSet<usize>)> = Vec::new();
            let mut wants_all: Vec<NodeId> = Vec::new();

            match &self.nodes[id].op {
                Op::Scan {
                    table,
                    filter,
                    index,
                } => {
                    let mut columns = used[id].clone();
                    let mut everything = all[id];
                    if let Some(prog) = filter {
                        columns.extend(prog.columns());
                        everything |= needs_every_column(prog);
                    }
                    if let Some(path) = index {
                        // 索引路径本身要读那一列(哪怕上层没引用它)
                        columns.insert(path.column);
                    }
                    let name = symbols
                        .relation(table)
                        .map(|relation| relation.name().to_string())
                        .unwrap_or_else(|| table.clone());
                    merge_used(out, name, if everything { None } else { Some(columns) });
                }
                Op::Filter { predicate } => {
                    let mut columns = used[id].clone();
                    columns.extend(predicate.columns());
                    if needs_every_column(predicate) {
                        wants_all.push(self.nodes[id].inputs[0]);
                    }
                    wants.push((self.nodes[id].inputs[0], columns));
                }
                Op::Project { progs, .. } => {
                    let mut columns = BTreeSet::new();
                    for index in &used[id] {
                        if let Some(prog) = progs.get(*index) {
                            columns.extend(prog.columns());
                        }
                    }
                    wants.push((self.nodes[id].inputs[0], columns));
                }
                Op::Sort { keys } => {
                    let mut columns = used[id].clone();
                    for key in keys {
                        columns.extend(key.prog.columns());
                    }
                    wants.push((self.nodes[id].inputs[0], columns));
                }
                Op::Limit { .. } => wants.push((self.nodes[id].inputs[0], used[id].clone())),
                // 去重要看整行,所以把输入的全部列都算上
                Op::Distinct => wants_all.push(self.nodes[id].inputs[0]),
                Op::Union { all: keep_all } => {
                    let (left, right) = (self.nodes[id].inputs[0], self.nodes[id].inputs[1]);
                    if *keep_all {
                        wants.push((left, used[id].clone()));
                        wants.push((right, used[id].clone()));
                    } else {
                        // `UNION` 去重:两边都按整行算
                        wants_all.push(left);
                        wants_all.push(right);
                    }
                }
                Op::Aggregate { group_by, aggs, .. } => {
                    let mut columns = BTreeSet::new();
                    for index in &used[id] {
                        match group_by.get(*index) {
                            Some(prog) => columns.extend(prog.columns()),
                            None => {
                                if let Some(spec) = aggs.get(index - group_by.len()) {
                                    if let Some(arg) = &spec.arg {
                                        columns.extend(arg.columns());
                                    }
                                    if let Some(filter) = &spec.filter {
                                        columns.extend(filter.columns());
                                    }
                                }
                            }
                        }
                    }
                    wants.push((self.nodes[id].inputs[0], columns));
                }
                Op::Join { on, strategy, .. } => {
                    let (left, right) = (self.nodes[id].inputs[0], self.nodes[id].inputs[1]);
                    let left_len = analysis.schemas[left].len();

                    // 输出是 `左 ++ 右`:被用到的下标按这个分界劈开
                    let mut left_columns = BTreeSet::new();
                    let mut right_columns = BTreeSet::new();
                    for index in &used[id] {
                        if *index < left_len {
                            left_columns.insert(*index);
                        } else {
                            right_columns.insert(*index - left_len);
                        }
                    }
                    if let Some(prog) = on {
                        // 连接条件面对的是 `左 ++ 右`
                        for index in prog.columns() {
                            if index < left_len {
                                left_columns.insert(index);
                            } else {
                                right_columns.insert(index - left_len);
                            }
                        }
                        if needs_every_column(prog) {
                            wants_all.push(left);
                            wants_all.push(right);
                        }
                    }
                    match strategy {
                        JoinStrategy::NestedLoop => {}
                        JoinStrategy::Hash { keys } => {
                            for key in keys {
                                left_columns.extend(key.outer.columns());
                                right_columns.extend(key.inner.columns());
                            }
                        }
                        JoinStrategy::IndexNestedLoop { key } => {
                            left_columns.extend(key.outer.columns());
                            right_columns.extend(key.inner.columns());
                        }
                    }

                    if all[id] {
                        wants_all.push(left);
                        wants_all.push(right);
                    } else {
                        wants.push((left, left_columns));
                        wants.push((right, right_columns));
                    }
                }
                // `COUNT(*)` **一列都没读**:空集合就是"这张表上至少有一列的权限"
                // (与聚合那条路同一个口径,见 `ir/AGENTS.md`).表名与 `Scan` 一样
                // 解成**生效名**再报,免得权限检查按写下来的名字去问.
                Op::CountRows { table, .. } => {
                    let name = symbols
                        .relation(table)
                        .map(|relation| relation.name().to_string())
                        .unwrap_or_else(|| table.clone());
                    merge_used(out, name, Some(BTreeSet::new()));
                }
                Op::SingleRow => {}
            }

            for target in wants_all {
                all[target] = true;
            }
            for (target, columns) in wants {
                if all[target] {
                    continue;
                }
                used[target].extend(columns);
            }
        }

        // 子计划是**另一棵计划**,各自从头走一遍(外层需求与它无关:它扫的是自己的表)
        for node in &self.nodes {
            let mut subplans = Vec::new();
            for_each_program(&node.op, &mut |prog| {
                subplans.extend(prog.subplans());
            });
            for spec in subplans {
                spec.plan.collect_used_columns(symbols, out)?;
            }
        }

        Ok(())
    }

    /// root 的输出 schema.
    pub fn schema(&self, symbols: &SymbolTable) -> Result<Vec<Column>> {
        let analysis = self.analyze(symbols)?;
        Ok(analysis.schemas[self.root].clone())
    }

    /// 把计划渲染成一棵可读的树(用显式栈,不递归).
    pub fn explain(&self, symbols: &SymbolTable) -> Result<String> {
        let analysis = self.analyze(symbols)?;
        let mut out = String::new();
        let mut stack: Vec<(NodeId, usize)> = vec![(self.root, 0)];

        while let Some((id, depth)) = stack.pop() {
            let node = &self.nodes[id];
            out.push_str(&"  ".repeat(depth));
            out.push_str(&render(id, node, &analysis));
            out.push('\n');
            // 倒着压栈,弹出顺序才与 inputs 的顺序一致
            for &child in node.inputs.iter().rev() {
                stack.push((child, depth + 1));
            }
        }

        Ok(out)
    }
}

/// 增量构建 [`Plan`],顺手挡住坏结构.
///
/// 每个 `push` 都校验输入下标严格小于新节点下标(不变量 I1);`finish` 校验
/// `root` 不越界.输入个数本来就由各方法固定,所以那一条在这里不可能出错.
#[derive(Debug, Default)]
pub struct PlanBuilder {
    nodes: Vec<Node>,
}

impl PlanBuilder {
    /// 空构建器.
    pub fn new() -> Self {
        PlanBuilder { nodes: Vec::new() }
    }

    /// 当前已有多少节点(也就是下一个节点的下标).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// 构建器里还没有节点.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 压入一个节点,返回它的下标.
    pub fn push(&mut self, op: Op, inputs: Vec<NodeId>) -> Result<NodeId> {
        let id = self.nodes.len();
        if inputs.len() != op.input_arity() {
            return Err(invalid(id, format!("{}", tr!("ir.plan.013", arg0 = 
                op.name(), arg1 = 
                op.input_arity(), arg2 = 
                inputs.len()
            ))));
        }
        for &input in &inputs {
            if input >= id {
                return Err(invalid(id, format!("{}", tr!("ir.plan.014", arg0 = 
                    input, arg1 =  id
                ))));
            }
        }
        self.nodes.push(Node { op, inputs });
        Ok(id)
    }

    /// 扫描一张表.
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

    /// 扫描一张表,并把谓词下推到存储.
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

    /// 把谓词**并进**某个 `Scan` 已经有的 `filter`(谓词下推的落点).
    ///
    /// 本来没有 filter 就直接挂上;已经有一个就 `AND` 上去(两个合取项仍然是一个
    /// 程序,扫一遍就够).
    pub fn add_scan_filter(&mut self, id: NodeId, filter: Prog) -> Result<()> {
        match self.nodes.get_mut(id).map(|node| &mut node.op) {
            Some(Op::Scan { filter: slot, .. }) => {
                *slot = Some(match slot.take() {
                    None => filter,
                    Some(existing) => existing.and(filter)?,
                });
                Ok(())
            }
            _ => Err(invalid(id, tr!("ir.plan.015").into_owned())),
        }
    }

    /// 给某个 `Scan` 挂上索引访问路径.
    pub fn set_scan_index(&mut self, id: NodeId, path: IndexPath) -> Result<()> {
        match self.nodes.get_mut(id).map(|node| &mut node.op) {
            Some(Op::Scan { index, .. }) => {
                *index = Some(path);
                Ok(())
            }
            _ => Err(invalid(id, tr!("ir.plan.016").into_owned())),
        }
    }

    /// 看一个已经压进去的节点的算子(绑定器挑连接策略时要看内侧是不是基表 `Scan`).
    pub fn op(&self, id: NodeId) -> Option<&Op> {
        self.nodes.get(id).map(|node| &node.op)
    }

    /// 单行零列.
    pub fn single_row(&mut self) -> NodeId {
        self.nodes.push(Node {
            op: Op::SingleRow,
            inputs: Vec::new(),
        });
        self.nodes.len() - 1
    }

    /// 选择.
    pub fn filter(&mut self, input: NodeId, predicate: Prog) -> Result<NodeId> {
        self.push(Op::Filter { predicate }, vec![input])
    }

    /// 投影.
    pub fn project(
        &mut self,
        input: NodeId,
        progs: Vec<Prog>,
        names: Vec<String>,
    ) -> Result<NodeId> {
        self.push(Op::Project { progs, names }, vec![input])
    }

    /// 连接.
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

    /// 排序.
    pub fn sort(&mut self, input: NodeId, keys: Vec<SortKey>) -> Result<NodeId> {
        self.push(Op::Sort { keys }, vec![input])
    }

    /// `UNION [ALL]`.
    pub fn union(&mut self, left: NodeId, right: NodeId, all: bool) -> Result<NodeId> {
        self.push(Op::Union { all }, vec![left, right])
    }

    /// 把另一个计划(派生表)的节点**接进来**,返回它的 root 在新计划里的下标.
    ///
    /// 派生表就用它:不新增算子,只把内层计划的节点整体搬过来.两点必须对:
    ///
    /// * `inputs` 整体加偏移--它们指向的是**同一个**数组里的更早节点;
    /// * 不变量 I1 仍然成立(偏移后仍然严格小于新下标).
    ///
    /// 程序里的**列**下标不用动:内层程序面对的一直是它自己的 schema.
    pub fn splice(&mut self, plan: Plan) -> Result<NodeId> {
        if plan.nodes.is_empty() {
            return Err(DbError::IrInvalid {
                node: None,
                message: tr!("ir.plan.017").into_owned(),
            });
        }

        let base = self.nodes.len();
        let root = base + plan.root;

        for mut node in plan.nodes {
            node.inputs = node
                .inputs
                .into_iter()
                .map(|input| input + base)
                .collect::<Vec<_>>();
            self.nodes.push(node);
        }

        Ok(root)
    }

    /// 去重.
    pub fn distinct(&mut self, input: NodeId) -> Result<NodeId> {
        self.push(Op::Distinct, vec![input])
    }

    /// 截断.
    pub fn limit(&mut self, input: NodeId, limit: Option<u64>, offset: u64) -> Result<NodeId> {
        self.push(Op::Limit { limit, offset }, vec![input])
    }

    /// 收尾.
    pub fn finish(self, root: NodeId) -> Result<Plan> {
        if self.nodes.is_empty() {
            return Err(DbError::IrInvalid {
                node: None,
                message: tr!("ir.plan.018").into_owned(),
            });
        }
        if root >= self.nodes.len() {
            return Err(DbError::IrInvalid {
                node: None,
                message: format!("{}", tr!("ir.plan.019", arg0 =  root, arg1 =  self.nodes.len())),
            });
        }
        Ok(Plan {
            nodes: self.nodes,
            root,
        })
    }
}

/// 推导一个投影输出列.
///
/// * **原样透传的列引用**(program 恰好只有一条 `Col(i)` 指令)→ 保留那一列自己
///   的类型,**含 typmod**.所以 `SELECT name FROM t` 出来的 `name` 还是
///   `VARCHAR(30)`,而不是退化成 `TEXT`.
/// * 计算出来的表达式 → 退化成它大类的默认类型.
///
/// `lower` 里给 `ORDER BY` 建输出作用域时也用它,两处必须一致.
pub fn project_column(prog: &Prog, name: &str, input: &[Column]) -> Result<Column> {
    let ty = prog.verify(input)?;
    let family = match ty {
        ScalarType::Value(family) => family,
        // 无类型 NULL 投影出来就是 TEXT(与 `SELECT NULL` 在 PostgreSQL 里一致)
        ScalarType::Null => TypeFamily::Text,
        ScalarType::Bool => {
            return Err(DbError::IrInvalid {
                node: None,
                message: format!("{}", tr!("ir.plan.020", arg0 =  name, arg1 =  ty)),
            })
        }
    };

    let data_type = match passthrough_column(prog) {
        // 透传列:保留原类型(也就是保留 typmod)
        Some(index) => input
            .get(index)
            .map(|column| column.ty)
            .unwrap_or_else(|| family.default_type()),
        None => family.default_type(),
    };

    Ok(Column::new(name, data_type))
}

/// 标量类型 → 一个具体的默认类型(用来给计算出来的列贴标签).
fn scalar_default_type(ty: ScalarType, node: NodeId, what: &str) -> Result<DataType> {
    match ty {
        ScalarType::Value(family) => Ok(family.default_type()),
        ScalarType::Null => Ok(TypeFamily::Text.default_type()),
        ScalarType::Bool => Err(invalid(node, format!("{}", tr!("ir.plan.021", arg0 =  what)))),
    }
}

/// 这个程序是不是"原样取第 i 列".
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

// ------------------------------------------------------------ 子查询的校验

/// 把一张表的需求并进总表:`None`(全部列)压过具体集合.
fn merge_used(
    out: &mut BTreeMap<String, Option<BTreeSet<usize>>>,
    table: String,
    columns: Option<BTreeSet<usize>>,
) {
    let slot = out.entry(table).or_insert_with(|| Some(BTreeSet::new()));
    match (&mut *slot, columns) {
        (slot @ None, _) => *slot = None,
        (slot, None) => *slot = None,
        (Some(held), Some(columns)) => held.extend(columns),
    }
}

/// 这个程序(含子程序)里有**相关**子查询吗?有就得把这一层的列全算上.
///
/// 相关子查询读外层行走的是 `Step::Outer`,而 `Prog::columns()` 看不到它
/// (它只数"本程序引用的输入列")--所以这里保守一次:整层都算读到.
fn needs_every_column(prog: &Prog) -> bool {
    prog.subplans().iter().any(|spec| spec.correlated)
}

/// 把一个 [`Op`] 里**所有** [`Prog`] 交给 `visit`.
///
/// 穷尽 match:加新算子时会编译报错,不会漏掉某个程序的子查询校验.
fn for_each_program<'a>(op: &'a Op, visit: &mut dyn FnMut(&'a Prog)) {
    match op {
        Op::SingleRow | Op::Distinct | Op::Union { .. } | Op::CountRows { .. } => {}
        Op::Scan { filter, index, .. } => {
            if let Some(prog) = filter {
                visit(prog);
            }
            if let Some(path) = index {
                path.keys.iter().for_each(&mut *visit);
            }
        }
        Op::Filter { predicate } => visit(predicate),
        Op::Project { progs, .. } => progs.iter().for_each(&mut *visit),
        Op::Join { on, strategy, .. } => {
            if let Some(prog) = on {
                visit(prog);
            }
            match strategy {
                JoinStrategy::NestedLoop => {}
                JoinStrategy::Hash { keys } => keys.iter().for_each(|key| {
                    visit(&key.outer);
                    visit(&key.inner);
                }),
                JoinStrategy::IndexNestedLoop { key } => {
                    visit(&key.outer);
                    visit(&key.inner);
                }
            }
        }
        Op::Aggregate { group_by, aggs, .. } => {
            group_by.iter().for_each(&mut *visit);
            for spec in aggs {
                if let Some(arg) = &spec.arg {
                    visit(arg);
                }
                if let Some(filter) = &spec.filter {
                    visit(filter);
                }
            }
        }
        Op::Sort { keys } => keys.iter().for_each(|key| visit(&key.prog)),
        Op::Limit { .. } => {}
    }
}

/// 这个计划里(含嵌套子计划)出现过外层引用吗.
///
/// 绑定器用它填 `SubPlan::correlated`.判断是**保守**的:只要哪里有
/// `Step::Outer` 就算相关--保守只会少缓存,不会算错.
pub(crate) fn plan_has_outer(plan: &Plan) -> bool {
    plan.nodes.iter().any(|node| {
        let mut found = false;
        for_each_program(&node.op, &mut |prog| {
            if !found {
                found = program_has_outer(prog);
            }
        });
        found
    })
}

/// 一个程序(含它的子程序)里出现过外层引用吗.
fn program_has_outer(prog: &Prog) -> bool {
    prog.steps().iter().any(|step| match step {
        Step::Outer { .. } => true,
        Step::Subquery { spec, .. } => plan_has_outer(&spec.plan),
        Step::Case {
            then_prog,
            else_prog,
            ..
        } => program_has_outer(then_prog) || program_has_outer(else_prog),
        Step::And { right_prog, .. } | Step::Or { right_prog, .. } => program_has_outer(right_prog),
        // `COALESCE` 的每个参数都是自足程序,也要走进去
        Step::Coalesce { args, .. } => args.iter().any(program_has_outer),
        _ => false,
    })
}

/// 校验一个计划里所有程序的子查询与外层引用.
///
/// `chain` 是这个计划的节点**执行时压着的那些外层行**的 schema(`chain[0]` 是最贴近
/// 的一层);最外层传空.
///
/// 两条规矩:
///
/// * `Step::Outer { depth, index }` 要能在 `chain[depth - 1]` 上取到那一列,且类型与
///   step 上记的一致--取不到就说明外层链对不上,直接报 `IrInvalid`;
/// * `SubPlan::outers` 必须等于 `[所在节点程序面对的那一行] ++ chain`(**绑定期**算
///   出来的链与**执行期**压着的帧必须一样).子查询没有记录外层链(`outers` 为空,
///   例如分组查询里的子查询,见 `lower`)时,内层就不能有任何外层引用--递归下去时
///   `chain` 为空,`Step::Outer` 自然会报错.
fn check_subqueries(
    plan: &Plan,
    analysis: &Analysis,
    symbols: &SymbolTable,
    chain: &[Vec<Column>],
) -> Result<()> {
    for (id, node) in plan.nodes.iter().enumerate() {
        let input = match analysis.program_inputs.get(id).and_then(|i| i.as_deref()) {
            Some(input) => input,
            None => continue,
        };

        let mut failure: Option<DbError> = None;
        for_each_program(&node.op, &mut |prog| {
            if failure.is_some() {
                return;
            }
            if let Err(e) = check_program(prog, symbols, input, chain, id) {
                failure = Some(e);
            }
        });

        if let Some(e) = failure {
            return Err(e);
        }
    }

    Ok(())
}

/// 校验一个程序里的子查询 step 与外层引用(见 `check_subqueries`).
///
/// 子程序(`CASE` 的分支,`AND`/`OR` 的右侧)与外层面对**同一行**,所以递归下去时
/// `input` 与 `chain` 原样传--但**必须递归**:子查询与外层引用一样可能藏在里面.
fn check_program(
    prog: &Prog,
    symbols: &SymbolTable,
    input: &[Column],
    chain: &[Vec<Column>],
    id: NodeId,
) -> Result<()> {
    for step in prog.steps() {
        match step {
            Step::Subquery { spec, .. } => {
                // 内层计划自己也要过一遍浅层校验,然后带着**它自己的外层链**
                // 递归下去(`spec.outers` 就是内层执行时压着的那些帧).
                let inner = spec.plan.analyze_shallow(symbols).map_err(|e| at(id, e))?;
                let root = &inner.schemas[spec.plan.root];

                if spec.arity != root.len() {
                    return Err(invalid(
                        id,
                        format!("{}", tr!("ir.plan.022", arg0 = 
                            spec.arity, arg1 = 
                            root.len()
                        )),
                    ));
                }
                if matches!(spec.mode, SubMode::Scalar | SubMode::Quantified { .. }) {
                    if root.len() != 1 {
                        return Err(invalid(
                            id,
                            format!("{}", tr!("ir.plan.023", arg0 =  root.len())),
                        ));
                    }
                    let actual = ScalarType::Value(root[0].ty.family());
                    if actual != spec.inner {
                        return Err(invalid(
                            id,
                            format!("{}", tr!("ir.plan.024", arg0 = 
                                spec.inner, arg1 =  actual
                            )),
                        ));
                    }
                }

                if !spec.outers.is_empty() {
                    let expected: Vec<Vec<Column>> = std::iter::once(input.to_vec())
                        .chain(chain.iter().cloned())
                        .collect();
                    if spec.outers != expected {
                        return Err(invalid(
                            id,
                            tr!("ir.plan.025").into_owned()
                                .to_string(),
                        ));
                    }
                }

                check_subqueries(&spec.plan, &inner, symbols, &spec.outers).map_err(|e| at(id, e))?;
            }
            Step::Outer {
                depth, index, ty, ..
            } => {
                let Some(level) = depth.checked_sub(1).and_then(|d| chain.get(d as usize)) else {
                    return Err(invalid(
                        id,
                        format!("{}", tr!("ir.plan.026", arg0 = 
                            depth, arg1 = 
                            chain.len()
                        )),
                    ));
                };
                let column = level.get(*index).ok_or_else(|| {
                    invalid(
                        id,
                        format!("{}", tr!("ir.plan.027", arg0 = 
                            depth, arg1 = 
                            index, arg2 = 
                            level.len()
                        )),
                    )
                })?;
                let actual = ScalarType::Value(column.ty.family());
                if actual != *ty {
                    return Err(invalid(
                        id,
                        format!("{}", tr!("ir.plan.028", arg0 = 
                            depth, arg1 =  index, arg2 =  ty, arg3 =  column.ty
                        )),
                    ));
                }
            }
            // 子程序面对同一个输入行,所以 `input` 与 `chain` 原样传下去
            Step::Case {
                then_prog,
                else_prog,
                ..
            } => {
                check_program(then_prog, symbols, input, chain, id)?;
                check_program(else_prog, symbols, input, chain, id)?;
            }
            Step::And { right_prog, .. } | Step::Or { right_prog, .. } => {
                check_program(right_prog, symbols, input, chain, id)?;
            }
            // `COALESCE` 的每个参数都是自足程序,面对的是同一个输入行
            Step::Coalesce { args, .. } => {
                for arg in args {
                    check_program(arg, symbols, input, chain, id)?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// 校验一条索引访问路径.
///
/// 两条检查都是为了**不安静地漏行**:
///
/// * 键不能读列--索引是访问路径而不是算子,键在没有输入行的情况下求值;
/// * 键与索引列必须**同一大类**--`Value::Int(1)` 与 `Value::Numeric("1")` 在值
///   层面不是同一个键,混着用会漏掉本该匹配的行.
///
/// 一组键(TODO 3.14(a) 的 `IN` 列表)**每个都要过**这两关:少查一个键就是少一批
/// 行,而那是安静的错答案.
fn verify_index_path(path: &IndexPath, table: &str, columns: &[Column], id: NodeId) -> Result<()> {
    let column = columns.get(path.column).ok_or_else(|| {
        invalid(
            id,
            format!("{}", tr!("ir.plan.029", arg0 = 
                path.column, arg1 = 
                table, arg2 = 
                columns.len()
            )),
        )
    })?;

    // **外层键**只探测一个值(相关子查询的一行一次):多出来的一律算降级坏了.
    if path.outer && path.keys.len() != 1 {
        return Err(invalid(id, tr!("ir.plan.030").into_owned()));
    }

    for key in &path.keys {
        let ty = key.verify(columns).map_err(|e| at(id, e))?;
        // **常量键**不读列:它在没有输入行的前提下求值(见 [`IndexPath::keys`]).
        //
        // **外层键**例外(TODO 8.7):降级时(`opt::pushdown` 的 `lower_outer_key`)它被
        // 重排成"面对**这张表的行**"的程序--外层那一层的列被搬到这张表里**同名**的列
        // 上,执行时 `Scan` 喂进去的正是外层行(`index_keys` 传的就是外层行栈顶那一行).
        // 于是它**恰好**只读索引那一列;别的列一律算降级坏了.
        match path.outer {
            true => {
                let key_columns = key.columns();
                if key_columns.len() != 1 || !key_columns.contains(&path.column) {
                    return Err(invalid(id, tr!("ir.plan.030").into_owned()));
                }
            }
            false => {
                if !key.columns().is_empty() {
                    return Err(invalid(id, tr!("ir.plan.030").into_owned()));
                }
            }
        }

        match ty {
            ScalarType::Value(family) if family == column.ty.family() => {}
            // 无类型的 `NULL` 键:那是个**真**的键(索引里 NULL 也占一个桶),查它
            // 只是多读几行,读出来的行由谓词自己复核掉(`x IN (1, NULL)` 碰上 NULL
            // 是 UNKNOWN,不是 TRUE).等值那种键不会走到这里--降级不认 `x = NULL`.
            ScalarType::Null => {}
            other => {
                return Err(invalid(
                    id,
                    format!("{}", tr!("ir.plan.031", arg0 = 
                        other, arg1 =  column.name, arg2 =  column.ty
                    )),
                ))
            }
        }
    }

    Ok(())
}

/// 校验一个等值键:两侧各自面对自己那一支的 schema,且**同一大类**.
fn verify_eq_key(key: &EqKey, left: &[Column], right: &[Column], id: NodeId, what: &str) -> Result<()> {
    let outer = key.outer.verify(left).map_err(|e| at(id, e))?;
    let inner = key.inner.verify(right).map_err(|e| at(id, e))?;

    match (outer, inner) {
        (ScalarType::Value(a), ScalarType::Value(b)) if a == b => Ok(()),
        _ => Err(invalid(
            id,
            format!("{}", tr!("ir.plan.032", arg0 = 
                what, arg1 =  outer, arg2 =  inner
            )),
        )),
    }
}

/// 校验连接策略:与 `ON` 配不配套,键同不同族,内侧能不能建索引.
///
/// 这些都为了让**手写的不一致计划**直接报错,而不是跑出一个奇怪的结果.
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
                return Err(invalid(id, tr!("ir.plan.033").into_owned()));
            }
            if keys.is_empty() {
                return Err(invalid(id, tr!("ir.plan.034").into_owned()));
            }
            for key in keys {
                verify_eq_key(key, left, right, id, &tr!("ir.plan.035").into_owned())?;
            }
            Ok(())
        }

        JoinStrategy::IndexNestedLoop { key } => {
            if on.is_none() {
                return Err(invalid(id, tr!("ir.plan.036").into_owned()));
            }

            let table = match inner {
                Op::Scan { table, .. } => table,
                _ => {
                    return Err(invalid(
                        id,
                        tr!("ir.plan.037").into_owned(),
                    ))
                }
            };

            let index_column = passthrough_column(&key.inner).ok_or_else(|| {
                invalid(
                    id,
                    tr!("ir.plan.038").into_owned(),
                )
            })?;

            let columns = symbols.columns(table).ok_or_else(|| DbError::UnknownSymbol {
                name: table.clone(),
            })?;
            let column = columns.get(index_column).ok_or_else(|| {
                invalid(
                    id,
                    format!("{}", tr!("ir.plan.039", arg0 =  index_column, arg1 =  table, arg2 =  columns.len())),
                )
            })?;

            let outer = key.outer.verify(left).map_err(|e| at(id, e))?;
            match outer {
                ScalarType::Value(family) if family == column.ty.family() => Ok(()),
                other => Err(invalid(
                    id,
                    format!("{}", tr!("ir.plan.040", arg0 = 
                        other, arg1 =  table, arg2 =  column.name, arg3 =  column.ty
                    )),
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
            format!("{}", tr!("ir.plan.041", arg0 =  what, arg1 =  want, arg2 =  actual)),
        ))
    }
}

fn render(id: NodeId, node: &Node, analysis: &Analysis) -> String {
    let input = analysis.program_input(id);
    match &node.op {
        Op::SingleRow => "SingleRow".to_string(),
        Op::CountRows { table, .. } => format!("CountRows({})", table),
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
                .map(|spec| {
                    let base = match (&spec.arg, spec.distinct) {
                        (None, _) => format!("{}(*)", spec.func.name()),
                        (Some(prog), true) => {
                            format!("{}(DISTINCT {})", spec.func.name(), prog.explain(input))
                        }
                        (Some(prog), false) => {
                            format!("{}({})", spec.func.name(), prog.explain(input))
                        }
                    };
                    match &spec.filter {
                        Some(filter) => {
                            format!("{} FILTER (WHERE {})", base, filter.explain(input))
                        }
                        None => base,
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
        Op::Distinct => "Distinct".to_string(),
        Op::Union { all } => format!("Union(all={})", all),
    }
}
