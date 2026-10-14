//! 线性表达式 IR：一个小寄存器机。
//!
//! 表达式在这里**不是树**，而是一串 [`Step`]：每个 step 写一个寄存器，后面的
//! step 引用前面写好的寄存器。于是求值就是一个 `for` 循环配一个寄存器数组，
//! **没有任何递归**。
//!
//! 两条不变量由 [`Prog::verify`] 静态保证：
//!
//! * **先定义后使用**：任何 step 读到的寄存器，必须在此前被写过；
//! * **结果已定义**：[`Prog::result`] 指向的寄存器必须被写过。
//!
//! 手写 IR 时用 [`ProgBuilder`]，它会自动分配寄存器，省掉手数寄存器号这个
//! 易错活。
//!
//! # 子查询在表达式层长什么样
//!
//! 表达式里可以有子查询（[`Step::Subquery`]，内层是一份自足的 [`SubPlan`]）与
//! **外层引用**（[`Step::Outer`]）。它们需要一点求值环境，所以求值分两个入口：
//!
//! * [`Prog::eval`] / [`Prog::eval_split`]：**没有环境**（[`NullEnv`]），只用于
//!   纯程序（INSERT 的值、连接的键、下推的谓词……）；
//! * [`Prog::eval_with`]：把 `&dyn Env` 带进去，实现见 [`crate::ir::exec`] 的
//!   `ExecCtx`（外层行栈 + 子查询缓存）。
//!
//! [`Step::Outer`] 的类型直接写在 step 上（`verify` 看不到外层链），由
//! [`crate::ir::Plan::analyze`] 回过头核对——见那里的 `check_subqueries`。
//!
//! 寄存器里装的东西（[`Scalar`]）、真值（[`Tri`]）与类型标签（[`ScalarType`]）
//! 不在本文件，见 [`crate::ir::value`]。

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::db::table::{Column, DataType, DecimalOp, DbError, Result, TypeFamily, Value};

use super::plan::Plan;
use super::value::{Scalar, ScalarType, Tri};

/// 寄存器号。一个程序用到的寄存器是 `0..nregs`。
pub type Reg = u16;

/// 二元运算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// 加
    Add,
    /// 减
    Sub,
    /// 乘
    Mul,
    /// 除
    Div,
    /// 等于
    Eq,
    /// 不等于
    Ne,
    /// 小于
    Lt,
    /// 小于等于
    Le,
    /// 大于
    Gt,
    /// 大于等于
    Ge,
    /// 逻辑与
    And,
    /// 逻辑或
    Or,
    /// `LIKE`（语法树里写作 `~~`）
    Like,
    /// `NOT LIKE`（`!~~`）
    NotLike,
    /// `ILIKE`（`~~*`）
    ILike,
    /// `NOT ILIKE`（`!~~*`）
    NotILike,
    /// `~`：POSIX 正则匹配，区分大小写。
    Regex,
    /// `!~`：`~` 取反。
    NotRegex,
    /// `~*`：POSIX 正则匹配，忽略大小写。
    IRegex,
    /// `!~*`：`~*` 取反。
    NotIRegex,
}

impl BinOp {
    /// 渲染用的符号。
    pub fn symbol(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "AND",
            BinOp::Or => "OR",
            BinOp::Like => "LIKE",
            BinOp::NotLike => "NOT LIKE",
            BinOp::ILike => "ILIKE",
            BinOp::NotILike => "NOT ILIKE",
            BinOp::Regex => "~",
            BinOp::NotRegex => "!~",
            BinOp::IRegex => "~*",
            BinOp::NotIRegex => "!~*",
        }
    }

    /// 是不是 `LIKE` 那一族（`LIKE` / `ILIKE` 及其取反）。
    ///
    /// 它们**不是**比较运算：操作数与结果都是文本→布尔的另一种规则，
    /// 求值走 `like_match`，类型检查走 [`pattern_type`]。
    pub fn is_like(&self) -> bool {
        matches!(
            self,
            BinOp::Like | BinOp::NotLike | BinOp::ILike | BinOp::NotILike
        )
    }

    /// 是不是正则匹配那一族（`~` / `~*` 及其取反）。
    pub fn is_regex(&self) -> bool {
        matches!(
            self,
            BinOp::Regex | BinOp::NotRegex | BinOp::IRegex | BinOp::NotIRegex
        )
    }

    /// 是不是「模式匹配」类运算（`LIKE` 族 + 正则族）。
    pub fn is_pattern(&self) -> bool {
        self.is_like() || self.is_regex()
    }

    /// 这一族里「要不要取反」。
    fn negated(&self) -> bool {
        matches!(
            self,
            BinOp::NotLike | BinOp::NotILike | BinOp::NotRegex | BinOp::NotIRegex
        )
    }

    /// 这一族里「是不是忽略大小写」。
    fn case_insensitive(&self) -> bool {
        matches!(
            self,
            BinOp::ILike | BinOp::NotILike | BinOp::IRegex | BinOp::NotIRegex
        )
    }

    /// 是否是逻辑运算（`AND` / `OR`）。
    pub fn is_logical(&self) -> bool {
        matches!(self, BinOp::And | BinOp::Or)
    }

    /// 是否是比较运算。
    pub fn is_comparison(&self) -> bool {
        matches!(
            self,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
    }

    /// 是否是算术运算。
    pub fn is_arithmetic(&self) -> bool {
        matches!(self, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div)
    }
}

/// 标量函数。要加新函数就在 `apply_call` 与 `call_type` 里各加一个分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    /// `ROUND(numeric, digits)`。
    Round,
    /// `UPPER(text)`。
    Upper,
    /// `LEFT(text, n)`（按**字符**数，`n` 为负时去掉右边 `|n|` 个字符）。
    Left,
    /// `CONCAT_WS(sep, ...)`：变参，跳过 NULL 参数、分隔符为 NULL 时结果是 NULL。
    ConcatWs,
    /// `EXTRACT(YEAR FROM ts)`：本版只认 `YEAR`，见 [`crate::ir::irgen`]。
    ///
    /// 值模型里 TIMESTAMP 是定宽规范文本（`YYYY-MM-DD HH:MM:SS.ffffff`），
    /// DATE 是定宽规范文本（`YYYY-MM-DD`），所以取年份就是取前 4 位——
    /// 不需要任何日期运算。
    ExtractYear,
    /// `LENGTH(text)`：**按字符**数算（与 `VARCHAR` 的长度口径一致）。
    Length,
}

impl ScalarFunc {
    /// 渲染用的名字。
    pub fn name(&self) -> &'static str {
        match self {
            ScalarFunc::Round => "ROUND",
            ScalarFunc::Upper => "UPPER",
            ScalarFunc::Left => "LEFT",
            ScalarFunc::ConcatWs => "CONCAT_WS",
            ScalarFunc::ExtractYear => "EXTRACT",
            ScalarFunc::Length => "LENGTH",
        }
    }

    /// 需要几个参数；`None` 表示**变参**（具体个数由 `call_type` 判）。
    pub fn arity(&self) -> Option<usize> {
        match self {
            ScalarFunc::Round | ScalarFunc::Left => Some(2),
            ScalarFunc::Upper | ScalarFunc::ExtractYear | ScalarFunc::Length => Some(1),
            ScalarFunc::ConcatWs => None,
        }
    }
}

/// 一元运算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// 取负
    Neg,
    /// 逻辑非
    Not,
}

impl UnOp {
    /// 渲染用的符号。
    pub fn symbol(&self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Not => "NOT",
        }
    }
}

