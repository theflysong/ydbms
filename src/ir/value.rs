//! IR 的**值层**：标量、真值、以及它们的类型标签。
//!
//! 三样东西，都是表达式层与执行层共用的词汇：
//!
//! * [`Scalar`] —— 一个寄存器里装的东西：要么是**可存储的值**
//!   （[`Value`]，直接复用 `db` 的值模型，不再抄一份），要么是 SQL 的**真值**；
//! * [`Tri`] —— 三值逻辑的真值。谓词的结果是它，不是 `bool`：
//!   `1 = NULL` 不是 false，是 `Unknown`；
//! * [`ScalarType`] —— 标量的静态类型。`Value(_)` 是可存储的大类、`Null` 是
//!   **无类型 NULL**、`Bool` 是 IR 私有的谓词类型。
//!
//! # 为什么 `Value` 不在这里
//!
//! [`Value`] 是**存储层**的值：`DataType::parse` 产出它、CSV 读写它、`PhysTab`
//! 存它、`Table` 的契约围着它转。它住在 [`crate::db::table`]，因为 `db` 是
//! **被依赖、不依赖别人**的一层（见 `src/db/AGENTS.md`）——把它搬进 `ir` 会让
//! 存储层反过来依赖 IR 层。这里只装 IR 自己的那一层：真值、标量包装、类型标签。
//!
//! **真值与 `ScalarType::Bool` 都不进 `db`**：它们不能作结果列，也不进磁盘格式。

use std::fmt;

use crate::db::table::{TypeFamily, Value};

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
