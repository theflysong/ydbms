//! 表的抽象层。
//!
//! 本模块只描述「表」是什么，不关心它具体怎么存储。DBMS 中的种种操作都针对
//! 这里的抽象进行：
//!
//! * [`TabShape`] —— 描述表的**形状**：列的顺序、列名与类型。
//! * [`Table`] —— 描述表的**行为**：行数、行访问与变更操作。
//!
//! [`Table`] 以 [`TabShape`] 为 supertrait，因此拿到一个 `&dyn Table` 就同时
//! 拿到了它的形状，不需要额外再传一份 schema。
//!
//! # 为什么行访问返回 owned 的 [`Row`]
//!
//! [`Table::row`] 的签名是 `fn row(&self, r: usize) -> Option<Row>`，返回**拥有
//! 所有权**的行，而不是 `Option<&[Value]>`。这是为了让「连接表」这类**虚表**
//! 也能实现本 trait：连接的结果是查询时才计算出来的，没有一块现成的内存可以
//! 借用。物理表另有一套 inherent 的借用快路径（
//! [`PhysTab::row_ref`](crate::db::phys::PhysTab::row_ref) /
//! [`PhysTab::cell_ref`](crate::db::phys::PhysTab::cell_ref)）用来避免拷贝。
//!
//! # 为什么变更操作有默认实现
//!
//! [`Table::insert`] / [`Table::update`] / [`Table::delete`] 的默认实现一律返回
//! [`DbError::ReadOnly`]。物理表会覆盖它们；而连接表之类的只读表什么都不用写，
//! 默认就是只读的。
//!
//! # 随机访问 vs. 顺序拉取
//!
//! 本 trait 是**随机访问**的契约：`nrows()` + `row(r)`，问哪一行就给哪一行。
//! 执行器的中间结果要的是另一件事：**只进不退地一行一行拉**，那是
//! [`RowSource`](crate::db::source::RowSource)（见 [`crate::db::source`]）；
//! [`TableCursor`](crate::db::source::TableCursor) 把前者当后者用。
//!
//! 支持的类型见 [`DataType`]：`INT` / `TEXT` / `CHAR(N)` / `VARCHAR(N)` /
//! `TIMESTAMP(N)` / `NUMERIC(N,M)`。

use std::fmt;

use bigdecimal::{BigDecimal, RoundingMode, Zero};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// 值的“大类”：决定两个值能不能比较，以及能不能放进某列。
///
/// `CHAR(N)` / `VARCHAR(N)` / `TEXT` **同属 [`TypeFamily::Text`]**——它们在比较与
/// 存放上是同一件事，只差 typmod。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeFamily {
    /// 整数。
    Int,
    /// 文本：`TEXT` / `CHAR(N)` / `VARCHAR(N)`。
    Text,
    /// 时间戳。
    Timestamp,
    /// 十进制数。
    Numeric,
}

/// 裸 `TIMESTAMP` 的默认小数秒位数（与 PostgreSQL 一致）。
pub const DEFAULT_TIMESTAMP_PRECISION: u32 = 6;

/// 时间戳小数秒的最大位数。
pub const MAX_TIMESTAMP_PRECISION: u32 = 6;

/// 计算出来的 `NUMERIC` 表达式退化成这个宽默认。
///
/// 它**从不起强制作用**——typmod 只在往真实列 INSERT 时检查，而计算结果列是
/// 插不进去的。所以这里取一个宽值只是为了让 schema 有个具体标签。
/// 裸 `NUMERIC` 的两条上限，**照抄 PostgreSQL**（不是我们自己的取舍）：
/// 小数点前最多 131072 位、小数点后最多 16383 位。
///
/// 有上限是为了挡住 `'1e-999999999'::numeric` 这种会把内存吃光的输入；
/// 这两个数大到任何真实数据都碰不到。
pub const MAX_NUMERIC_INT_DIGITS: usize = 131_072;
/// 小数点后的位数上限（同上）。
pub const MAX_NUMERIC_FRAC_DIGITS: usize = 16_383;

impl fmt::Display for TypeFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeFamily::Int => f.write_str("INT"),
            TypeFamily::Text => f.write_str("TEXT"),
            TypeFamily::Timestamp => f.write_str("TIMESTAMP"),
            TypeFamily::Numeric => f.write_str("NUMERIC"),
        }
    }
}

impl TypeFamily {
    /// 大类 → 它的默认具体类型。计算出来的投影列用它。
    pub fn default_type(&self) -> DataType {
        match self {
            TypeFamily::Int => DataType::Int,
            TypeFamily::Text => DataType::Text,
            TypeFamily::Timestamp => DataType::Timestamp(DEFAULT_TIMESTAMP_PRECISION),
            // 计算结果不限精度与标度（与 PostgreSQL 的 `numeric` 一致）
            TypeFamily::Numeric => DataType::NumericAny,
        }
    }

    /// 跨大类比较时的稳定次序。同一列内不会发生，只是为了给出全序。
    fn rank(&self) -> u8 {
        match self {
            TypeFamily::Int => 0,
            TypeFamily::Numeric => 1,
            TypeFamily::Text => 2,
            TypeFamily::Timestamp => 3,
        }
    }
}