/// 一条指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `regs[dst] = 输入行的第 index 列`
    Col {
        /// 目的寄存器。
        dst: Reg,
        /// 列在**本算子输入 schema** 里的下标。
        index: usize,
    },
    /// `regs[dst] = 常量`
    Const {
        /// 目的寄存器。
        dst: Reg,
        /// 常量。
        value: Scalar,
    },
    /// `regs[dst] = op regs[src]`
    Unary {
        /// 目的寄存器。
        dst: Reg,
        /// 运算。
        op: UnOp,
        /// 源寄存器。
        src: Reg,
    },
    /// `regs[dst] = regs[lhs] op regs[rhs]`
    Binary {
        /// 目的寄存器。
        dst: Reg,
        /// 运算。
        op: BinOp,
        /// 左操作数。
        lhs: Reg,
        /// 右操作数。
        rhs: Reg,
    },
    /// `regs[dst] = if regs[cond] is TRUE { then_prog } else { else_prog }`。
    ///
    /// 走 SQL 的 `CASE` 语义：条件**不是 TRUE**（含 `Unknown`）就取 else 分支。
    ///
    /// **两个分支是自足的子程序**（自己的寄存器空间），只有被选中的那一个才会
    /// 求值——这正是 SQL 标准给 `CASE` 保证的惰性：`CASE WHEN 1 = 0 THEN 1/0
    /// ELSE 42 END` 是 42，不是"除以零"。代价是求值多了一层**有界递归**
    /// （深度 = 表达式嵌套深度，而降级阶段本来就在这个嵌套上递归）。
    Case {
        /// 目的寄存器。
        dst: Reg,
        /// 条件寄存器（真值）。
        cond: Reg,
        /// 条件为 TRUE 时求值的子程序。
        then_prog: Prog,
        /// 否则求值的子程序。
        else_prog: Prog,
        /// 统一后的类型是 `NUMERIC`：取到 `INT` 时要**真的转成**它。
        ///
        /// `INT` 与 `NUMERIC` 是唯一允许的隐式放大（见 `unify_branches`），
        /// 而放行类型之后运行时也得真转，否则物化时会以 `TypeMismatch` 报错。
        /// 这个标志由**降级**算出来（它要拿两个分支的类型），[`Prog::verify`]
        /// 强制它与 `unify_branches` 的结果一致——不一致就报 `IrInvalid`，
        /// 宁可拒绝，也不让 `Int` 悄悄落进 `NUMERIC` 列。
        widen: bool,
    },
    /// `regs[dst] = regs[left] AND right_prog`，**左侧不是 FALSE 才算右侧**。
    ///
    /// Kleene 三值逻辑下与"两侧都算完再 AND"完全等价（`FALSE AND 任何` = FALSE），
    /// 差别只在**右侧会不会被求值**：`WHERE false AND (1/0 = 1)` 不再报除零。
    And {
        /// 目的寄存器。
        dst: Reg,
        /// 左侧（本程序里的寄存器）。
        left: Reg,
        /// 左侧不是 FALSE 时才求值的右侧子程序。
        right_prog: Prog,
    },
    /// `regs[dst] = regs[left] OR right_prog`，**左侧不是 TRUE 才算右侧**。
    Or {
        /// 目的寄存器。
        dst: Reg,
        /// 左侧（本程序里的寄存器）。
        left: Reg,
        /// 左侧不是 TRUE 时才求值的右侧子程序。
        right_prog: Prog,
    },
    /// 标量函数调用。
    Call {
        /// 目的寄存器。
        dst: Reg,
        /// 调用哪个函数。
        func: ScalarFunc,
        /// 参数寄存器。
        args: Vec<Reg>,
    },
    /// 类型转换（`x::numeric`）。
    ///
    /// 只放行**值模型里真有意义**的几组转换，见 `cast_type`：同族恒等、
    /// `INT → NUMERIC`、`NULL → 任意`。别的（`int::text` 之类）在验证期就拒绝。
    Cast {
        /// 目的寄存器。
        dst: Reg,
        /// 源寄存器。
        src: Reg,
        /// 目标类型。
        to: DataType,
    },
    /// 求值一个**内层查询**（子查询），结果是一列里的一个值 / 一个存在性 / 一次
    /// 量化的比较。
    ///
    /// 内层计划是**自足**的（自己的 `nodes`/`root`，见 [`SubPlan`]），执行时按当前
    /// 外层行新建一份源——这正是拉取式执行模型让"相关子查询"变成自然事的地方：
    /// 内层计划本来就是一个 `RowSource`。
    Subquery {
        /// 目的寄存器。
        dst: Reg,
        /// 内层计划与它的模式。
        spec: Box<SubPlan>,
    },
    /// 读**外层**查询的某一列（相关子查询的绑定）。
    ///
    /// `depth` 是隔了几层（`1` 是直接外层），`index` 是那一层**扁平 schema** 里的
    /// 下标——与 [`Prog::verify`] 输入的那个下标是同一套。类型直接写在 step 上
    /// （`verify` 看不到外层链），由 [`crate::ir::Plan::analyze`] 回过头核对。
    Outer {
        /// 目的寄存器。
        dst: Reg,
        /// 隔了几层，从 1 开始。
        depth: u16,
        /// 外层扁平 schema 里的列下标。
        index: usize,
        /// 这一列的类型（分析期核对）。
        ty: ScalarType,
    },
    /// `regs[dst] = (regs[src] IS [NOT] NULL)`。
    ///
    /// 它**永远是确定的**真或假，不会给 `Unknown`——这正是 `IS NULL` 存在的理由。
    IsNull {
        /// 目的寄存器。
        dst: Reg,
        /// 源寄存器。
        src: Reg,
        /// 是 `IS NOT NULL` 吗。
        negated: bool,
    },
}

/// 一个内层查询（子查询）。
///
/// 它**不是**外层计划的一部分：`plan` 有自己的节点数组与 root，所以"为外层每一行
/// 重新建一份源"只是多调一次构建。内层对外的引用走 [`Step::Outer`]，`outers` 就是
/// 那些引用面对的 schema 链。
///
/// 校验分两处（因为 [`Prog::verify`] 拿不到符号表）：`verify` 只看 step 自己的类型，
/// 深一层的一致性（内层计划本身、`outers[0]` 与所在节点的输入 schema）由
/// [`crate::ir::Plan::analyze`] 递归检查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubPlan {
    /// 内层计划。
    pub plan: Plan,
    /// 求值方式。
    pub mode: SubMode,
    /// 内层结果列的类型（`Exists` 模式下无意义）。
    pub inner: ScalarType,
    /// 内层的输出列数（`Scalar` / `Quantified` 必须恰好 1）。
    pub arity: usize,
    /// 外层 schema 链：`outers[0]` 是直接外层，`outers[1]` 再往外一层……
    /// 未相关时为空。
    pub outers: Vec<Vec<Column>>,
    /// 内层（含嵌套子计划）里出现过 [`Step::Outer`] 吗。
    ///
    /// 只是**缓存提示**：相关就不能缓存结果。判断是保守的（出现过就算），
    /// 保守只会少缓存，不会算错。
    pub correlated: bool,
}

/// 子查询的求值方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubMode {
    /// 标量子查询：内层必须只有一列。0 行 → `NULL`；多于 1 行 → 运行期错误。
    Scalar,
    /// `EXISTS (…)`：内层有没有行。
    Exists,
    /// `x op ANY (…)` / `x op ALL (…)`。
    ///
    /// `IN (子查询)` 就是 `op = Eq, all = false`；空集的语义与 SQL 标准一致
    /// （`ALL` → 真、`ANY` → 假）。
    Quantified {
        /// 要比较的外层表达式；按**当前行**求值。
        test: Prog,
        /// 比较运算符。
        op: BinOp,
        /// 是 `ALL` 吗（否则 `ANY`）。
        all: bool,
    },
}

/// 求值一个 [`Prog`] 时需要的**环境**。
///
/// 只有在程序里出现 [`Step::Outer`] 或 [`Step::Subquery`] 时才用得上——普通的
/// 程序走 [`Prog::eval`]，那里的环境是 [`NullEnv`]。
///
/// 实现见 [`crate::ir::exec`] 的 `ExecCtx`（它持有外层行栈与子查询结果缓存）。
pub trait Env {
    /// 取第 `depth` 层外层的第 `index` 列。
    fn outer(&self, depth: u16, index: usize) -> std::result::Result<Scalar, EvalError>;

    /// 求值一个内层子查询；`row` 是本程序面对的那一行，也就是**内层的外层行**。
    fn subquery(&self, spec: &SubPlan, row: &[Value]) -> std::result::Result<Scalar, EvalError>;
}

/// 没有环境的求值：遇到 [`Step::Outer`] / [`Step::Subquery`] 就报错。
///
/// [`Prog::eval`] 与 [`Prog::eval_split`] 用它，于是那些"只有普通 step"的程序
/// （INSERT 的值、连接的键、下推的谓词……）保持原来的签名与行为。
pub struct NullEnv;

impl Env for NullEnv {
    fn outer(&self, _depth: u16, _index: usize) -> std::result::Result<Scalar, EvalError> {
        Err(EvalError::NoEnv {
            what: "外层引用",
        })
    }

    fn subquery(&self, _spec: &SubPlan, _row: &[Value]) -> std::result::Result<Scalar, EvalError> {
        Err(EvalError::NoEnv { what: "子查询" })
    }
}

/// 求值期的失败。
///
/// [`Prog`] 并不知道自己在计划里的节点下标，所以这里只描述原因；由
/// [`crate::ir::exec`] 补上节点下标，转成 [`DbError::IrRuntime`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// 列下标越界。
    ColumnOutOfRange {
        /// 请求的下标。
        index: usize,
        /// 本行实际有的列数。
        arity: usize,
    },
    /// 寄存器下标越界。
    RegisterOutOfRange {
        /// 请求的寄存器。
        reg: Reg,
        /// 程序声明的寄存器数。
        nregs: Reg,
    },
    /// 期望整数却拿到别的类型（算术只对 INT）。
    NotAnInt,
    /// 两侧类型不属于同一大类，没法比较。
    MixedTypes,
    /// 十进制运算或函数调用失败（溢出、除零、参数不对）。
    Decimal {
        /// 具体原因。
        reason: String,
    },
    /// 期望布尔却拿到整数。
    NotABool,
    /// 除以零。
    DivisionByZero,
    /// 整数溢出。
    Overflow {
        /// 溢出的运算符号。
        op: &'static str,
    },
    /// 正则表达式不合法（`~` / `~*`）。
    BadPattern {
        /// 正则编译器的说法。
        reason: String,
    },
    /// 日期 / 时间戳文本看不懂（`EXTRACT` 取年份）。
    BadDate {
        /// 出问题的文本。
        value: String,
    },
    /// 需要求值环境却没给（[`Prog::eval`] 遇到了 [`Step::Outer`] / [`Step::Subquery`]）。
    NoEnv {
        /// 需要什么。
        what: &'static str,
    },
    /// 内层子查询失败：**原样**带上它的错误（不走 `IrRuntime` 拍扁）。
    Nested(Box<DbError>),
    /// 标量子查询返回了多行——SQL 要求最多一行。
    ScalarSubqueryRows {
        /// 实际行数。
        rows: usize,
    },
}

