# AGENTS.md —— `src/db`

## 职能

数据库核心的**值模型与表抽象**。这一层不认识 SQL，也不认识计划：

- `table.rs` —— [`DataType`](table.rs) / [`Value`](table.rs) / [`Column`](table.rs) /
  [`Table`](table.rs) / [`TabShape`](table.rs) / [`DbError`](table.rs)，
  以及十进制算术与 CSV 字段读写。
- `phys.rs` —— 物化表 [`PhysTab`](phys.rs)。
- `source.rs` —— 拉取式的中间结果 [`RowSource`](source.rs) / 基表游标
  [`TableCursor`](source.rs) / 物化 [`materialize`](source.rs) 与抽干 [`drain`](source.rs)。

**上游**：`pers/` 通过 `&dyn Table` 落盘；`ir/` 把 `Value` 当标量用、把 `RowSource`
当执行管道的每一节。这一层**被依赖，不依赖别人**，请保持这个方向。

## 特殊规则

### 两个契约，不要混

- **`Table` 是随机访问**（`nrows()` + `row(r)`）：基表与落盘要它。
- **`RowSource` 是只进不退**（`next()` + `error()`）：执行器要它。连接、哈希连接、
  索引扫描将来都是"又一个 `RowSource`"，**不需要**再有一个 `JoinTab: Table`。

**物化只发生在 `materialize`/`drain` 的调用点**（`Sort`/`Aggregate`、连接的内侧、
被多个父节点引用的节点、最终结果）。别为了图方便在别的算子里攒行——拉取式的收益
全在"上层停手，下层就不算"上。

`next()` 返回 `None` 有两个可能：**正常结束**与**出错**，后者必须能通过 `error()`
问到。一旦结束或出错，后续 `next()` 要继续返回 `None`（幂等）。

### 不变式

- **规范文本是不变式，不是风格。** `Value::Numeric` 去掉尾随零、`Timestamp` 定宽
  文本。原因是 `Ord` 与 `PartialEq` 必须一致——一旦同一个值有两种文本表示，
  `Ord` 说相等而 `PartialEq` 说不等，排序与 `GROUP BY` 就会出错。
- **NULL 没有类型。** `Value::family()` 返回 `Option<TypeFamily>`，别给它编一个大类。
  它能进任何列，能不能放由 `Column::not_null` 决定。
- **`DataType::parse` 是强制 typmod 的唯一入口**，**`Table::validate` 是强制 typmod
  与 `NOT NULL` 的唯一入口**。别在别处重复校验——两处校验迟早会分叉。

### 十进制算术

- 算术走 `bigdecimal`，**比较仍走手写的 `compare_decimal`**（无分配，排序热路径）。
  两套并存是**有意**的，不要"统一"掉。
- **归一化必须用 `to_plain_string()`，不能用 `to_string()`。** 后者在大数量级上会
  切成科学计数法（`1e50` → `"1e+50"`），而 `compare_decimal` 按整数部分**长度**
  比较——会静默地全盘比错。
- **`bigdecimal` 的 `Div` 遇到零除数会 `panic!`，不是返回 `Err`。** 必须先
  `is_zero()` 拦掉。
- **辅助函数绝不能返回哨兵值来"代替"溢出。** 踩过的坑：`pow10` 溢出时返回 `0`，
  而 `checked_mul(0)` 是 `Some(0)`——溢出被吞掉、除法分子变成 0，结果是**一个安静的
  错答案 `0`**，而不是报错。溢出要一路 `Option`/`Err` 传出去。
- 除法标度按 PostgreSQL 的 `select_div_scale`，其中「领头数字组更小就把 `qweight`
  再减一」那一步最容易漏，漏了会少 4 位小数，并让 `=` 的真假**反过来**。

### 磁盘契约

- `DataType` 的 serde 表示（如 `"INT"`、`"NUMERIC_ANY"`）与 `Column` 的字段名
  **属于磁盘契约**，改名就是改文件格式。
- CSV 里 `NULL` 写成 `\N`，以 `\` 开头的文本多写一个 `\` 转义。**这三者
  （NULL / 空字符串 / 字面文本 `\N`）必须始终可区分。**
- 新增字段一律 `#[serde(default)]`，保证老文件还能读。