/// 列的数据类型。
///
/// 该类型会出现在持久化的模式文件里（见 [`crate::pers`]），因此派生了 serde 并按
/// `UPPERCASE` 序列化。`Int` 是单元变体，磁盘上写作 `"INT"`，与老文件兼容；
/// 带参数的写成 `{"CHAR":4}` / `{"NUMERIC":[10,2]}`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DataType {
    /// 32 位有符号整数。
    Int,
    /// 不定长文本。
    Text,
    /// `CHAR(N)` —— 语法树里叫 `bpchar`。
    Char(u32),
    /// `VARCHAR(N)`。
    VarChar(u32),
    /// `TIMESTAMP(N)`，N 是小数秒位数。
    Timestamp(u32),
    /// `NUMERIC(N, M)`：N 是总位数，M 是小数位。
    Numeric(u32, u32),
    /// 裸 `NUMERIC`：**精度与标度都不限**（任意精度十进制）。
    ///
    /// 计算出来的数值列（`AVG`、四则运算的结果、裸小数常量）都用它——这与
    /// PostgreSQL 一致：`avg(numeric)`、`numeric + numeric` 的结果都是不限定的
    /// `numeric`。
    #[serde(rename = "NUMERIC_ANY")]
    NumericAny,
}

impl DataType {
    /// 这个类型属于哪个大类。
    pub fn family(&self) -> TypeFamily {
        match self {
            DataType::Int => TypeFamily::Int,
            DataType::Text | DataType::Char(_) | DataType::VarChar(_) => TypeFamily::Text,
            DataType::Timestamp(_) => TypeFamily::Timestamp,
            DataType::Numeric(_, _) | DataType::NumericAny => TypeFamily::Numeric,
        }
    }

    /// 按这个类型解析一段文本，**并在这里强制 typmod**。
    ///
    /// INSERT 的常量、`WHERE name = 'x'` 里的字面量、CSV 读回，全都走这里，
    /// 于是长度/精度的检查只有一处。
    pub fn parse(&self, text: &str) -> Result<Value> {
        match self {
            DataType::Int => text
                .trim()
                .parse::<i32>()
                .map(Value::Int)
                .map_err(|e| self.bad_value(text, format!("不是合法的 INT：{}", e))),

            DataType::Text => Ok(Value::Text(text.to_string())),

            DataType::Char(n) | DataType::VarChar(n) => {
                let chars = text.chars().count();
                if chars > *n as usize {
                    return Err(self.bad_value(
                        text,
                        format!("长度 {} 超过上限 {}（按字符数算）", chars, n),
                    ));
                }
                Ok(Value::Text(text.to_string()))
            }

            DataType::Timestamp(precision) => {
                let canonical = canonical_timestamp(text)
                    .map_err(|reason| self.bad_value(text, reason))?;
                let digits = fraction_digits(text);
                if digits > *precision as usize {
                    return Err(self.bad_value(
                        text,
                        format!("小数秒 {} 位超过 TIMESTAMP({}) 的上限", digits, precision),
                    ));
                }
                Ok(Value::Timestamp(canonical))
            }

            DataType::NumericAny => {
                let canonical = canonical_big_decimal(text)
                    .map_err(|reason| self.bad_value(text, reason))?;
                let (_, int_part, frac_part) = split_decimal(&canonical);
                let int_digits = if int_part == "0" { 0 } else { int_part.len() };
                if int_digits > MAX_NUMERIC_INT_DIGITS {
                    return Err(self.bad_value(
                        text,
                        format!(
                            "整数位数 {} 超过 NUMERIC 的上限 {}",
                            int_digits, MAX_NUMERIC_INT_DIGITS
                        ),
                    ));
                }
                if frac_part.len() > MAX_NUMERIC_FRAC_DIGITS {
                    return Err(self.bad_value(
                        text,
                        format!(
                            "小数位 {} 超过 NUMERIC 的上限 {}",
                            frac_part.len(),
                            MAX_NUMERIC_FRAC_DIGITS
                        ),
                    ));
                }
                Ok(Value::Numeric(canonical))
            }

            DataType::Numeric(precision, scale) => {
                let canonical = canonical_big_decimal(text)
                    .map_err(|reason| self.bad_value(text, reason))?;
                let (_, int_part, frac_part) = split_decimal(&canonical);
                let int_digits = if int_part == "0" { 0 } else { int_part.len() };
                if frac_part.len() > *scale as usize {
                    return Err(self.bad_value(
                        text,
                        format!(
                            "小数位 {} 超过 NUMERIC({},{}) 的上限 {}",
                            frac_part.len(),
                            precision,
                            scale,
                            scale
                        ),
                    ));
                }
                if int_digits + frac_part.len() > *precision as usize {
                    return Err(self.bad_value(
                        text,
                        format!(
                            "总位数 {} 超过 NUMERIC({},{}) 的上限",
                            int_digits + frac_part.len(),
                            precision,
                            scale
                        ),
                    ));
                }
                Ok(Value::Numeric(canonical))
            }
        }
    }

    /// 这个值能不能放进这种列：大类要对上，typmod 也要过。
    pub fn accepts(&self, value: &Value) -> bool {
        // NULL 任何类型都收得下；能不能放由 `Column::not_null` 在 `validate` 里管
        if value.is_null() {
            return true;
        }
        if Some(self.family()) != value.family() {
            return false;
        }
        match value {
            // 上面已经为 NULL 提前返回了
            Value::Null => true,
            Value::Int(_) => true,
            Value::Timestamp(text) => {
                let (_, _, frac) = split_decimal(text);
                frac.len() <= timestamp_precision_of(self) as usize
            }
            Value::Numeric(text) => {
                let (_, int_part, frac_part) = split_decimal(text);
                let int_digits = if int_part == "0" { 0 } else { int_part.len() };
                match self {
                    DataType::Numeric(precision, scale) => {
                        frac_part.len() <= *scale as usize
                            && int_digits + frac_part.len() <= *precision as usize
                    }
                    // 不限精度，任何值都收得下
                    DataType::NumericAny => true,
                    _ => true,
                }
            }
            Value::Text(text) => match self {
                DataType::Char(n) | DataType::VarChar(n) => text.chars().count() <= *n as usize,
                _ => true,
            },
        }
    }