impl EvalError {
    /// 人类可读的原因。
    pub fn message(&self) -> String {
        match self {
            EvalError::ColumnOutOfRange { index, arity } => {
                format!("列下标 {} 越界（本行只有 {} 列）", index, arity)
            }
            EvalError::RegisterOutOfRange { reg, nregs } => {
                format!("寄存器 {} 越界（nregs = {}）", reg, nregs)
            }
            EvalError::NotAnInt => "算术运算要求 INT".to_string(),
            EvalError::MixedTypes => "两侧类型不属于同一大类".to_string(),
            EvalError::Decimal { reason } => reason.clone(),
            EvalError::NotABool => "期望真值，实际是可存储的值".to_string(),
            EvalError::DivisionByZero => "除以零".to_string(),
            EvalError::Overflow { op } => format!("整数溢出（{}）", op),
            EvalError::BadPattern { reason } => format!("正则表达式不合法：{}", reason),
            EvalError::BadDate { value } => {
                format!("`{}` 不是一个能取年份的日期 / 时间戳文本", value)
            }
            EvalError::NoEnv { what } => format!("{} 需要求值环境，而这里没有", what),
            EvalError::Nested(e) => e.to_string(),
            EvalError::ScalarSubqueryRows { rows } => {
                format!("标量子查询返回了 {} 行（SQL 要求最多 1 行）", rows)
            }
        }
    }

    /// 补上节点下标，转成本 crate 的统一错误类型。
    ///
    /// 传 `None` 表示这个程序不属于任何计划节点（例如 `INSERT` 里的值表达式）。
    ///
    /// [`EvalError::Nested`] 例外：内层子查询的错误**原样返回**——它已经带着自己的
    /// 节点号了，包进 `IrRuntime` 只会把它拍扁成一行字。
    pub fn into_db_error(self, node: Option<usize>) -> DbError {
        match self {
            EvalError::Nested(e) => *e,
            other => DbError::IrRuntime {
                node,
                message: other.message(),
            },
        }
    }
}

/// 线性表达式程序：一串 [`Step`]，结果留在 [`Prog::result`] 号寄存器里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prog {
    steps: Vec<Step>,
    result: Reg,
    nregs: Reg,
}

impl Prog {
    /// 由指令序列直接构造（一般用 [`ProgBuilder`] 更省事）。
    pub fn new(steps: Vec<Step>, result: Reg, nregs: Reg) -> Self {
        Prog {
            steps,
            result,
            nregs,
        }
    }

    /// 指令序列。
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// 结果所在的寄存器。
    pub fn result(&self) -> Reg {
        self.result
    }

    /// 声明的寄存器数量。
    pub fn nregs(&self) -> Reg {
        self.nregs
    }

    /// 静态校验：先定义后使用、下标不越界、结果已定义、逐 step 类型推导。
    ///
    /// 返回结果类型。`input` 是本算子**输入**的列定义。
    pub fn verify(&self, input: &[Column]) -> Result<ScalarType> {
        let mut defined: Vec<Option<ScalarType>> = vec![None; self.nregs as usize];

        for (at, step) in self.steps.iter().enumerate() {
            match step {
                Step::Col { dst, index } => {
                    let column = input.get(*index).ok_or_else(|| {
                        invalid(format!(
                            "step {}: 列下标 {} 越界（输入只有 {} 列）",
                            at,
                            index,
                            input.len()
                        ))
                    })?;
                    // 列的类型直接来自它的大类——不再假设是 INT
                    let ty = ScalarType::Value(column.ty.family());
                    write_reg(&mut defined, *dst, ty, at)?;
                }
                Step::Const { dst, value } => write_reg(&mut defined, *dst, value.ty(), at)?,
                Step::Unary { dst, op, src } => {
                    let src_ty = read_reg(&defined, *src, at)?;
                    let ty = match op {
                        UnOp::Neg => {
                            require(src_ty, int_type(), at, "取负（-）")?;
                            int_type()
                        }
                        UnOp::Not => {
                            require(src_ty, ScalarType::Bool, at, "NOT")?;
                            ScalarType::Bool
                        }
                    };
                    write_reg(&mut defined, *dst, ty, at)?;
                }
                Step::Binary { dst, op, lhs, rhs } => {
                    let l = read_reg(&defined, *lhs, at)?;
                    let r = read_reg(&defined, *rhs, at)?;
                    let ty = binary_type(*op, l, r, at)?;
                    write_reg(&mut defined, *dst, ty, at)?;
                }
                Step::IsNull { dst, src, .. } => {
                    // 任何类型都能拿来做 IS NULL
                    read_reg(&defined, *src, at)?;
                    write_reg(&mut defined, *dst, ScalarType::Bool, at)?;
                }
                Step::Case {
                    dst,
                    cond,
                    then_prog,
                    else_prog,
                    widen,
                } => {
                    require(read_reg(&defined, *cond, at)?, ScalarType::Bool, at, "CASE 的条件")?;
                    // 两个分支各自面对**同一个**输入 schema（它们是自足程序）
                    let then_ty = then_prog.verify(input)?;
                    let else_ty = else_prog.verify(input)?;
                    let unified = unify_branches(then_ty, else_ty, at)?;
                    // 放大标志必须与类型推导一致：这是降级与校验之间的一条约定，
                    // 对不上就报错（而不是让 `Int` 悄悄落进 `NUMERIC` 列）
                    let needs_widen = unified == ScalarType::Value(TypeFamily::Numeric);
                    if *widen != needs_widen {
                        return Err(invalid(format!(
                            "step {}: CASE 的放大标志是 {}，而两个分支的类型 {} / {} 推导出来应该是 {}",
                            at, widen, then_ty, else_ty, needs_widen
                        )));
                    }
                    write_reg(&mut defined, *dst, unified, at)?;
                }
                Step::And { dst, left, right_prog } | Step::Or { dst, left, right_prog } => {
                    let what = if matches!(step, Step::And { .. }) {
                        "AND"
                    } else {
                        "OR"
                    };
                    require(read_reg(&defined, *left, at)?, ScalarType::Bool, at, what)?;
                    require(right_prog.verify(input)?, ScalarType::Bool, at, what)?;
                    write_reg(&mut defined, *dst, ScalarType::Bool, at)?;
                }
                Step::Call { dst, func, args } => {
                    let mut types = Vec::with_capacity(args.len());
                    for reg in args {
                        types.push(read_reg(&defined, *reg, at)?);
                    }
                    write_reg(&mut defined, *dst, call_type(*func, &types, at)?, at)?;
                }
                Step::Cast { dst, src, to } => {
                    let src_ty = read_reg(&defined, *src, at)?;
                    write_reg(&mut defined, *dst, cast_type(src_ty, *to, at)?, at)?;
                }
                Step::Subquery { dst, spec } => {
                    // `test` 按**当前行**求值（它就是外层表达式），所以用本程序的
                    // 输入 schema 校验。内层计划本身的校验在 `Plan::analyze` 里做
                    // （那里才有符号表）。
                    if let SubMode::Quantified { test, op, .. } = &spec.mode {
                        let test_ty = test.verify(input)?;
                        binary_type(*op, test_ty, spec.inner, at)?;
                    }
                    let ty = subquery_type(spec);
                    write_reg(&mut defined, *dst, ty, at)?;
                }                Step::Outer { dst, ty, .. } => {
                    // 类型是 step 自己带的；与真正的外层 schema 是否一致由
                    // `Plan::analyze` 核对（见 `Step::Outer` 的文档）。
                    write_reg(&mut defined, *dst, *ty, at)?;
                }
            }
        }

        let index = self.result as usize;
        match defined.get(index) {
            Some(Some(ty)) => Ok(*ty),
            Some(None) => Err(invalid(format!(
                "结果寄存器 {} 从未被写过",
                self.result
            ))),
            None => Err(invalid(format!(
                "结果寄存器 {} 越界（nregs = {}）",
                self.result, self.nregs
            ))),
        }
    }

    /// 求值。一个 `for` 循环 + 一个寄存器数组，无递归。
    ///
    /// 输入是**一整行**；输入被拆成两半的场合（连接的 `ON`）用 [`Prog::eval_split`]。
    ///
    /// 需要环境（`Step::Outer` / `Step::Subquery`）的程序用 [`Prog::eval_with`]；
    /// 这个入口给的是 [`NullEnv`]，遇到那两个 step 会报 [`EvalError::NoEnv`]。
    pub fn eval(&self, row: &[Value]) -> std::result::Result<Scalar, EvalError> {
        self.eval_with(row, &[], &NullEnv)
    }

    /// 求值，输入可以是**拆开的两半**：`Step::Col { index }` 在
    /// `index < left.len()` 时取左行，否则取右行。
    ///
    /// 连接用它省掉「为每一对候选行拼一整行」：程序里的列下标是按 `左 ++ 右` 编的
    /// （与 [`crate::ir::Plan::analyze`] 推导的 schema 一致），于是一行都不用拼。
    /// [`Prog::eval`] 就是 `eval_split(row, &[])`，取法与过去完全相同。
    pub fn eval_split(
        &self,
        left: &[Value],
        right: &[Value],
    ) -> std::result::Result<Scalar, EvalError> {
        self.eval_with(left, right, &NullEnv)
    }

