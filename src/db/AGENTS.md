# AGENTS.md -- `src/db`

## 职能

数据库核心的**值模型与表抽象**.这一层不认识 SQL,也不认识计划:

- `table.rs` -- [`DataType`](table.rs) / [`Value`](table.rs) / [`Column`](table.rs) /
  [`Table`](table.rs) / [`TabShape`](table.rs) / [`DbError`](table.rs),
  以及十进制算术与 CSV 字段读写.
- `phys.rs` -- 物化表 [`PhysTab`](phys.rs).
- `source.rs` -- 拉取式的中间结果 [`RowSource`](source.rs) / 基表游标
  [`TableCursor`](source.rs) / 物化 [`materialize`](source.rs) 与抽干 [`drain`](source.rs).
- `index.rs` -- 一列上的"键 → 行号"索引 [`Index`](index.rs),**可失效可重建**的
  [`IndexSlot`](index.rs),以及把行号读成行的 [`IndexSource`](index.rs).

**上游**:`pers/` 通过 `&dyn Table` 落盘;`ir/` 把 `Value` 当标量用,把 `RowSource`
当执行管道的每一节.这一层**被依赖,不依赖别人**,请保持这个方向.

## 特殊规则

### 两个契约,不要混

- **`Table` 是随机访问**(`nrows()` + `row(r)`):基表与落盘要它.
- **`RowSource` 是只进不退**(`next()` + `error()`):执行器要它.连接,哈希连接,
  索引扫描都是"又一个 `RowSource`"(`ir/exec.rs` 与 `index.rs`),**不需要**再有
  一个 `JoinTab: Table`.

**物化只发生在 `materialize`/`drain` 的调用点**(`Sort`/`Aggregate`,嵌套循环连接的
内侧,哈希连接的 build 侧,被多个父节点引用的节点,最终结果).别为了图方便在别的
算子里攒行--拉取式的收益全在"上层停手,下层就不算"上.索引攒的是**行号**而不是行,
所以索引嵌套循环不物化内侧.

`next()` 返回 `None` 有两个可能:**正常结束**与**出错**,后者必须能通过 `error()`
问到.一旦结束或出错,后续 `next()` 要继续返回 `None`(幂等).

### 谁强制什么:两层

- **`db` 这一层**:`DataType::parse`(typmod 的唯一解析入口)与 `Table::validate`
  (typmod 与 `NOT NULL` 的唯一强制入口,`insert`/`update`/`update_row` 都走它).
  老写法只比"大类相不相等",`VARCHAR(30)` 会被误判--**别再把校验写一遍**.
- **`ir` 那一层**:`CHECK`(`ir/checks.rs` 的 `CheckedTable` 装饰器)与 `DEFAULT`
  (`ir/irgen.rs` 在 INSERT 缺列时补).它们要 `Prog`,而 `db` 不认识 IR.

### 整表快照(`Table::snapshot`)

事务回滚要一份"改之前的表",所以 trait 上有一个 `snapshot()`:**物理表与带 `CHECK`
的表都覆盖成 `clone`**,其余实现走默认(按形状与逐行 `row` 物化).

- **默认实现会把装饰丢掉**:`CheckedTable` 若走默认,回滚之后那张表就不再强制
  `CHECK` 了--**新的装饰器实现必须覆盖 `snapshot`**;
- 默认实现存在的唯一理由是让只读的虚表(测试里的计数表之类)不用为它做任何事.

### 索引(`IndexSlot`)

索引是**会话级的目录对象**(名字在 `ir/symbol.rs`):`CREATE INDEX` 建一份,
`INSERT` 追加,`UPDATE` 作废,下次用到时重建.三条要注意:`RefCell` 是**必需**的
(目录是 `&` 借出去的,而失效/重建/追加都要改它);`IndexSource` 仍然是**惰性**的
(第一次 `next()` 才建,`LIMIT 0` 之下连索引都不建);索引攒的是**行号**而不是行.

### 不变式

- **规范文本是不变式,不是风格.** `Value::Numeric` 去掉尾随零,`Timestamp` 定宽
  文本.原因是 `Ord` 与 `PartialEq` 必须一致--一旦同一个值有两种文本表示,
  `Ord` 说相等而 `PartialEq` 说不等,排序与 `GROUP BY` 就会出错.
- **NULL 没有类型.** `Value::family()` 返回 `Option<TypeFamily>`,别给它编一个大类.
  它能进任何列,能不能放由 `Column::not_null` 决定.
- **`Value` 的 `Hash` 只跟派生的 `Eq` 对齐**(文本/内容相等),那是哈希连接与索引拿它
  当键的依据;`Ord` 是另一套(`NUMERIC` 按定点比).两者不冲突靠的是规范文本这条不变式.
  **跨大类的值不是同一个键**(`Int(1)` 与 `Numeric("1")`),所以键必须同族--
  `ir/plan.rs` 的 `analyze` 强制这一条.
- **`DataType::parse` 是强制 typmod 的唯一入口**,**`Table::validate` 是强制 typmod
  与 `NOT NULL` 的唯一入口**.别在别处重复校验--两处校验迟早会分叉.

### 十进制算术

- 算术走 `bigdecimal`,**比较仍走手写的 `compare_decimal`**(无分配,排序热路径).
  两套并存是**有意**的,不要"统一"掉.
- **归一化必须用 `to_plain_string()`,不能用 `to_string()`.** 后者在大数量级上会
  切成科学计数法(`1e50` → `"1e+50"`),而 `compare_decimal` 按整数部分**长度**
  比较--会静默地全盘比错.
- **`bigdecimal` 的 `Div` 遇到零除数会 `panic!`,不是返回 `Err`.** 必须先
  `is_zero()` 拦掉.
- **辅助函数绝不能返回哨兵值来"代替"溢出.** 踩过的坑:`pow10` 溢出时返回 `0`,
  而 `checked_mul(0)` 是 `Some(0)`--溢出被吞掉,除法分子变成 0,结果是**一个安静的
  错答案 `0`**,而不是报错.溢出要一路 `Option`/`Err` 传出去.
- 除法标度按 PostgreSQL 的 `select_div_scale`,其中"领头数字组更小就把 `qweight`
  再减一"那一步最容易漏,漏了会少 4 位小数,并让 `=` 的真假**反过来**.

### 磁盘契约

- `DataType` 的 serde 表示(如 `"INT"`,`"NUMERIC_ANY"`)与 `Column` 的字段名
  **属于磁盘契约**,改名就是改文件格式.
- CSV 里 `NULL` 写成 `\N`,以 `\` 开头的文本多写一个 `\` 转义.**这三者
  (NULL / 空字符串 / 字面文本 `\N`)必须始终可区分.**
- 新增字段一律 `#[serde(default)]`,保证老文件还能读.