    /// 从 CSV 字段读回一个值。
    ///
    /// 顺序很重要：**先**看是不是 NULL 记法，**再**做反转义，最后才按类型解析。
    pub fn from_csv_field(&self, field: &str) -> Result<Value> {
        if field == "\\N" {
            return Ok(Value::Null);
        }

        // 反转义只作用于文本族；别的列上以 `\` 开头的东西本来就该解析失败
        let text = if self.family() == TypeFamily::Text {
            match field.strip_prefix('\\') {
                Some(rest) => rest.to_string(),
                None => field.to_string(),
            }
        } else {
            field.to_string()
        };

        self.parse(&text)
    }

    fn bad_value(&self, text: &str, reason: String) -> DbError {
        DbError::BadValue {
            column: None,
            ty: *self,
            value: text.to_string(),
            reason,
        }
    }
}

fn timestamp_precision_of(ty: &DataType) -> u32 {
    match ty {
        DataType::Timestamp(n) => *n,
        _ => MAX_TIMESTAMP_PRECISION,
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Int => f.write_str("INT"),
            DataType::Text => f.write_str("TEXT"),
            DataType::Char(n) => write!(f, "CHAR({})", n),
            DataType::VarChar(n) => write!(f, "VARCHAR({})", n),
            DataType::Timestamp(n) => write!(f, "TIMESTAMP({})", n),
            DataType::Numeric(p, s) => write!(f, "NUMERIC({},{})", p, s),
            DataType::NumericAny => f.write_str("NUMERIC"),
        }
    }
}

/// 一个单元格的值。
///
/// 刻意**没有**实现 `Copy`：`Text(String)` 之类本来就不能 `Copy`。
///
/// 实现了 `Hash`，于是能直接当哈希表的键（哈希连接与索引都用它）。它只要求
/// **与 `Eq` 一致**——`Eq` 是派生的按文本/内容相等，而 `Ord` 是手写的（`NUMERIC`
/// 按定点比、跨大类按大类排名）。两者不冲突：规范文本是不变式，相等的值文本也相等。
///
/// # 三种新值都用“规范化文本”存
///
/// * `Text` —— 原样。
/// * `Timestamp` —— 规范化成**定宽**的 `YYYY-MM-DD HH:MM:SS.ffffff`，于是**字典序
///   就等于时间序**，不需要任何日期算术。
/// * `Numeric` —— 规范化成十进制文本（去前导零、**去尾随零**、`-0` 变 `0`）。
///
/// > **不变量（很重要）**：`Value::Numeric` 的文本必须已经规范化。手写一个未
/// > 规范化的 `Value::Numeric("1.50".into())` 会让 `Ord`（数值比较）与 `PartialEq`
/// > （文本比较）**不一致**，从而违反 `Ord` 的契约。请一律用 [`DataType::parse`]
/// > 或 [`Value::numeric`] 构造。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    /// SQL 的 NULL。**它没有类型**——靠上下文定型（见 [`crate::ir`] 的
    /// `ScalarType::Null`）。
    Null,
    /// 一个有符号整数。
    Int(i32),
    /// 文本值：`TEXT` / `CHAR(N)` / `VARCHAR(N)`。**不补空格**（见模块文档的偏差）。
    Text(String),
    /// 时间戳，规范化的定宽文本。
    Timestamp(String),
    /// 十进制数，规范化的十进制文本。
    Numeric(String),
}

impl Value {
    /// 该值是不是 SQL 的 NULL。
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// 该值所属的大类；NULL 没有大类，返回 `None`。
    pub fn family(&self) -> Option<TypeFamily> {
        Some(match self {
            Value::Null => return None,
            Value::Int(_) => TypeFamily::Int,
            Value::Text(_) => TypeFamily::Text,
            Value::Timestamp(_) => TypeFamily::Timestamp,
            Value::Numeric(_) => TypeFamily::Numeric,
        })
    }

    /// 该值所属大类的默认具体类型；NULL 退化成 `TEXT`（PostgreSQL 里
    /// `SELECT NULL` 也是 text）。
    pub fn data_type(&self) -> DataType {
        self.family()
            .unwrap_or(TypeFamily::Text)
            .default_type()
    }

    /// 若是整数则取出其内容。
    pub fn as_int(&self) -> Option<i32> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// 安全地构造一个 `NUMERIC`（走规范化，保证 `Ord`/`PartialEq` 一致）。
    pub fn numeric(text: &str) -> Result<Value> {
        DataType::NumericAny.parse(text)
    }

    /// 安全地构造一个 `TIMESTAMP`。
    pub fn timestamp(text: &str) -> Result<Value> {
        DataType::Timestamp(DEFAULT_TIMESTAMP_PRECISION).parse(text)
    }