    /// 带环境的求值（[`Prog::eval`] / [`Prog::eval_split`] 的通用版本）。
    ///
    /// 输入拆成两半的惰性只在 `Step::Col` 上保持；**只有**遇到 `Step::Subquery`
    /// 才把两半拼成一行（内层的外层引用要的是一个完整的 schema），连接为每一对
    /// 候选行拼整行的代价因此一点都没多付。
    pub fn eval_with(
        &self,
        left: &[Value],
        right: &[Value],
        env: &dyn Env,
    ) -> std::result::Result<Scalar, EvalError> {
        let mut regs: Vec<Scalar> = vec![Scalar::Value(Value::Int(0)); self.nregs as usize];

        for step in &self.steps {
            match step {
                Step::Col { dst, index } => {
                    let value = column_of(left, right, *index)?;
                    // 任何可存储类型都能进寄存器，不再要求是 INT
                    put(&mut regs, *dst, Scalar::Value(value.clone()), self.nregs)?;
                }
                Step::Const { dst, value } => put(&mut regs, *dst, value.clone(), self.nregs)?,
                Step::Unary { dst, op, src } => {
                    let s = get(&regs, *src, self.nregs)?;
                    let v = match op {
                        UnOp::Neg => Scalar::Value(Value::Int(
                            s.as_int()
                                .ok_or(EvalError::NotAnInt)?
                                .checked_neg()
                                .ok_or(EvalError::Overflow { op: "-" })?,
                        )),
                        UnOp::Not => Scalar::Truth(s.as_truth().ok_or(EvalError::NotABool)?.not()),
                    };
                    put(&mut regs, *dst, v, self.nregs)?;
                }
                Step::Binary { dst, op, lhs, rhs } => {
                    let l = get(&regs, *lhs, self.nregs)?;
                    let r = get(&regs, *rhs, self.nregs)?;
                    let v = apply(*op, l, r)?;
                    put(&mut regs, *dst, v, self.nregs)?;
                }
                Step::Case {
                    dst,
                    cond,
                    then_prog,
                    else_prog,
                    widen,
                } => {
                    let take_then = get(&regs, *cond, self.nregs)?
                        .as_truth()
                        .ok_or(EvalError::NotABool)?
                        .is_true();
                    // **只有被选中的那一支才求值**：死分支里的运行时错误不该冒出来
                    let picked = if take_then {
                        then_prog.eval_with(left, right, env)?
                    } else {
                        else_prog.eval_with(left, right, env)?
                    };
                    let picked = if *widen { widen_branches(picked) } else { picked };
                    put(&mut regs, *dst, picked, self.nregs)?;
                }
                Step::And {
                    dst,
                    left: l,
                    right_prog,
                } => {
                    let t = get(&regs, *l, self.nregs)?
                        .as_truth()
                        .ok_or(EvalError::NotABool)?;
                    // FALSE AND 任何 = FALSE，所以左侧为假时右侧**不求值**
                    let v = if t == Tri::False {
                        Tri::False
                    } else {
                        let r = right_prog.eval_with(left, right, env)?;
                        t.and(r.as_truth().ok_or(EvalError::NotABool)?)
                    };
                    put(&mut regs, *dst, Scalar::Truth(v), self.nregs)?;
                }
                Step::Or {
                    dst,
                    left: l,
                    right_prog,
                } => {
                    let t = get(&regs, *l, self.nregs)?
                        .as_truth()
                        .ok_or(EvalError::NotABool)?;
                    // TRUE OR 任何 = TRUE，所以左侧为真时右侧**不求值**
                    let v = if t == Tri::True {
                        Tri::True
                    } else {
                        let r = right_prog.eval_with(left, right, env)?;
                        t.or(r.as_truth().ok_or(EvalError::NotABool)?)
                    };
                    put(&mut regs, *dst, Scalar::Truth(v), self.nregs)?;
                }
                Step::Call { dst, func, args } => {
                    let mut values = Vec::with_capacity(args.len());
                    for reg in args {
                        values.push(get(&regs, *reg, self.nregs)?);
                    }
                    let v = apply_call(*func, &values)?;
                    put(&mut regs, *dst, v, self.nregs)?;
                }
                Step::Cast { dst, src, to } => {
                    let v = get(&regs, *src, self.nregs)?;
                    let v = cast_value(v, *to)?;
                    put(&mut regs, *dst, v, self.nregs)?;
                }
                Step::IsNull { dst, src, negated } => {
                    // 对**真值**来说，"是 NULL"就是 UNKNOWN —— `(a = b) IS NULL`
                    // 在 SQL 里是合法且常用的写法。
                    let is_null = match get(&regs, *src, self.nregs)? {
                        Scalar::Value(v) => v.is_null(),
                        Scalar::Truth(t) => t == Tri::Unknown,
                    };
                    // 结果永远确定，不会 Unknown：这正是 IS NULL 存在的理由
                    let truth = if is_null != *negated {
                        Tri::True
                    } else {
                        Tri::False
                    };
                    put(&mut regs, *dst, Scalar::Truth(truth), self.nregs)?;
                }
                Step::Subquery { dst, spec } => {
                    // 内层的外层引用面对的是**本程序面对的那一行**，所以这里要把
                    // 拆开的两半拼起来（只在这一步拼）。
                    let row: Vec<Value> = if right.is_empty() {
                        left.to_vec()
                    } else {
                        left.iter().chain(right.iter()).cloned().collect()
                    };
                    let v = env.subquery(spec, &row)?;
                    put(&mut regs, *dst, v, self.nregs)?;
                }
                Step::Outer { dst, depth, index, .. } => {
                    let v = env.outer(*depth, *index)?;
                    put(&mut regs, *dst, v, self.nregs)?;
                }
            }
        }

        regs.get(self.result as usize)
            .cloned()
            .ok_or(EvalError::RegisterOutOfRange {
                reg: self.result,
                nregs: self.nregs,
            })
    }

    /// 求值并当作谓词用；结果必须是真值（可能是 `Unknown`）。
    pub fn eval_truth(&self, row: &[Value]) -> std::result::Result<Tri, EvalError> {
        self.eval(row)?.as_truth().ok_or(EvalError::NotABool)
    }

    /// 同 [`Prog::eval_truth`]，但输入可以是拆开的两半（连接的 `ON` 用）。
    pub fn eval_truth_split(&self, left: &[Value], right: &[Value]) -> std::result::Result<Tri, EvalError> {
        self.eval_split(left, right)?
            .as_truth()
            .ok_or(EvalError::NotABool)
    }

    /// 带环境的"求值成谓词"（见 [`Prog::eval_with`]）。
    pub fn eval_truth_with(
        &self,
        row: &[Value],
        env: &dyn Env,
    ) -> std::result::Result<Tri, EvalError> {
        self.eval_with(row, &[], env)?
            .as_truth()
            .ok_or(EvalError::NotABool)
    }

    /// 这个程序里有子查询 step 吗。
    ///
    /// 谓词下推与索引键用它当**闸门**：含子查询的程序面对的行必须是它被降级时
    /// 那个 schema（`Step::Subquery` 的外层引用按那个 schema 编的），推给单表
    /// `Scan` 就会换掉那一行——那正是 `outers[0] == program_input` 这条不变量
    /// 要挡住的事（见 [`crate::ir::plan`]）。
    pub fn has_subquery(&self) -> bool {
        self.steps.iter().any(|step| match step {
            Step::Subquery { .. } => true,
            _ => subprograms(step).any(Prog::has_subquery),
        })
    }

    /// 这个程序引用外层行吗。
    pub fn has_outer(&self) -> bool {
        self.steps.iter().any(|step| match step {
            Step::Outer { .. } => true,
            _ => subprograms(step).any(Prog::has_outer),
        })
    }

