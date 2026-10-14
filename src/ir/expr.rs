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

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;

use crate::db::table::{Column, DecimalOp, DbError, Result, TypeFamily, Value};

/// 寄存器号。一个程序用到的寄存器是 `0..nregs`。
pub type Reg = u16;

/// SQL 的**三值逻辑**真值。谓词的结果是它，不是 `bool`：
/// `1 = NULL` 不是 false，是 `Unknown`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tri {
    /// 真。
    True,
    /// 假。
    False,
    /// 未知（SQL 的 NULL 谓词结果）。`WHERE` 会把它丢掉。
    Unknown,
}

impl Tri {
    /// `NOT`。
    pub fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }

    /// Kleene 逻辑的 `AND`：有一侧是 False 就是 False。
    pub fn and(self, other: Tri) -> Tri {
        match (self, other) {
            (Tri::False, _) | (_, Tri::False) => Tri::False,
            (Tri::True, Tri::True) => Tri::True,
            _ => Tri::Unknown,
        }
    }

    /// Kleene 逻辑的 `OR`：有一侧是 True 就是 True。
    pub fn or(self, other: Tri) -> Tri {
        match (self, other) {
            (Tri::True, _) | (_, Tri::True) => Tri::True,
            (Tri::False, Tri::False) => Tri::False,
            _ => Tri::Unknown,
        }
    }

    /// 是不是确定的真。`WHERE` / `HAVING` / `CASE` 都只认这个。
    pub fn is_true(self) -> bool {
        matches!(self, Tri::True)
    }
}

impl fmt::Display for Tri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tri::True => f.write_str("true"),
            Tri::False => f.write_str("false"),
            Tri::Unknown => f.write_str("NULL"),
        }
    }
}

/// IR 标量。
///
/// 可存储的值**直接复用 [`Value`]**，不再抄一份；`Truth` 是 IR 私有的谓词类型，
/// **不进** [`DataType`](crate::db::table::DataType)、也**不进**[`Value`] 与磁盘格式。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scalar {
    /// 一个可存储的值（NULL / INT / TEXT / CHAR / VARCHAR / TIMESTAMP / NUMERIC）。
    Value(Value),
    /// 三值逻辑真值。
    Truth(Tri),
}

/// 标量的类型。
///
/// `Value(_)` 是可存储的大类；`Null` 是**无类型 NULL**；`Bool` 是 IR 私有的谓词
/// 类型。后两者都明确地被隔离在 [`Value`] 之外。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarType {
    /// 某个可存储的大类。
    Value(TypeFamily),
    /// 无类型的 NULL。比较时与任何 `Value(_)` 相容；投影时退化成 `TEXT`。
    Null,
    /// 谓词的真值（IR 私有）。
    Bool,
}

impl fmt::Display for ScalarType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScalarType::Value(family) => write!(f, "{}", family),
            ScalarType::Null => f.write_str("NULL"),
            ScalarType::Bool => f.write_str("BOOL"),
        }
    }
}

impl Scalar {
    /// 该值的类型。
    pub fn ty(&self) -> ScalarType {
        match self {
            Scalar::Value(Value::Null) => ScalarType::Null,
            Scalar::Value(v) => match v.family() {
                Some(family) => ScalarType::Value(family),
                None => ScalarType::Null,
            },
            Scalar::Truth(_) => ScalarType::Bool,
        }
    }

    /// 取出整数内容。
    pub fn as_int(&self) -> Option<i32> {
        match self {
            Scalar::Value(v) => v.as_int(),
            Scalar::Truth(_) => None,
        }
    }

    /// 取出三值真值。
    pub fn as_truth(&self) -> Option<Tri> {
        match self {
            Scalar::Truth(t) => Some(*t),
            Scalar::Value(_) => None,
        }
    }

    /// 是不是 NULL。
    pub fn is_null(&self) -> bool {
        matches!(self, Scalar::Value(Value::Null))
    }

    /// 转成能进结果表的 [`Value`]；真值**进不了**结果表，返回 `None`。
    pub fn to_value(&self) -> Option<Value> {
        match self {
            Scalar::Value(v) => Some(v.clone()),
            Scalar::Truth(_) => None,
        }
    }
}

impl fmt::Display for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scalar::Value(v) => write!(f, "{}", v),
            Scalar::Truth(t) => write!(f, "{}", t),
        }
    }
}

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
        }
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

/// 标量函数。目前只登记了 `ROUND`，要加新函数就在 `apply_call` 里加一个分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    /// `ROUND(numeric, digits)`。
    Round,
}