    /// 写成 CSV 字段。
    ///
    /// * NULL → `\N`（PostgreSQL COPY 的惯例）
    /// * **以 `\` 开头的文本 → 多写一个 `\` 转义**
    ///
    /// 有了这条转义，「NULL」「空字符串」「字面文本 `\N`」三者才**完全可区分**。
    /// 只有以反斜杠开头的文本会受影响。
    pub fn to_csv_field(&self) -> String {
        match self {
            Value::Null => "\\N".to_string(),
            Value::Text(text) if text.starts_with('\\') => format!("\\{}", text),
            other => other.to_string(),
        }
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Int(v) => write!(f, "{}", v),
            Value::Text(v) | Value::Timestamp(v) | Value::Numeric(v) => f.write_str(v),
        }
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            // NULL 排在最前。**排序时它在前还是在后由 `SortKey::nulls_first`
            // 决定**，不是这里——这里只要一个与 `Eq` 一致的全序。
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            (Value::Null, _) => std::cmp::Ordering::Less,
            (_, Value::Null) => std::cmp::Ordering::Greater,
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            // 定宽，所以字典序 == 时间序
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            // 字典序**是错的**（"10" < "9"），必须按十进制定点比
            (Value::Numeric(a), Value::Numeric(b)) => compare_decimal(a, b),
            // 跨大类：同一列内不会发生，给个稳定次序而已
            _ => {
                let left = self.family().map(|f| f.rank()).unwrap_or(0);
                let right = other.family().map(|f| f.rank()).unwrap_or(0);
                left.cmp(&right)
            }
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// 十进制运算的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecimalOp {
    /// 加
    Add,
    /// 减
    Sub,
    /// 乘
    Mul,
    /// 除
    Div,
}

/// 把十进制文本解析成 [`BigDecimal`] 并**规范化**回文本。
///
/// 用的是 `to_plain_string()` 而**不是** `to_string()`：后者在大数量级上会切成
/// 科学计数法（`1e50` → `"1e+50"`），而我们的规范文本必须是纯十进制——否则
/// `compare_decimal` 按整数部分长度比较的那套就全错了。
pub fn canonical_big_decimal(text: &str) -> std::result::Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("空的 NUMERIC".to_string());
    }
    let value = BigDecimal::from_str(trimmed)
        .map_err(|e| format!("`{}` 不是合法的十进制数：{}", trimmed, e))?;
    if value.is_zero() {
        return Ok("0".to_string());
    }
    Ok(value.normalized().to_plain_string())
}

/// 一个值对应的规范十进制文本。
fn decimal_text(value: &Value) -> Option<String> {
    match value {
        Value::Int(v) => Some(v.to_string()),
        Value::Numeric(text) => Some(text.clone()),
        _ => None,
    }
}

/// 最高有效位的**十进制指数**：`1.5` → 0、`2500` → 3、`0.001` → -3、`0` → 0。
fn msd_exponent(int_part: &str, frac_part: &str) -> i64 {
    if int_part != "0" {
        return int_part.len() as i64 - 1;
    }
    for (index, ch) in frac_part.chars().enumerate() {
        if ch != '0' {
            return -(index as i64) - 1;
        }
    }
    0
}

/// 一个数在 PostgreSQL 内部表示里的**领头数字组**（base-10000）。
///
/// PG 的 `select_div_scale` 要先比较两个操作数的领头组，比的是**组的数值**，
/// 不是单个十进制位——这是整条规则里最容易漏掉的一步。
///
/// 组的数值要**右对齐**填满这一组：`0.001` 的领头组是 `10`（不是 `1`），
/// 因为 1 落在 `10^-3` 这一位上，组里它下面还空着一位。
fn leading_group(int_part: &str, frac_part: &str) -> u32 {
    let msd = msd_exponent(int_part, frac_part);
    let weight = msd.div_euclid(4);
    // 这一组从最高有效位往下有几个十进制位（1..=4）
    let group_digits = (msd - weight * 4 + 1) as usize;

    // 从最高有效位往后的有效数字（规范文本里整数部分不会有前导零）
    let significant = if int_part == "0" {
        frac_part.trim_start_matches('0')
    } else {
        // 借一个临时串：整数部分与小数部分拼起来就是有效数字序列
        return leading_group_of(&format!("{}{}", int_part, frac_part), group_digits);
    };
    leading_group_of(significant, group_digits)
}

/// 见 [`leading_group`]：从有效数字序列里取领头组。
fn leading_group_of(significant: &str, group_digits: usize) -> u32 {
    let available = significant.len().min(group_digits);
    if available == 0 {
        return 0;
    }
    let head: u32 = significant[..available].parse().unwrap_or(0);
    // 组里剩下的低位补零，于是组的数值是右对齐的
    head * 10u32.pow((group_digits - available) as u32)
}