    /// 这个程序引用到的所有输入列下标（含子程序里的）。谓词下推分派用它。
    pub fn columns(&self) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        for step in &self.steps {
            if let Step::Col { index, .. } = step {
                out.insert(*index);
            }
            for prog in subprograms(step) {
                out.extend(prog.columns());
            }
        }
        out
    }

    /// 把两个程序合成一个 `self AND other`（真值程序）。
    ///
    /// 末尾追加一条 [`Step::And`]，`other` 原样当**自足子程序**（它有自己的寄存器
    /// 空间，于是不用整体平移，也不会与 `self` 的寄存器撞上）。**这是短路求值**：
    /// `self` 为 FALSE 时 `other` 不求值。
    ///
    /// 谓词下推要把同一张表上的几个合取项并进同一个 `Scan.filter`，用的就是它。
    ///
    /// > 寄存器数是 `u16`：合并后越界会报 [`DbError::IrInvalid`]，不静默回绕。
    pub fn and(self, other: Prog) -> Result<Prog> {
        let dst = self.nregs;
        let nregs = dst
            .checked_add(1)
            .ok_or_else(|| invalid("合并后的寄存器数超过 u16".to_string()))?;

        let mut steps = self.steps;
        steps.push(Step::And {
            dst,
            left: self.result,
            right_prog: other,
        });
        Ok(Prog::new(steps, dst, nregs))
    }

    /// 把程序里所有列下标整体减掉 `offset`。
    ///
    /// 用途只有一个：**谓词下推**——程序本来是按**扁平 schema**（`左 ++ 右`）编的，
    /// 推到某一张表的 `Scan.filter` 上之后，下标必须是**这张表自己**的列。
    ///
    /// 调用方保证这些下标都落在 `offset` 之后；万一不在，这里报错，**不静默取 0**
    /// （那会读错列，是个安静的错答案）。
    pub fn rebase_columns(&self, offset: usize) -> Result<Prog> {
        let steps = self
            .steps
            .iter()
            .map(|step| match step {
                Step::Col { dst, index } => {
                    let index = index.checked_sub(offset).ok_or_else(|| {
                        invalid(format!("列下标 {} 落在 offset {} 之前", index, offset))
                    })?;
                    Ok(Step::Col { dst: *dst, index })
                }
                // 子程序里的列下标也是按**同一个输入 schema** 编的，要一起平移
                Step::Case {
                    dst,
                    cond,
                    then_prog,
                    else_prog,
                    widen,
                } => Ok(Step::Case {
                    dst: *dst,
                    cond: *cond,
                    then_prog: then_prog.rebase_columns(offset)?,
                    else_prog: else_prog.rebase_columns(offset)?,
                    widen: *widen,
                }),
                Step::And {
                    dst,
                    left,
                    right_prog,
                } => Ok(Step::And {
                    dst: *dst,
                    left: *left,
                    right_prog: right_prog.rebase_columns(offset)?,
                }),
                Step::Or {
                    dst,
                    left,
                    right_prog,
                } => Ok(Step::Or {
                    dst: *dst,
                    left: *left,
                    right_prog: right_prog.rebase_columns(offset)?,
                }),
                other => Ok(other.clone()),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Prog::new(steps, self.result, self.nregs))
    }

    /// 用列名把程序渲染成中缀表达式。`input` 是本算子输入的列定义。
    pub fn explain(&self, input: &[Column]) -> String {
        let n = self.nregs as usize;
        let mut texts: Vec<String> = vec!["?".to_string(); n];
        let mut compound: Vec<bool> = vec![false; n];

        for step in &self.steps {
            match step {
                Step::Col { dst, index } => {
                    let text = input
                        .get(*index)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| format!("#{}", index));
                    set(&mut texts, *dst, text);
                    set(&mut compound, *dst, false);
                }
                Step::Const { dst, value } => {
                    set(&mut texts, *dst, value.to_string());
                    set(&mut compound, *dst, false);
                }
                Step::Unary { dst, op, src } => {
                    let inner = at(&texts, *src);
                    set(&mut texts, *dst, format!("{}({})", op.symbol(), inner));
                    set(&mut compound, *dst, false);
                }
                Step::Case {
                    dst,
                    cond,
                    then_prog,
                    else_prog,
                    ..
                } => {
                    let c = at(&texts, *cond);
                    // 分支是自足程序：各自渲染一遍（寄存器名互不相干）
                    let t = then_prog.explain(input);
                    let e = else_prog.explain(input);
                    set(
                        &mut texts,
                        *dst,
                        format!("CASE WHEN {} THEN {} ELSE {} END", c, t, e),
                    );
                    set(&mut compound, *dst, false);
                }
                Step::And {
                    dst,
                    left,
                    right_prog,
                }
                | Step::Or {
                    dst,
                    left,
                    right_prog,
                } => {
                    let op = if matches!(step, Step::And { .. }) {
                        "AND"
                    } else {
                        "OR"
                    };
                    let l = if flag(&compound, *left) {
                        format!("({})", at(&texts, *left))
                    } else {
                        at(&texts, *left)
                    };
                    // 与 `Step::Binary` 同一套括号规则：右侧是复合表达式就括起来
                    let rendered = right_prog.explain(input);
                    let r = if result_is_compound(right_prog) {
                        format!("({})", rendered)
                    } else {
                        rendered
                    };
                    set(&mut texts, *dst, format!("{} {} {}", l, op, r));
                    set(&mut compound, *dst, true);
                }
                Step::Call { dst, func, args } => {
                    let rendered: Vec<String> =
                        args.iter().map(|reg| at(&texts, *reg)).collect();
                    set(
                        &mut texts,
                        *dst,
                        format!("{}({})", func.name(), rendered.join(", ")),
                    );
                    set(&mut compound, *dst, false);
                }
                Step::Cast { dst, src, to } => {
                    let inner = at(&texts, *src);
                    set(&mut texts, *dst, format!("CAST({} AS {})", inner, to));
                    set(&mut compound, *dst, false);
                }
                Step::Subquery { dst, spec } => {
                    set(&mut texts, *dst, render_subquery(spec));
                    set(&mut compound, *dst, false);
                }
                Step::Outer { dst, depth, index, .. } => {
                    set(&mut texts, *dst, format!("outer[{}]#{}", depth, index));
                    set(&mut compound, *dst, false);
                }
                Step::IsNull { dst, src, negated } => {
                    let inner = at(&texts, *src);
                    let op = if *negated { "IS NOT NULL" } else { "IS NULL" };
                    set(&mut texts, *dst, format!("({} {})", inner, op));
                    set(&mut compound, *dst, false);
                }
                Step::Binary { dst, op, lhs, rhs } => {
                    let l = if flag(&compound, *lhs) {
                        format!("({})", at(&texts, *lhs))
                    } else {
                        at(&texts, *lhs)
                    };
                    let r = if flag(&compound, *rhs) {
                        format!("({})", at(&texts, *rhs))
                    } else {
                        at(&texts, *rhs)
                    };
                    set(&mut texts, *dst, format!("{} {} {}", l, op.symbol(), r));
                    set(&mut compound, *dst, true);
                }
            }
        }

        at(&texts, self.result)
    }
}

/// 增量构建 [`Prog`]，自动分配寄存器。
///
/// 它**不做类型检查**（看不到输入 schema），类型统一留给 [`Prog::verify`]。
/// 因此这里的方法都返回 [`Reg`] 而不是 `Result`。
#[derive(Debug, Default)]
pub struct ProgBuilder {
    steps: Vec<Step>,
    nregs: Reg,
}

impl ProgBuilder {
    /// 空程序。
    pub fn new() -> Self {
        ProgBuilder {
            steps: Vec::new(),
            nregs: 0,
        }
    }

    fn fresh(&mut self) -> Reg {
        let reg = self.nregs;
        self.nregs += 1;
        reg
    }

    /// 读输入行的第 `index` 列。
    pub fn col(&mut self, index: usize) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Col { dst, index });
        dst
    }

    /// 整数常量。
    pub fn int(&mut self, v: i32) -> Reg {
        self.const_(Scalar::Value(Value::Int(v)))
    }

    /// 任意可存储类型的常量（TEXT / TIMESTAMP / NUMERIC 的字面量走这里）。
    pub fn value(&mut self, v: Value) -> Reg {
        self.const_(Scalar::Value(v))
    }

    /// 真值常量。
    pub fn truth(&mut self, v: Tri) -> Reg {
        self.const_(Scalar::Truth(v))
    }

    /// 布尔常量（确定的真/假）。
    pub fn boolean(&mut self, v: bool) -> Reg {
        self.truth(if v { Tri::True } else { Tri::False })
    }

    /// 任意常量。
    pub fn const_(&mut self, value: Scalar) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Const { dst, value });
        dst
    }

    /// 一元运算。
    pub fn unary(&mut self, op: UnOp, src: Reg) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Unary { dst, op, src });
        dst
    }

    /// 二元运算。
    pub fn binary(&mut self, op: BinOp, lhs: Reg, rhs: Reg) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Binary { dst, op, lhs, rhs });
        dst
    }

    /// `CASE`：条件为 TRUE 求值 `then_prog`，否则求值 `else_prog`。
    ///
    /// `widen` 见 [`Step::Case`]：统一后的类型是 `NUMERIC` 时为 `true`。
    pub fn case(&mut self, cond: Reg, then_prog: Prog, else_prog: Prog, widen: bool) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Case {
            dst,
            cond,
            then_prog,
            else_prog,
            widen,
        });
        dst
    }

    /// `AND`：左侧不是 FALSE 才求值右侧。
    pub fn and(&mut self, left: Reg, right_prog: Prog) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::And {
            dst,
            left,
            right_prog,
        });
        dst
    }

    /// `OR`：左侧不是 TRUE 才求值右侧。
    pub fn or(&mut self, left: Reg, right_prog: Prog) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Or {
            dst,
            left,
            right_prog,
        });
        dst
    }

    /// 标量函数调用。
    pub fn call(&mut self, func: ScalarFunc, args: Vec<Reg>) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Call { dst, func, args });
        dst
    }

    /// 类型转换（`x::numeric`）。
    pub fn cast(&mut self, src: Reg, to: DataType) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Cast { dst, src, to });
        dst
    }

    /// 求值一个子查询。
    pub fn subquery(&mut self, spec: SubPlan) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Subquery {
            dst,
            spec: Box::new(spec),
        });
        dst
    }

    /// 读外层查询的某一列（相关子查询用）。
    pub fn outer(&mut self, depth: u16, index: usize, ty: ScalarType) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Outer {
            dst,
            depth,
            index,
            ty,
        });
        dst
    }

    /// `IS NULL` / `IS NOT NULL`。
    pub fn is_null(&mut self, src: Reg, negated: bool) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::IsNull { dst, src, negated });
        dst
    }

    /// 收尾：指定哪个寄存器是结果。
    pub fn finish(self, result: Reg) -> Prog {
        Prog {
            steps: self.steps,
            result,
            nregs: self.nregs,
        }
    }
}

fn invalid(message: String) -> DbError {
    DbError::IrInvalid {
        node: None,
        message,
    }
}

/// 这条指令写的寄存器。
fn step_dst(step: &Step) -> Option<Reg> {
    match step {
        Step::Col { dst, .. }
        | Step::Const { dst, .. }
        | Step::Unary { dst, .. }
        | Step::Binary { dst, .. }
        | Step::Case { dst, .. }
        | Step::And { dst, .. }
        | Step::Or { dst, .. }
        | Step::Call { dst, .. }
        | Step::Cast { dst, .. }
        | Step::Subquery { dst, .. }
        | Step::Outer { dst, .. }
        | Step::IsNull { dst, .. } => Some(*dst),
    }
}

/// 一个子程序的渲染结果是"复合表达式"吗（渲染时要不要加括号）。
///
/// [`Prog::explain`] 用的是"每个寄存器一个 compound 标志"那套；子程序是独立渲染的，
/// 所以这里只看**写结果的那一条指令**——与 `Binary` / `And` / `Or` 渲染时置
/// `compound = true` 的口径一致。
fn result_is_compound(prog: &Prog) -> bool {
    let dst = prog.result();
    prog.steps()
        .iter()
        .rev()
        .find(|step| step_dst(step) == Some(dst))
        .is_some_and(|step| matches!(step, Step::Binary { .. } | Step::And { .. } | Step::Or { .. }))
}