impl ScalarFunc {
    /// 渲染用的名字。
    pub fn name(&self) -> &'static str {
        match self {
            ScalarFunc::Round => "ROUND",
        }
    }

    /// 需要几个参数。
    pub fn arity(&self) -> usize {
        match self {
            ScalarFunc::Round => 2,
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
    /// `regs[dst] = if regs[cond] is TRUE { regs[then_value] } else { regs[else_value] }`。
    ///
    /// 走 SQL 的 `CASE` 语义：条件**不是 TRUE**（含 `Unknown`）就取 else 分支。
    ///
    /// > 已知差异：寄存器机没有跳转，所以**两个分支都会先被算出来**。死分支里的
    /// > 算术错误（例如除零）会冒出来，而 PostgreSQL 是惰性的。
    Case {
        /// 目的寄存器。
        dst: Reg,
        /// 条件寄存器（真值）。
        cond: Reg,
        /// 条件为 TRUE 时取它。
        then_value: Reg,
        /// 否则取它。
        else_value: Reg,
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
        }
    }

    /// 补上节点下标，转成本 crate 的统一错误类型。
    ///
    /// 传 `None` 表示这个程序不属于任何计划节点（例如 `INSERT` 里的值表达式）。
    pub fn into_db_error(self, node: Option<usize>) -> DbError {
        DbError::IrRuntime {
            node,
            message: self.message(),
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
                    then_value,
                    else_value,
                } => {
                    require(read_reg(&defined, *cond, at)?, ScalarType::Bool, at, "CASE 的条件")?;
                    let then_ty = read_reg(&defined, *then_value, at)?;
                    let else_ty = read_reg(&defined, *else_value, at)?;
                    write_reg(&mut defined, *dst, unify_branches(then_ty, else_ty, at)?, at)?;
                }
                Step::Call { dst, func, args } => {
                    let mut types = Vec::with_capacity(args.len());
                    for reg in args {
                        types.push(read_reg(&defined, *reg, at)?);
                    }
                    write_reg(&mut defined, *dst, call_type(*func, &types, at)?, at)?;
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
    pub fn eval(&self, row: &[Value]) -> std::result::Result<Scalar, EvalError> {
        self.eval_split(row, &[])
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
                    then_value,
                    else_value,
                } => {
                    let take_then = get(&regs, *cond, self.nregs)?
                        .as_truth()
                        .ok_or(EvalError::NotABool)?
                        .is_true();
                    // 两个分支都已经算好了，另一侧的类型是现成的（放大要用它）
                    let (picked, other) = if take_then {
                        (
                            get(&regs, *then_value, self.nregs)?,
                            get(&regs, *else_value, self.nregs)?,
                        )
                    } else {
                        (
                            get(&regs, *else_value, self.nregs)?,
                            get(&regs, *then_value, self.nregs)?,
                        )
                    };
                    let picked = widen_branches(picked, &other);
                    put(&mut regs, *dst, picked, self.nregs)?;
                }
                Step::Call { dst, func, args } => {
                    let mut values = Vec::with_capacity(args.len());
                    for reg in args {
                        values.push(get(&regs, *reg, self.nregs)?);
                    }
                    let v = apply_call(*func, &values)?;
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

    /// 这个程序引用到的所有输入列下标。将来做谓词下推分派时要用。
    pub fn columns(&self) -> BTreeSet<usize> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                Step::Col { index, .. } => Some(*index),
                _ => None,
            })
            .collect()
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
                    then_value,
                    else_value,
                } => {
                    let c = at(&texts, *cond);
                    let t = at(&texts, *then_value);
                    let e = at(&texts, *else_value);
                    set(
                        &mut texts,
                        *dst,
                        format!("CASE WHEN {} THEN {} ELSE {} END", c, t, e),
                    );
                    set(&mut compound, *dst, false);
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

    /// `CASE`：条件为 TRUE 取 `then_value`，否则取 `else_value`。
    pub fn case(&mut self, cond: Reg, then_value: Reg, else_value: Reg) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Case {
            dst,
            cond,
            then_value,
            else_value,
        });
        dst
    }

    /// 标量函数调用。
    pub fn call(&mut self, func: ScalarFunc, args: Vec<Reg>) -> Reg {
        let dst = self.fresh();
        self.steps.push(Step::Call { dst, func, args });
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

fn apply(op: BinOp, l: Scalar, r: Scalar) -> std::result::Result<Scalar, EvalError> {
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

/// 「整数」这个类型，写起来短一点。
fn int_type() -> ScalarType {
    ScalarType::Value(TypeFamily::Int)
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

/// `CASE` 取到的那一侧要按另一侧的类型**真的转换**。
///
/// 只改 [`unify_branches`]（类型检查）是不够的：`CASE WHEN true THEN 1 ELSE 1.5 END`
/// 的列类型成了 `NUMERIC`，而取到的值还是 `Value::Int`，物化时会以 `TypeMismatch`
/// 报错。这与比较侧的 [`widen_numeric`] 是同一条规矩——类型放行之后，运行时也得真转。
///
/// 两个分支都已经算好了（见 [`Step::Case`] 的已知差异），所以另一侧的类型是现成的。
fn widen_branches(picked: Scalar, other: &Scalar) -> Scalar {
    match (&picked, other) {
        (Scalar::Value(Value::Int(i)), Scalar::Value(Value::Numeric(_))) => {
            Scalar::Value(Value::Numeric(i.to_string()))
        }
        _ => picked,
    }
}

/// 标量函数的参数与返回类型。
fn call_type(func: ScalarFunc, args: &[ScalarType], at: usize) -> Result<ScalarType> {
    if args.len() != func.arity() {
        return Err(invalid(format!(
            "step {}: {} 需要 {} 个参数，实际给了 {} 个",
            at,
            func.name(),
            func.arity(),
            args.len()
        )));
    }

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
    }
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