/// PostgreSQL `select_div_scale` 的规则：**除法要保留多少位小数**。
///
/// 大意是「至少 16 位有效数字」，再与两个操作数各自的小数位数取大，最后夹到
/// `[0, 1000]`。
///
/// 关键的一步是 `firstdigit1 < firstdigit2` 时把 `qweight` **再减一**：商的最高
/// 位组可能比「权重之差」少一组，商因此要多留 4 位小数。漏掉它会让结果**少 4 位**
/// （`1.0/3` 给 16 位而不是 20 位）。
///
/// > 下面这张对照表是用临时起的 PostgreSQL 18.6 实测出来的，不是照文档推的：
/// > `1.0/3`→20、`12345.0/7`→16、`100.0/28`→16、`1.0/300`→20、`1.0/0.3`→16。
fn division_scale(a: &Value, b: &Value) -> i64 {
    /// PG 的 `NUMERIC_MIN_SIG_DIGITS`
    const MIN_SIG_DIGITS: i64 = 16;
    /// PG 的 `NUMERIC_MAX_DISPLAY_SCALE`
    const MAX_DISPLAY_SCALE: i64 = 1000;

    let (ta, tb) = match (decimal_text(a), decimal_text(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => return MIN_SIG_DIGITS,
    };
    let (_, ai, af) = split_decimal(&ta);
    let (_, bi, bf) = split_decimal(&tb);

    // PG 内部按「四位一组」存，这里换算成组下标
    let weight = |int_part: &str, frac_part: &str| msd_exponent(int_part, frac_part).div_euclid(4);

    let qweight = if ai == "0" && af.is_empty() {
        // 被除数是 0，商就是 0——PG 单独处理这一支
        0
    } else {
        // 领头组更小 → 商的最高位组少一组
        let adjust = i64::from(leading_group(ai, af) < leading_group(bi, bf));
        weight(ai, af) - weight(bi, bf) - adjust
    };

    (MIN_SIG_DIGITS - qweight * 4)
        .max(af.len() as i64)
        .max(bf.len() as i64)
        .clamp(0, MAX_DISPLAY_SCALE)
}

impl Value {
    /// 转成 [`BigDecimal`]。只有 `Int` 与 `Numeric` 能转。
    fn to_decimal(&self) -> std::result::Result<BigDecimal, String> {
        match self {
            Value::Int(v) => Ok(BigDecimal::from(*v)),
            Value::Numeric(text) => BigDecimal::from_str(text)
                .map_err(|e| format!("`{}` 不是合法的十进制数：{}", text, e)),
            other => Err(format!("`{}` 不能参与十进制运算", other)),
        }
    }

    /// 十进制加减乘除。任一侧为 NULL 时调用方应当先拦掉。
    pub fn decimal(&self, op: DecimalOp, other: &Value) -> std::result::Result<Value, String> {
        let a = self.to_decimal()?;
        let b = other.to_decimal()?;

        let result = match op {
            DecimalOp::Add => a + b,
            DecimalOp::Sub => a - b,
            DecimalOp::Mul => a * b,
            DecimalOp::Div => {
                // **必须**自己先拦：bigdecimal 的 `Div` 遇到零除数是 `panic!`
                if b.is_zero() {
                    return Err("除以零".to_string());
                }
                let scale = division_scale(self, other);
                (a / b).with_scale_round(scale, RoundingMode::HalfUp)
            }
        };

        Ok(Value::Numeric(canonical_big_decimal(&result.to_plain_string())?))
    }

    /// 四舍五入到 `digits` 位小数，**远离零**（与 PostgreSQL 的 `round(numeric)` 一致）。
    ///
    /// `digits` 可以是负数，表示舍到十位、百位——`ROUND(1234.5, -2)` → `1200`。
    pub fn round_to(&self, digits: i32) -> std::result::Result<Value, String> {
        /// 位数绝对值上限，挡住 `ROUND(x, 1000000)` 这种会把内存铺满的输入。
        const MAX_DIGITS: i32 = 1000;
        if digits.abs() > MAX_DIGITS {
            return Err(format!("ROUND 的位数超出 ±{} 的范围", MAX_DIGITS));
        }

        let value = self.to_decimal()?;
        let rounded = value.with_scale_round(digits as i64, RoundingMode::HalfUp);
        Ok(Value::Numeric(canonical_big_decimal(&rounded.to_plain_string())?))
    }
}


/// 比较两个规范化十进制文本。
///
/// 规范化保证「数值相等 ⟺ 文本相等」，所以这个顺序与 `PartialEq` 一致，
/// `Ord` 的契约成立。
fn compare_decimal(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (neg_a, ia, fa) = split_decimal(a);
    let (neg_b, ib, fb) = split_decimal(b);
    match (neg_a, neg_b) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => compare_magnitude((ia, fa), (ib, fb)),
        (true, true) => compare_magnitude((ib, fb), (ia, fa)),
    }
}