/// 一条指令里的**子程序**（`CASE` 的两个分支、`AND`/`OR` 的右侧）。
///
/// 它们与外层程序面对同一个输入行，但有**自己的寄存器空间**——所以遍历一个
/// 程序时（校验、求值、渲染、收集列下标）必须走进它们，寄存器平移却不必。
fn subprograms(step: &Step) -> impl Iterator<Item = &Prog> {
    let mut branches: Vec<&Prog> = Vec::new();
    match step {
        Step::Case {
            then_prog,
            else_prog,
            ..
        } => {
            branches.push(then_prog);
            branches.push(else_prog);
        }
        Step::And { right_prog, .. } | Step::Or { right_prog, .. } => {
            branches.push(right_prog);
        }
        _ => {}
    }
    branches.into_iter()
}

/// 一个子查询 step 的结果类型。
fn subquery_type(spec: &SubPlan) -> ScalarType {
    match spec.mode {
        SubMode::Scalar => spec.inner,
        SubMode::Exists | SubMode::Quantified { .. } => ScalarType::Bool,
    }
}

/// [`Prog::explain`] 里子查询的渲染。
fn render_subquery(spec: &SubPlan) -> String {
    let kind = match &spec.mode {
        SubMode::Scalar => "scalar".to_string(),
        SubMode::Exists => "exists".to_string(),
        SubMode::Quantified { op, all, .. } => {
            format!("{} {}", op.symbol(), if *all { "ALL" } else { "ANY" })
        }
    };
    format!("<sublink:{}{}>", kind, if spec.correlated { ", correlated" } else { "" })
}

fn write_reg(
    defined: &mut [Option<ScalarType>],
    reg: Reg,
    ty: ScalarType,
    at: usize,
) -> Result<()> {
    match defined.get_mut(reg as usize) {
        Some(slot) => {
            *slot = Some(ty);
            Ok(())
        }
        None => Err(invalid(format!(
            "step {}: 目的寄存器 {} 越界（nregs = {}）",
            at,
            reg,
            defined.len()
        ))),
    }
}

fn read_reg(defined: &[Option<ScalarType>], reg: Reg, at: usize) -> Result<ScalarType> {
    match defined.get(reg as usize) {
        Some(Some(ty)) => Ok(*ty),
        Some(None) => Err(invalid(format!(
            "step {}: 寄存器 {} 在被读之前还没写过（线性 IR 要求先定义后使用）",
            at, reg
        ))),
        None => Err(invalid(format!(
            "step {}: 寄存器 {} 越界（nregs = {}）",
            at,
            reg,
            defined.len()
        ))),
    }
}

fn require(actual: ScalarType, want: ScalarType, at: usize, what: &str) -> Result<()> {
    if actual == want {
        Ok(())
    } else {
        Err(invalid(format!(
            "step {}: {} 需要 {}，实际是 {}",
            at, what, want, actual
        )))
    }
}

fn binary_type(op: BinOp, l: ScalarType, r: ScalarType, at: usize) -> Result<ScalarType> {
    if op.is_logical() {
        require(l, ScalarType::Bool, at, op.symbol())?;
        require(r, ScalarType::Bool, at, op.symbol())?;
        return Ok(ScalarType::Bool);
    }
    if op.is_pattern() {
        return pattern_type(op, l, r, at);
    }
    if op.is_comparison() {
        // NULL 与任何可存储的大类都相容：`x = NULL` 是合法写法，结果是 UNKNOWN。
        // 另外 INT 与 NUMERIC 可以直接比——见 [`numeric_pair`]。
        let compatible = l == r
            || matches!(l, ScalarType::Null)
            || matches!(r, ScalarType::Null)
            || numeric_pair(l, r);
        if !compatible {
            return Err(invalid(format!(
                "step {}: {} 两侧类型不同（{} 与 {}）",
                at,
                op.symbol(),
                l,
                r
            )));
        }
        return Ok(ScalarType::Bool);
    }
    // 算术：INT 与 NUMERIC。**任一侧是 NUMERIC，结果就是 NUMERIC**
    // （对应 PostgreSQL 里 int → numeric 的隐式转换）。
    let left = arithmetic_family(l, at, op)?;
    let right = arithmetic_family(r, at, op)?;
    Ok(if left == TypeFamily::Numeric || right == TypeFamily::Numeric {
        ScalarType::Value(TypeFamily::Numeric)
    } else {
        int_type()
    })
}

/// `LIKE` / `ILIKE` / `~` / `~*` 的类型规则：两侧都必须是 **TEXT**（或 NULL），
/// 结果是布尔。别的家族（`timestamp LIKE …`、`int ~* …`）在这里就被拒绝——
/// 与比较运算一样，宁可拒绝也不要安静地按大类排名去比。
fn pattern_type(op: BinOp, l: ScalarType, r: ScalarType, at: usize) -> Result<ScalarType> {
    let is_text = |ty: ScalarType| {
        matches!(ty, ScalarType::Value(TypeFamily::Text) | ScalarType::Null)
    };
    if !is_text(l) || !is_text(r) {
        return Err(invalid(format!(
            "step {}: {} 的两侧必须是 TEXT，实际是 {} 与 {}",
            at,
            op.symbol(),
            l,
            r
        )));
    }
    Ok(ScalarType::Bool)
}

/// 算术操作数只能是 INT 或 NUMERIC，返回它的大类。
fn arithmetic_family(ty: ScalarType, at: usize, op: BinOp) -> Result<TypeFamily> {
    match ty {
        ScalarType::Value(TypeFamily::Int) => Ok(TypeFamily::Int),
        ScalarType::Value(TypeFamily::Numeric) => Ok(TypeFamily::Numeric),
        other => Err(invalid(format!(
            "step {}: {} 的操作数必须是 INT 或 NUMERIC，实际是 {}",
            at,
            op.symbol(),
            other
        ))),
    }
}

pub(crate) fn apply(op: BinOp, l: Scalar, r: Scalar) -> std::result::Result<Scalar, EvalError> {
    if op.is_pattern() {
        // NULL 传播：任一侧是 NULL → UNKNOWN（与比较运算同一条规矩）
        if l.is_null() || r.is_null() {
            return Ok(Scalar::Truth(Tri::Unknown));
        }
        let (value, pattern) = match (&l, &r) {
            (Scalar::Value(a), Scalar::Value(b)) => (a.to_string(), b.to_string()),
            _ => return Err(EvalError::NotABool),
        };
        let matched = if op.is_like() {
            like_match(&value, &pattern, op.case_insensitive())
        } else {
            regex_match(&value, &pattern, op.case_insensitive())?
        };
        let truth = if matched != op.negated() {
            Tri::True
        } else {
            Tri::False
        };
        return Ok(Scalar::Truth(truth));
    }

    if op.is_arithmetic() {
        // NULL 传播：任何一侧是 NULL，结果就是 NULL
        if l.is_null() || r.is_null() {
            return Ok(Scalar::Value(Value::Null));
        }

        let (left, right) = match (&l, &r) {
            (Scalar::Value(a), Scalar::Value(b)) => (a, b),
            _ => return Err(EvalError::NotAnInt),
        };

        // 两侧都是 INT 走整数路径，结果仍是 INT
        if let (Value::Int(a), Value::Int(b)) = (left, right) {
            let v = match op {
                BinOp::Add => a.checked_add(*b).ok_or(EvalError::Overflow { op: "+" })?,
                BinOp::Sub => a.checked_sub(*b).ok_or(EvalError::Overflow { op: "-" })?,
                BinOp::Mul => a.checked_mul(*b).ok_or(EvalError::Overflow { op: "*" })?,
                BinOp::Div => {
                    if *b == 0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    a.checked_div(*b).ok_or(EvalError::Overflow { op: "/" })?
                }
                _ => unreachable!("is_arithmetic 只包含四则运算"),
            };
            return Ok(Scalar::Value(Value::Int(v)));
        }

        // 只要有一侧是 NUMERIC，就走定点十进制
        let decimal_op = match op {
            BinOp::Add => DecimalOp::Add,
            BinOp::Sub => DecimalOp::Sub,
            BinOp::Mul => DecimalOp::Mul,
            BinOp::Div => DecimalOp::Div,
            _ => unreachable!("is_arithmetic 只包含四则运算"),
        };
        let value = left
            .decimal(decimal_op, right)
            .map_err(|reason| EvalError::Decimal { reason })?;
        return Ok(Scalar::Value(value));
    }

    if op.is_logical() {
        let a = l.as_truth().ok_or(EvalError::NotABool)?;
        let b = r.as_truth().ok_or(EvalError::NotABool)?;
        return Ok(Scalar::Truth(if op == BinOp::And {
            a.and(b)
        } else {
            a.or(b)
        }));
    }

    // 比较：任一侧是 NULL → UNKNOWN（这就是 `= NULL` 不是 false 而是未知）
    if l.is_null() || r.is_null() {
        return Ok(Scalar::Truth(Tri::Unknown));
    }

    let ord = compare(l, r)?;
    let v = match op {
        BinOp::Eq => ord == Ordering::Equal,
        BinOp::Ne => ord != Ordering::Equal,
        BinOp::Lt => ord == Ordering::Less,
        BinOp::Le => ord != Ordering::Greater,
        BinOp::Gt => ord == Ordering::Greater,
        BinOp::Ge => ord != Ordering::Less,
        _ => unreachable!("剩下的都是比较运算"),
    };
    Ok(Scalar::Truth(if v { Tri::True } else { Tri::False }))
}

fn compare(l: Scalar, r: Scalar) -> std::result::Result<Ordering, EvalError> {
    // INT 与 NUMERIC 先放大再比（见 [`widen_numeric`]）
    let (l, r) = widen_numeric(l, r);
    match (l, r) {
        // Value 的 Ord 是手写的：NUMERIC 走十进制定点比较，TIMESTAMP 靠定宽文本
        (Scalar::Value(a), Scalar::Value(b)) => {
            if a.family() != b.family() {
                return Err(EvalError::MixedTypes);
            }
            Ok(a.cmp(&b))
        }
        (Scalar::Truth(a), Scalar::Truth(b)) => Ok(a.cmp(&b)),
        _ => Err(EvalError::MixedTypes),
    }
}