/// 比较两个**非负**十进制数的大小。返回 `(整数部分, 小数部分)`。
fn compare_magnitude(
    (ia, fa): (&str, &str),
    (ib, fb): (&str, &str),
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let ia = ia.trim_start_matches('0');
    let ib = ib.trim_start_matches('0');

    // 整数部分位数多的更大（前导零已去掉）
    match ia.len().cmp(&ib.len()) {
        Ordering::Equal => match ia.cmp(ib) {
            Ordering::Equal => {}
            other => return other,
        },
        other => return other,
    }

    // 小数部分逐位比，短的补 0
    let width = fa.len().max(fb.len());
    let mut left = fa.bytes().chain(std::iter::repeat(b'0'));
    let mut right = fb.bytes().chain(std::iter::repeat(b'0'));
    for _ in 0..width {
        match left.next().cmp(&right.next()) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// 把十进制文本拆成 `(是否负数, 整数部分, 小数部分)`。
fn split_decimal(text: &str) -> (bool, &str, &str) {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    match rest.split_once('.') {
        Some((int_part, frac_part)) => (negative, int_part, frac_part),
        None => (negative, rest, ""),
    }
}

/// 小数部分的位数（用于 `TIMESTAMP(N)` 检查，在规范化之前数）。
fn fraction_digits(text: &str) -> usize {
    let trimmed = text.trim();
    match trimmed.split_once('.') {
        Some((_, frac)) => frac.chars().filter(|c| c.is_ascii_digit()).count(),
        None => 0,
    }
}

/// 把时间戳文本规范化成定宽（16 + 7 = 23 字符）的
/// `YYYY-MM-DD HH:MM:SS.ffffff`。
///
/// 定宽是关键：有了它，字典序就等于时间序，**完全不需要日期算术**。
///
/// 已知不做的事：不校验月长与闰年（`2024-02-30` 会被接受）——那需要真正的日历
/// 计算，而本版没有任何日期运算要支持。
fn canonical_timestamp(text: &str) -> std::result::Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("空的时间戳".to_string());
    }

    // 日期与时间之间允许 'T' 或空格
    let (date_part, time_part) = match trimmed.split_once(['T', 't', ' ']) {
        Some((d, t)) => (d, t.trim()),
        None => (trimmed, ""),
    };

    let date: Vec<&str> = date_part.split('-').collect();
    if date.len() != 3 {
        return Err(format!("`{}` 不是 YYYY-MM-DD 形式", text));
    }
    let year = parse_digits(date[0], "年")?;
    let month = parse_digits(date[1], "月")?;
    let day = parse_digits(date[2], "日")?;

    if !(1..=12).contains(&month) {
        return Err(format!("月份 {} 越界", month));
    }
    if !(1..=31).contains(&day) {
        return Err(format!("日 {} 越界", day));
    }
    // `year` 是 u32，只可能上溢——`year < 0` 是永假的比较（编译器会指出来）
    if year > 9999 {
        return Err(format!(
            "年份 {} 越界（只支持 0..=9999，定宽才保证字典序等于时间序）",
            year
        ));
    }

    let (hour, minute, second, frac) = if time_part.is_empty() {
        (0u32, 0u32, 0u32, String::new())
    } else {
        let (hms, frac) = match time_part.split_once('.') {
            Some((hms, f)) => (hms, f.to_string()),
            None => (time_part, String::new()),
        };
        let parts: Vec<&str> = hms.split(':').collect();
        if parts.len() != 3 {
            return Err(format!("`{}` 的时间部分不是 HH:MM:SS", text));
        }
        (
            parse_digits(parts[0], "时")?,
            parse_digits(parts[1], "分")?,
            parse_digits(parts[2], "秒")?,
            frac,
        )
    };

    if hour > 23 {
        return Err(format!("小时 {} 越界", hour));
    }
    if minute > 59 {
        return Err(format!("分钟 {} 越界", minute));
    }
    if second > 59 {
        return Err(format!("秒 {} 越界", second));
    }
    if !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("`{}` 的小数秒里有非数字", text));
    }
    if frac.len() > MAX_TIMESTAMP_PRECISION as usize {
        return Err(format!(
            "小数秒 {} 位超过上限 {}",
            frac.len(),
            MAX_TIMESTAMP_PRECISION
        ));
    }

    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:0<width$}",
        year,
        month,
        day,
        hour,
        minute,
        second,
        frac,
        width = MAX_TIMESTAMP_PRECISION as usize
    ))
}

/// 把一个纯数字段解析成 `u32`（允许前导零，长度不限）。
fn parse_digits(text: &str, what: &str) -> std::result::Result<u32, String> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{} 不是数字：`{}`", what, text));
    }
    text.parse::<u32>()
        .map_err(|e| format!("{} 越界：`{}`（{}）", what, text, e))
}


/// 一行数据，按列的物理顺序排列。
pub type Row = Vec<Value>;

/// 一列的定义：列名 + 类型。
///
/// 派生 serde 是为了让「列定义」能整体写进持久化的模式文件；这同时意味着
/// **字段名 `name` / `ty` 属于磁盘契约**，改名会改变文件格式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    /// 列名。
    pub name: String,
    /// 列的数据类型。
    pub ty: DataType,
    /// 列声明了 `NOT NULL`。**这是真的会强制**的（在 `Table::validate` 里）。
    ///
    /// 老模式文件里没有这个键，`#[serde(default)]` 会读成 `false`。
    #[serde(default)]
    pub not_null: bool,
}

impl Column {
    /// 构造一列（可空）。
    pub fn new(name: impl Into<String>, ty: DataType) -> Self {
        Column {
            name: name.into(),
            ty,
            not_null: false,
        }
    }

    /// 构造一个 `NOT NULL` 的列。
    pub fn not_null(name: impl Into<String>, ty: DataType) -> Self {
        Column {
            name: name.into(),
            ty,
            not_null: true,
        }
    }

    /// 列名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 列的数据类型。
    pub fn ty(&self) -> DataType {
        self.ty
    }
}

impl fmt::Display for Column {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.ty)?;
        if self.not_null {
            f.write_str(" NOT NULL")?;
        }
        Ok(())
    }
}

/// 表的形状：一根有序的列定义列表。
///
/// 这是「表长什么样」的最小抽象。物理表的形状、连接表的形状都可以用它描述；
/// 物理表正是由某个 `TabShape` 生成出来的。
pub trait TabShape {
    /// 按物理顺序返回所有列定义。
    ///
    /// 这是本 trait 唯一必需实现的方法，其余方法都由它推导而来。
    fn columns(&self) -> &[Column];

    /// 列数（表的元数）。
    fn arity(&self) -> usize {
        self.columns().len()
    }

    /// 按序号取列定义；越界返回 `None`。
    fn column(&self, i: usize) -> Option<&Column> {
        self.columns().get(i)
    }

    /// 按列名查列序号；列不存在返回 `None`。
    fn index_of(&self, name: &str) -> Option<usize> {
        self.columns().iter().position(|c| c.name == name)
    }
}

/// [`TabShape`] 的通用实现：把自己拥有的一份列定义当作形状。
///
/// [`ColumnsShape::push`] 让拼接形状变得容易，将来
/// `JoinTab` 的形状就是「左表的列 ++ 右表的列」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnsShape {
    columns: Vec<Column>,
}

impl ColumnsShape {
    /// 用一份列定义构造形状。
    pub fn new(columns: Vec<Column>) -> Self {
        ColumnsShape { columns }
    }