/// `LIKE` / `ILIKE` 的匹配器：`%` 任意长、`_` 任意一个字符、`\` 是转义。
///
/// 自己写而不把模式翻成正则：`LIKE` 的语义小到可以直说，也免得为每一行都编译
/// 一次正则。按**字符**匹配（与 `VARCHAR` 的长度检查口径一致），用的是带回溯的
/// 线性扫描（`star` 记住最近一个 `%`）——不是那条会指数爆炸的递归写法。
fn like_match(value: &str, pattern: &str, case_insensitive: bool) -> bool {
    let text: Vec<char> = value.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();

    let mut ti = 0usize;
    let mut pi = 0usize;
    // 最近一个 `%`：`(pattern 下标, 当时吞到的文本位置)`
    let mut star: Option<(usize, usize)> = None;

    loop {
        if ti == text.len() {
            // 文本看完了：剩下的模式必须全是 `%`（`\%` 是字面反斜杠，不算）
            if pat[pi..].iter().all(|c| *c == '%') {
                return true;
            }
            match star {
                Some((sp, si)) if si < text.len() => {
                    pi = sp + 1;
                    ti = si + 1;
                    star = Some((sp, si + 1));
                    continue;
                }
                _ => return false,
            }
        }

        // `%` 先不吞字符（吞 0 个），让后面的模式先试；试不动了再回溯多吞一个
        let mut consumed = true;
        let matched = match pat.get(pi) {
            Some('%') => {
                star = Some((pi, ti));
                pi += 1;
                consumed = false;
                true
            }
            Some('_') => {
                pi += 1;
                true
            }
            Some('\\') => {
                let (literal, step) = match pat.get(pi + 1) {
                    Some(next) => (*next, 2),
                    // 模式以 `\` 结尾：按字面 `\` 比
                    None => ('\\', 1),
                };
                if char_eq(literal, text[ti], case_insensitive) {
                    pi += step;
                    true
                } else {
                    false
                }
            }
            Some(ch) => {
                if char_eq(*ch, text[ti], case_insensitive) {
                    pi += 1;
                    true
                } else {
                    false
                }
            }
            None => false,
        };

        if matched {
            if consumed {
                ti += 1;
            }
            continue;
        }

        match star {
            Some((sp, si)) if si < text.len() => {
                pi = sp + 1;
                ti = si + 1;
                star = Some((sp, si + 1));
            }
            _ => return false,
        }
    }
}

/// 两个字符在（可能的）忽略大小写意义下相等。
fn char_eq(a: char, b: char, case_insensitive: bool) -> bool {
    if !case_insensitive {
        return a == b;
    }
    // `to_lowercase` 可能给出多个字符（例如 `İ`），所以比迭代器而不是比字符
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// `~` / `~*` 的正则匹配。
///
/// 用的是 Rust 的 `regex`（本 crate 已有的依赖），**不是 POSIX ERE**：两边差在
/// 少数转义上，见 [`translate_pattern`]。每一行都编译一次模式——模式通常是常量，
/// 但本版没有模式缓存，这在长表上是个已知的开销。
fn regex_match(
    value: &str,
    pattern: &str,
    case_insensitive: bool,
) -> std::result::Result<bool, EvalError> {
    let translated = translate_pattern(pattern);
    let re = regex::RegexBuilder::new(&translated)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| EvalError::BadPattern {
            reason: e.to_string(),
        })?;
    Ok(re.is_match(value))
}

/// 把 PostgreSQL 正则里与 Rust `regex` 不同的那几个转义翻译过来。
///
/// 只翻**词边界**这一组（`~* '\mman\M'` 这种写法要用）：`\m` / `\M` / `\y` 都是
/// 词边界、`\Y` 是非词边界。其余转义（`\d`、`\w`、`\s`、`\.`…）两边一致，原样
/// 交给 `regex`；不认识的一律原样送过去，让 `regex` 自己报错。
fn translate_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('m') | Some('M') | Some('y') => out.push_str("\\b"),
            Some('Y') => out.push_str("\\B"),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }

    out
}

/// 「整数」这个类型，写起来短一点。
fn int_type() -> ScalarType {    ScalarType::Value(TypeFamily::Int)
}

/// 「一侧 INT、另一侧 NUMERIC」吗。
///
/// 这是 PostgreSQL 里**唯一**允许的隐式放大：`int = numeric` 合法（`int → numeric`
/// 是无损放大），而 `text = int`、`timestamp = text` 都会报
/// `operator does not exist`。所以这里只放行这一对，别的跨大类仍然拒绝。
fn numeric_pair(l: ScalarType, r: ScalarType) -> bool {
    matches!(
        (l, r),
        (
            ScalarType::Value(TypeFamily::Int),
            ScalarType::Value(TypeFamily::Numeric)
        ) | (
            ScalarType::Value(TypeFamily::Numeric),
            ScalarType::Value(TypeFamily::Int)
        )
    )
}

/// 比较前先把 INT 放大成 NUMERIC。
///
/// **这一步是必须的，不只是为了让类型检查过关**：`Value::cmp` 对跨大类的兜底是
/// 「按大类的排名比」（`Int` 排 0、`Numeric` 排 1），于是 `3 > 1.5` 会被算成
/// `Less`——**一个安静的错答案**。所以放行类型之后一定要在这里真的做转换。
fn widen_numeric(l: Scalar, r: Scalar) -> (Scalar, Scalar) {
    match (&l, &r) {
        (Scalar::Value(Value::Int(i)), Scalar::Value(Value::Numeric(_))) => {
            (Scalar::Value(Value::Numeric(i.to_string())), r)
        }
        (Scalar::Value(Value::Numeric(_)), Scalar::Value(Value::Int(i))) => {
            (l, Scalar::Value(Value::Numeric(i.to_string())))
        }
        _ => (l, r),
    }
}

/// `CASE` 两个分支的类型统一：同类型就取它，一侧是 NULL 就采纳另一侧，
/// 一侧 `INT` 一侧 `NUMERIC` 就统一成 `NUMERIC`（**与比较侧同一条隐式放大规则**）。
///
/// 只在这里放行是不够的：运行时也要真的把 `Int` 转成 `Numeric`，见 [`widen_branches`]。
fn unify_branches(a: ScalarType, b: ScalarType, at: usize) -> Result<ScalarType> {
    if a == b {
        return Ok(a);
    }
    if matches!(a, ScalarType::Null) {
        return Ok(b);
    }
    if matches!(b, ScalarType::Null) {
        return Ok(a);
    }
    // PostgreSQL 里唯一的隐式放大：`CASE WHEN true THEN 1 ELSE 1.5 END` 的类型是 numeric
    if numeric_pair(a, b) {
        return Ok(ScalarType::Value(TypeFamily::Numeric));
    }
    Err(invalid(format!(
        "step {}: CASE 两个分支类型不同（{} 与 {}）",
        at, a, b
    )))
}

/// 两个分支统一后的类型；统一不了就是 `None`。
///
/// 与 [`unify_branches`] 是同一条规则，只是不提错误、不需要 step 下标——
/// 降级阶段算 [`Step::Case::widen`] 的类型探针用它。
pub(crate) fn unified_branch_type(then_ty: ScalarType, else_ty: ScalarType) -> Option<ScalarType> {
    unify_branches(then_ty, else_ty, 0).ok()
}

/// `CASE` 取到的那一侧要按**统一后的类型**真的转换（`INT` → `NUMERIC`）。
///
/// 只改 [`unify_branches`]（类型检查）是不够的：`CASE WHEN true THEN 1 ELSE 1.5 END`
/// 的列类型成了 `NUMERIC`，而取到的值还是 `Value::Int`，物化时会以 `TypeMismatch`
/// 报错。这与比较侧的 [`widen_numeric`] 是同一条规矩——类型放行之后，运行时也得真转。
///
/// 是否需要转由 [`Step::Case::widen`] 决定（那是降级时按两个分支的类型算出来的），
/// 这里只做转换本身。
fn widen_branches(picked: Scalar) -> Scalar {
    match picked {
        Scalar::Value(Value::Int(i)) => Scalar::Value(Value::Numeric(i.to_string())),
        other => other,
    }
}