    /// 在末尾追加一列。
    pub fn push(&mut self, col: Column) {
        self.columns.push(col);
    }
}

impl TabShape for ColumnsShape {
    fn columns(&self) -> &[Column] {
        &self.columns
    }
}

impl From<Vec<Column>> for ColumnsShape {
    fn from(columns: Vec<Column>) -> Self {
        ColumnsShape::new(columns)
    }
}

/// 表操作可能出现的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// 插入的行列数与该表的元数不一致。
    ArityMismatch {
        /// 表的元数。
        expected: usize,
        /// 实际给出的列数。
        got: usize,
    },
    /// 某个值的类型与该列声明的类型不一致。
    ///
    /// 目前 [`DataType`] 只有 `Int`、[`Value`] 只有 `Int`，因此这个分支暂时不可达；
    /// 校验逻辑仍然按通用方式实现，等加入第二种类型后即可生效。
    TypeMismatch {
        /// 出错的列名。
        column: String,
        /// 该列声明的类型。
        expected: DataType,
        /// 实际给出的类型。
        got: DataType,
    },
    /// 行号越界。
    RowOutOfBounds {
        /// 请求的行号。
        index: usize,
        /// 当前行数。
        nrows: usize,
    },
    /// 列号越界。
    ColumnOutOfBounds {
        /// 请求的列号。
        index: usize,
        /// 当前元数。
        arity: usize,
    },
    /// 对只读表发起了变更操作。
    ReadOnly {
        /// 表的名称。
        table: String,
    },
    /// 持久化时读写文件失败。
    ///
    /// 这里存的是 [`std::io::ErrorKind`] 与文本，而不是 [`std::io::Error`]：
    /// 后者既不是 `Clone` 也不是 `PartialEq`，放进来会让 `DbError` 失去这两个
    /// 能力（现有代码大量用 `assert_eq!` 比较错误）。
    Io {
        /// 出错的文件路径。
        path: String,
        /// I/O 错误的类别。
        kind: std::io::ErrorKind,
        /// 错误的文字描述。
        message: String,
    },
    /// 持久化文件的内容不合法。
    ///
    /// 覆盖：模式文件不是合法 JSON、模式版本不认识、CSV 表头与模式对不上、
    /// 字段数不齐、整数解析失败等。
    Format {
        /// 出错的文件路径。
        path: String,
        /// 出错的行号（1 起，含表头行）；不适用时为 `None`。
        line: Option<usize>,
        /// 具体原因。
        message: String,
    },
    /// 数据文件在，但配套的模式文件缺失。
    MissingSchema {
        /// 期望存在的模式文件路径。
        path: String,
    },
    /// 符号表里查不到这个名字。
    UnknownSymbol {
        /// 被引用的名字。
        name: String,
    },
    /// 符号表里已经有这个名字了。
    DuplicateSymbol {
        /// 重复的名字。
        name: String,
    },
    /// 线性 IR 的**静态校验**失败。
    ///
    /// 覆盖：寄存器先使用后定义、寄存器/列下标越界、类型不符、输入个数不对、
    /// 输入下标违反拓扑序等。这些都是「IR 本身写错了」，与数据无关。
    IrInvalid {
        /// 出错的节点下标；与具体节点无关时为 `None`。
        node: Option<usize>,
        /// 具体原因。
        message: String,
    },
    /// 线性 IR 的**运行期**失败：除零、整数溢出。
    IrRuntime {
        /// 出错时正在执行的节点下标；不属于任何计划节点时为 `None`
        /// （例如 `INSERT` 里的值表达式）。
        node: Option<usize>,
        /// 具体原因。
        message: String,
    },
    /// SQL 解析失败。
    Parse {
        /// 解析器给出的描述。
        message: String,
    },
    /// 语法合法，但本版不支持这种构造。
    ///
    /// 一律显式拒绝、**绝不猜**：把不认识的写法当作别的东西执行，比报错危险得多。
    Unsupported {
        /// 不支持的构造。
        feature: String,
    },
    /// 找不到这一列。
    UnknownColumn {
        /// 被引用的列名。
        name: String,
        /// 当时可见的列名。
        available: Vec<String>,
    },
    /// 列名有歧义。
    AmbiguousColumn {
        /// 有歧义的列名。
        name: String,
        /// 候选的限定名。
        candidates: Vec<String>,
    },
    /// 一个值无法被解释成目标类型，或超出了该类型的 typmod 限制。
    ///
    /// 覆盖：`VARCHAR(3)` 插 4 个字符、`NUMERIC(4,2)` 插 `1234.56`、
    /// `TIMESTAMP` 文本解析失败等。
    BadValue {
        /// 出问题的列名；上下文不知道列名时为 `None`。
        column: Option<String>,
        /// 目标类型。
        ty: DataType,
        /// 出问题的原始值。
        value: String,
        /// 具体原因。
        reason: String,
    },
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::ArityMismatch { expected, got } => write!(
                f,
                "column count mismatch: expected {}, got {}",
                expected, got
            ),
            DbError::TypeMismatch {
                column,
                expected,
                got,
            } => write!(
                f,
                "type mismatch for column `{}`: expected {}, got {}",
                column, expected, got
            ),
            DbError::RowOutOfBounds { index, nrows } => {
                write!(f, "row {} out of bounds (table has {} rows)", index, nrows)
            }
            DbError::ColumnOutOfBounds { index, arity } => write!(
                f,
                "column {} out of bounds (table has {} columns)",
                index, arity
            ),
            DbError::ReadOnly { table } => write!(f, "table `{}` is read-only", table),
            DbError::Io {
                path,
                kind,
                message,
            } => write!(f, "i/o error ({}) on {}: {}", kind, path, message),
            DbError::Format {
                path,
                line,
                message,
            } => match line {
                Some(line) => write!(f, "malformed data at {}:{}: {}", path, line, message),
                None => write!(f, "malformed data at {}: {}", path, message),
            },
            DbError::MissingSchema { path } => write!(f, "missing schema file: {}", path),
            DbError::UnknownSymbol { name } => write!(f, "unknown symbol: {}", name),
            DbError::DuplicateSymbol { name } => write!(f, "symbol already defined: {}", name),
            DbError::IrInvalid { node, message } => match node {
                Some(node) => write!(f, "invalid IR at node {}: {}", node, message),
                None => write!(f, "invalid IR: {}", message),
            },
            DbError::IrRuntime { node, message } => match node {
                Some(node) => write!(f, "IR runtime error at node {}: {}", node, message),
                None => write!(f, "IR runtime error: {}", message),
            },
            DbError::Parse { message } => write!(f, "SQL parse error: {}", message),
            DbError::Unsupported { feature } => write!(f, "unsupported: {}", feature),
            DbError::UnknownColumn { name, available } => write!(
                f,
                "unknown column `{}` (available: {})",
                name,
                available.join(", ")
            ),
            DbError::AmbiguousColumn { name, candidates } => write!(
                f,
                "ambiguous column `{}` (candidates: {})",
                name,
                candidates.join(", ")
            ),
            DbError::BadValue {
                column,
                ty,
                value,
                reason,
            } => match column {
                Some(column) => write!(
                    f,
                    "value `{}` does not fit column `{}` {}: {}",
                    value, column, ty, reason
                ),
                None => write!(f, "value `{}` does not fit {}: {}", value, ty, reason),
            },
        }
    }
}