/// 标量函数的参数与返回类型。
fn call_type(func: ScalarFunc, args: &[ScalarType], at: usize) -> Result<ScalarType> {
    if let Some(arity) = func.arity() {
        if args.len() != arity {
            return Err(invalid(format!(
                "step {}: {} 需要 {} 个参数，实际给了 {} 个",
                at,
                func.name(),
                arity,
                args.len()
            )));
        }
    } else if args.is_empty() {
        return Err(invalid(format!(
            "step {}: {} 至少需要 1 个参数",
            at,
            func.name()
        )));
    }

    // 「这一位要么是文本、要么是 NULL」——文本函数共同的参数规则
    let text_arg = |index: usize, what: &str| -> Result<()> {
        match args[index] {
            ScalarType::Value(TypeFamily::Text) | ScalarType::Null => Ok(()),
            other => Err(invalid(format!(
                "step {}: {} 的{}必须是 TEXT，实际是 {}",
                at,
                func.name(),
                what,
                other
            ))),
        }
    };

    match func {
        ScalarFunc::Round => {
            // NULL 一律放行：`ROUND(NULL, 2)` 的结果就是 NULL
            match args[0] {
                ScalarType::Value(TypeFamily::Int)
                | ScalarType::Value(TypeFamily::Numeric)
                | ScalarType::Null => {}
                other => {
                    return Err(invalid(format!(
                        "step {}: ROUND 的第一个参数必须是数值，实际是 {}",
                        at, other
                    )))
                }
            }
            match args[1] {
                ScalarType::Value(TypeFamily::Int) | ScalarType::Null => {}
                other => {
                    return Err(invalid(format!(
                        "step {}: ROUND 的位数必须是 INT，实际是 {}",
                        at, other
                    )))
                }
            }
            // 与 PostgreSQL 一致：round(numeric, int) 返回 numeric
            Ok(ScalarType::Value(TypeFamily::Numeric))
        }

        ScalarFunc::Upper => {
            text_arg(0, "参数")?;
            Ok(ScalarType::Value(TypeFamily::Text))
        }

        ScalarFunc::Left => {
            text_arg(0, "第一个参数")?;
            match args[1] {
                ScalarType::Value(TypeFamily::Int) | ScalarType::Null => {}
                other => {
                    return Err(invalid(format!(
                        "step {}: LEFT 的第二个参数必须是 INT，实际是 {}",
                        at, other
                    )))
                }
            }
            Ok(ScalarType::Value(TypeFamily::Text))
        }

        ScalarFunc::ConcatWs => {
            // 分隔符必须是文本（NULL 也放行：结果是 NULL）
            text_arg(0, "分隔符")?;
            // 其余参数**可以是任何可存储类型**（PostgreSQL 会把它们转成文本），
            // 只有真值不行——那是 IR 私有的、进不了结果列。
            if let Some(other) = args.iter().find(|ty| matches!(ty, ScalarType::Bool)) {
                return Err(invalid(format!(
                    "step {}: CONCAT_WS 的参数不能是 {}",
                    at, other
                )));
            }
            Ok(ScalarType::Value(TypeFamily::Text))
        }

        ScalarFunc::Length => {
            text_arg(0, "参数")?;
            Ok(int_type())
        }

        ScalarFunc::ExtractYear => {
            match args[0] {
                ScalarType::Value(TypeFamily::Text)
                | ScalarType::Value(TypeFamily::Timestamp)
                | ScalarType::Value(TypeFamily::Date)
                | ScalarType::Null => {}
                other => {
                    return Err(invalid(format!(
                        "step {}: EXTRACT 的参数必须是 TIMESTAMP / DATE 或日期文本，实际是 {}",
                        at, other
                    )))
                }
            }
            Ok(int_type())
        }
    }
}

fn apply_call(func: ScalarFunc, args: &[Scalar]) -> std::result::Result<Scalar, EvalError> {
    match func {
        ScalarFunc::Round => {
            // 任一侧是 NULL → NULL
            if args.iter().any(|arg| arg.is_null()) {
                return Ok(Scalar::Value(Value::Null));
            }
            let value = match &args[0] {
                Scalar::Value(v) => v.clone(),
                Scalar::Truth(_) => return Err(EvalError::NotAnInt),
            };
            let digits = args[1].as_int().ok_or(EvalError::NotAnInt)?;
            // 负数表示舍到十位、百位（`ROUND(1234.5, -2)` → `1200`）
            let rounded = value
                .round_to(digits)
                .map_err(|reason| EvalError::Decimal { reason })?;
            Ok(Scalar::Value(rounded))
        }

        ScalarFunc::Upper => {
            let text = scalar_text(&args[0])?;
            Ok(match text {
                Some(text) => Scalar::Value(Value::Text(text.to_uppercase())),
                None => Scalar::Value(Value::Null),
            })
        }

        ScalarFunc::Left => {
            let text = scalar_text(&args[0])?;
            let digits = args[1].as_int();
            match (text, digits) {
                (None, _) | (_, None) => Ok(Scalar::Value(Value::Null)),
                (Some(text), Some(n)) => {
                    let chars: Vec<char> = text.chars().collect();
                    // PostgreSQL：`n` 为负时去掉右边 `|n|` 个字符
                    let keep = if n >= 0 {
                        (n as usize).min(chars.len())
                    } else {
                        chars.len().saturating_sub(n.unsigned_abs() as usize)
                    };
                    Ok(Scalar::Value(Value::Text(chars[..keep].iter().collect())))
                }
            }
        }

        ScalarFunc::ConcatWs => {
            // 分隔符为 NULL → NULL（PostgreSQL 就是这么定的）
            if args[0].is_null() {
                return Ok(Scalar::Value(Value::Null));
            }
            let separator = scalar_text(&args[0])?.unwrap_or_default();
            let mut parts = Vec::with_capacity(args.len());
            for arg in &args[1..] {
                // NULL 参数**跳过**，不算一个空串
                if let Some(text) = scalar_text(arg)? {
                    parts.push(text);
                }
            }
            Ok(Scalar::Value(Value::Text(parts.join(&separator))))
        }

        ScalarFunc::Length => {
            let text = scalar_text(&args[0])?;
            match text {
                None => Ok(Scalar::Value(Value::Null)),
                Some(text) => {
                    let count = i32::try_from(text.chars().count())
                        .map_err(|_| EvalError::Overflow { op: "LENGTH" })?;
                    Ok(Scalar::Value(Value::Int(count)))
                }
            }
        }

        ScalarFunc::ExtractYear => {
            let text = scalar_text(&args[0])?;
            match text {
                None => Ok(Scalar::Value(Value::Null)),
                Some(text) => {
                    let year = text
                        .chars()
                        .take(4)
                        .collect::<String>()
                        .parse::<i32>()
                        .map_err(|_| EvalError::BadDate { value: text.clone() })?;
                    if text.chars().count() < 4 {
                        return Err(EvalError::BadDate { value: text });
                    }
                    Ok(Scalar::Value(Value::Int(year)))
                }
            }
        }
    }
}

/// 一个标量当文本用；NULL 给 `None`，真值给错误。
fn scalar_text(value: &Scalar) -> std::result::Result<Option<String>, EvalError> {
    match value {
        Scalar::Value(Value::Null) => Ok(None),
        Scalar::Value(v) => Ok(Some(v.to_string())),
        Scalar::Truth(_) => Err(EvalError::NotABool),
    }
}

/// `x::to` 的类型规则。
///
/// 只管**值模型里真有意义**的几组：同族恒等（`x::int`、`name::varchar(3)`）、
/// `INT → NUMERIC`、`NULL → 任意`。别的组合（`int::text`、`timestamp::int`）
/// 在这里就报 `IrInvalid`——宁可拒绝，也不要给一个安静的错误答案。
fn cast_type(from: ScalarType, to: DataType, at: usize) -> Result<ScalarType> {
    let target = ScalarType::Value(to.family());
    if matches!(from, ScalarType::Null) {
        return Ok(target);
    }
    match from {
        ScalarType::Value(family) if family == to.family() => Ok(target),
        ScalarType::Value(TypeFamily::Int) if to.family() == TypeFamily::Numeric => Ok(target),
        other => Err(invalid(format!(
            "step {}: 不能把 {} 转成 {}",
            at, other, to
        ))),
    }
}

/// `x::to` 的求值：类型规则放行之后，这里必须**真的转**（同 [`widen_numeric`]）。
fn cast_value(value: Scalar, to: DataType) -> std::result::Result<Scalar, EvalError> {
    let value = match value {
        Scalar::Value(v) => v,
        Scalar::Truth(_) => return Err(EvalError::NotABool),
    };

    let converted = match (&value, to.family()) {
        (Value::Null, _) => Value::Null,
        (Value::Int(i), TypeFamily::Numeric) => Value::Numeric(i.to_string()),
        // 同族恒等（含 INT → INT，以及 typmod 的收窄只发生在 INSERT 时）
        (other, _) => other.clone(),
    };

    Ok(Scalar::Value(converted))
}

/// 从**分裂的输入**里取第 `index` 列。
///
/// 程序里的列下标是按 `左 ++ 右` 编的：落在这两半的哪一半，就取哪一半。
/// `eval` 传的是 `(整行, 空)`，于是取法与过去完全相同；越界错误里的 `arity`
/// 也是两半之和，即过去的整行长度。
fn column_of<'r>(
    left: &'r [Value],
    right: &'r [Value],
    index: usize,
) -> std::result::Result<&'r Value, EvalError> {
    match left.get(index) {
        Some(value) => Ok(value),
        // `left.get(index)` 是 None 就保证了 `index >= left.len()`，减法不会下溢
        None => right
            .get(index - left.len())
            .ok_or(EvalError::ColumnOutOfRange {
                index,
                arity: left.len() + right.len(),
            }),
    }
}

fn put(
    regs: &mut [Scalar],
    dst: Reg,
    v: Scalar,
    nregs: Reg,
) -> std::result::Result<(), EvalError> {
    match regs.get_mut(dst as usize) {
        Some(slot) => {
            *slot = v;
            Ok(())
        }
        None => Err(EvalError::RegisterOutOfRange { reg: dst, nregs }),
    }
}

fn get(regs: &[Scalar], reg: Reg, nregs: Reg) -> std::result::Result<Scalar, EvalError> {
    regs.get(reg as usize)
        .cloned()
        .ok_or(EvalError::RegisterOutOfRange { reg, nregs })
}

fn at<T: Clone + Default>(v: &[T], index: Reg) -> T {
    v.get(index as usize).cloned().unwrap_or_default()
}

fn set<T>(v: &mut [T], index: Reg, value: T) {
    if let Some(slot) = v.get_mut(index as usize) {
        *slot = value;
    }
}

fn flag(v: &[bool], index: Reg) -> bool {
    v.get(index as usize).copied().unwrap_or(false)
}