impl std::error::Error for DbError {}

/// 本 crate 通用的 [`Result`](std::result::Result) 别名。
pub type Result<T> = std::result::Result<T, DbError>;

/// 表的抽象行为。
///
/// 所有针对表的操作都应该写在本 trait 之上。实现者至少要给出
/// [`name`](Table::name)、[`nrows`](Table::nrows) 与 [`row`](Table::row)；
/// 其余方法都有默认实现。
///
/// 本 trait 是 dyn-compatible 的：可以写成 `&dyn Table` 或 `Box<dyn Table>`，
/// 于是目录（catalog）里能放一个异构的表集合。
pub trait Table: TabShape {
    /// 表的名称。
    fn name(&self) -> &str;

    /// 表中的行数。
    fn nrows(&self) -> usize;

    /// 取出第 `r` 行的副本；越界返回 `None`。
    fn row(&self, r: usize) -> Option<Row>;

    /// 取出第 `r` 行第 `c` 列的值；越界返回 `None`。
    fn value(&self, r: usize, c: usize) -> Option<Value> {
        self.row(r)?.into_iter().nth(c)
    }

    /// 顺序遍历所有行。默认逐行调用 [`row`](Table::row)；
    /// 具体实现可以在需要时提供更高效的版本。
    fn iter_rows(&self) -> Box<dyn Iterator<Item = Row> + '_> {
        Box::new((0..self.nrows()).filter_map(move |i| self.row(i)))
    }

    /// 校验一行数据是否符合本表的形状（元数 + 逐列类型）。
    fn validate(&self, row: &Row) -> Result<()> {
        let expected = self.arity();
        if row.len() != expected {
            return Err(DbError::ArityMismatch {
                expected,
                got: row.len(),
            });
        }
        for (col, value) in self.columns().iter().zip(row.iter()) {
            // NULL 任何类型都收得下——除非列声明了 NOT NULL
            if value.is_null() {
                if col.not_null {
                    return Err(DbError::BadValue {
                        column: Some(col.name.clone()),
                        ty: col.ty,
                        value: "NULL".to_string(),
                        reason: "列声明了 NOT NULL，不接受 NULL".to_string(),
                    });
                }
                continue;
            }

            // 大类必须对上
            if Some(col.ty.family()) != value.family() {
                return Err(DbError::TypeMismatch {
                    column: col.name.clone(),
                    expected: col.ty,
                    got: value.data_type(),
                });
            }
            // typmod 也要过——这是 INSERT 唯一的强制入口
            if !col.ty.accepts(value) {
                return Err(DbError::BadValue {
                    column: Some(col.name.clone()),
                    ty: col.ty,
                    value: value.to_string(),
                    reason: format!("不符合 {} 的长度/精度限制", col.ty),
                });
            }
        }
        Ok(())
    }

    /// 在末尾追加一行，返回新行的序号。
    ///
    /// 默认实现拒绝写入（表是只读的）。
    fn insert(&mut self, _row: Row) -> Result<usize> {
        Err(DbError::ReadOnly {
            table: self.name().to_string(),
        })
    }

    /// 修改第 `r` 行第 `c` 列的值。
    ///
    /// 默认实现拒绝写入（表是只读的）。
    fn update(&mut self, _r: usize, _c: usize, _value: Value) -> Result<()> {
        Err(DbError::ReadOnly {
            table: self.name().to_string(),
        })
    }

    /// 删除第 `r` 行。
    ///
    /// 默认实现拒绝写入（表是只读的）。
    fn delete(&mut self, _r: usize) -> Result<()> {
        Err(DbError::ReadOnly {
            table: self.name().to_string(),
        })
    }
}
