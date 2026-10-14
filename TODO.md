# TODO

这份文件按**阶段**分组，组内按**难度**标注。每项都写清：为什么缺、缺在哪、改动量级、怎么验收。
阶段之间大致有依赖——**先让测试跑起来，再收正确性，最后动性能**。

难度标记：**【易】** 半天内、改动集中；**【中】** 一到两天、要动一两层；**【难】** 需要新机制或重写。

---

## 分组总览

| 阶段 | 目标 | 待办 | 已完成 |
|---|---|---|---|
| **0** | 跑通 `tests/script/db1/` 的 query4 / query5 | 2 | — |
| **1** | 跑通 project1 的测试套件（`ydbms-test/`） | 5 | 1.1 `\timing` / `\set`、1.6 REPL 多行 |
| **2** | 正确性收尾（与 PostgreSQL 的已知偏差） | 1 | 2.1 `CASE` 两分支放大 |
| **3** | 性能与架构（索引、哈希连接、下推、重排、UPDATE） | 3 | 3.0 拉取式执行模型、3.1 索引 / 索引嵌套循环、3.2 哈希连接、3.3 谓词下推 |

**当前基线**（改动前先确认还成立）：

```text
db.sql 52/0 · query1 4/0 · query2 5/0 · query3 8/0 · query4 0/6 · query5 8/1
（格式：成功 / 失败。query5 唯一那条失败是 EXTRACT——它在降级期就被
 unsupported 拒绝，与执行模型无关；query4 的 6 条同理，全是"没实现"）
cargo test --offline → 368 通过，0 warning
执行模型：拉取式 —— ir::exec 的每个节点产出一个 RowSource，
          只有需要随机访问 / 重扫 / 哈希 build / 最终结果才物化（见 ir/exec.rs）
访问路径：WHERE 的合取项下推到单表 Scan、Scan 可走索引、
          连接有嵌套循环 / 哈希 / 索引嵌套循环三种策略（见 ir/plan.rs 的 JoinStrategy）
连接规模：两张 N 行的表等值连接，N=2000 时嵌套循环 1.63 秒 → 哈希 6.15 毫秒、
          索引嵌套循环 4.26 毫秒，N 翻倍后者约 ×2（跑 tests/ir_join.rs 里
          #[ignore] 的 join_scaling_benchmark 可复现）
交互式终端：cmd/ —— rustyline 行编辑 + Tab 补全 + 多行累积，
          引擎与 SQL 部分的分工见「阶段 1」开头
```

> **测试台只数成败，不比对结果。** 所以"某条查询不报错"不等于"算对了"——
> 0.1 的 `ORDER BY <序号>` 就是活例子：它现在不报错，但排的是常量，顺序是错的。

---

# 阶段 0 —— 跑通 db1 的 query4 / query5

`query1`~`query3` 已全绿。这两份是 `tests/script/db1/` 里剩下的。

## 0.1 【中】query5：`EXTRACT(YEAR FROM ts)` 与 `ORDER BY <序号>`

query5 的 9 条里 8 条不报错，只剩 `EXTRACT` 那一条：

| 缺什么 | 出现在 | 说明 |
|---|---|---|
| `EXTRACT(YEAR FROM ts)` | Test 5.4 | 有了它才能"按入学年份排序"。TIMESTAMP 已是定宽文本，取前 4 位即可，**不需要日期运算** |
| `ORDER BY <输出列序号>` | Test 5.5 | `ORDER BY 3 DESC, 1` 指第 3、第 1 个输出列 |

> **Test 5.5 现在"过"着，但那是假的。** 它被降级成 `Sort(keys=[3 DESC, 1 ASC])`——
> 排的是**常量**，顺序是错的（`EXPLAIN` 一眼可见）。它没被算作失败，只是因为
> **测试台只数错误、不比对结果**（见上面的基线注）。这一条仍然是缺的。

- **量级**：`EXTRACT` 加一个 `Step`（或复用 `Step::Call`）+ 降级分支，约 40 行；
  `ORDER BY <序号>` 在 `lower_sort_keys` 里认 `AConst` 整数，约 15 行。
- **验收**：`query5.sql` 9/0；`EXPLAIN` 里排序键指向正确的输出列
  （现在是 `Sort(keys=[3 DESC, 1 ASC])`，应当变成 `Sort(keys=[credits DESC, code ASC])`）。

## 0.2 【难】query4：整套子查询机制

query4 的 6 条**全部**要子查询，一条都跑不了：

| 缺什么 | 出现在 |
|---|---|
| 标量子查询 | [4-1] `WHERE g.gpa > (SELECT AVG(gpa) …)` |
| `IN (子查询)` | [4-2] |
| `UNION` | [4-2]（两个子查询取并集） |
| `EXISTS` / `NOT EXISTS`（**相关**子查询） | [4-3] |
| `SELECT` 列表里的相关标量子查询 | [4-4] |
| `FROM` 里的子查询（派生表） | [4-5] |
| `ALL` | [4-6] |

- **为什么难**：这不是"再加一个算子"，而是**执行入口**的问题。相关子查询要求执行器能
  **在外层某一行上重新求值一个内层计划**，即执行要"可重入"。
  **拉取式执行模型（见 `ir/exec.rs` 的文件头）已经把这件事变成自然的事**：内层计划就是一个 `RowSource`，
  而构建管道是纯的（`execute_source(&plan, &symbols)` 不碰任何状态），
  所以"为外层每一行重新构建/重开一份内层源"只是多调一次构建——`execute` **不用再动**。
  剩下的活是把外层某一行的值**绑**到内层上（内层程序读的是外层的列），
  以及给 `Op` 加一个"子计划"变体。
- **路线建议**：先做**非相关**的（标量、`IN`、`UNION`、非相关 `EXISTS`、派生表）：
  内层计划独立跑完、抽干成一份物化结果再当常量用；**相关**子查询单独一步，
  走上面那个"按外层行重开内层源"的入口。
- **量级**：非相关部分 300+ 行；相关部分是"子计划节点 + 外层值绑定"，**不再需要重写执行器**。
- **验收**：`query4.sql` 6/0。

---

# 阶段 1 —— 跑通 project1 的测试套件

目标：`ydbms-test/` 能跑完 —— `init.sql`（建库）→ `filmdb.sql`（7.6 万条数据）→
`tests.sql`（12 组）与 `correctness/queries.sql`（25 条，带 `expect/` 文件）。

**分层**（已经摆正）：`cmd/` 是**执行引擎**——接受任务（交互式终端敲进来的
一段、`\i` 读进来的内容）、驱动 SQL、管两路输出；`ir/` 是引擎里的 **SQL 部分**——解析、
降级、执行。交互式终端（行编辑、Tab 补全、多行）也在 `cmd/` 下，`main.rs` 只剩一行。

**已实测确认能用的**（别重做）：

- schema 限定名 `study.movies` —— 限定名被当作**不透明的标识符**拼起来用，**不需要真的 schema 系统**；
- `CROSS JOIN`（Test 1.2 要）；
- 4 表连接、内/左外连接、自连接、`GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT`、聚合与 `COUNT(DISTINCT)`、
  `IS NULL`/`IS NOT NULL`、`CASE`、`ROUND`、`died - born` 这类整数运算；
- 元命令 `\i`、`\o <文件>`、`\echo`、`\dt`、`\timing`、`\set ON_ERROR_STOP`；
- **装载可行**：1 万条单行 INSERT 用 0.43 秒 → 7.6 万条约 **3.3 秒**。

## 1.2 【中】DDL：表级约束与 `DEFAULT`

`filmdb.sql` 的 11 张表里，**只有 1 张**现在建得起来。实测的拒绝原因：

| 缺什么 | 影响的表 |
|---|---|
| **`CHECK` 约束**（含 `length()`） | `countries`、`movies`、`people`、`credits` |
| **表级约束** `primary key(...)` / `unique(...)` / `foreign key ... references ... on delete cascade` | `countries`、`movies`、`credits`、`forum_topics`、`forum_posts`、`films_francais`、`alt_titles`、`movie_title_ft_index2` |
| **`DEFAULT`** | `people.gender default '?'`、`movie_title_ft_index2.titleid default 1` |

- **要点**：`CHECK` 现在是**故意拒绝**的（"不强制就会静默违规"）。放开它必须先决定：
  **真强制**（INSERT 时求值，要能求 `length()`）还是**接受但记入 `unenforced`**。
  按本项目一贯的口径（`NOT NULL`、typmod 都真强制），**建议真强制**。
- **表级约束**：`primary key(...)` / `unique(...)` 现在没实现唯一性检查，只能进 `unenforced`（要显示出来）；
  `foreign key ... references` 目前**整个拒绝**，需要决定是"接受但记入 `unenforced`"还是"实现引用检查"。
  `on delete cascade` 只要不实现 DELETE 就没有语义，可接受 + 记录。
- **量级**：表级约束解析与记录 80 行；`DEFAULT`（存进 `Column`，并在 INSERT 缺列时补）120 行；
  `CHECK` 若只接受 + 记录 40 行，若真强制要接表达式求值，200 行。
- **验收**：`filmdb.sql` 里的全部 `CREATE TABLE` 11/11 成功；建表提示里如实列出未强制的约束。

## 1.3 【中】`DATE` 类型 + `length()`

| 缺什么 | 出现在 |
|---|---|
| **`date` 类型** | `forum_members.registered`、`forum_topics.post_date`、`forum_posts.post_date` |
| **`length()`** | 8 处 `CHECK` 里 |

- `filmdb.sql` 用到的类型只有 `int`/`integer`、`varchar`、`char`、`text`、**`date`**——`date` 是唯一缺的。
- `DATE` 可以照 `TIMESTAMP` 的做法：**存规范化的定宽文本**（`YYYY-MM-DD`），于是字典序即时间序，
  仍然不需要日期运算。（0.1 的 `EXTRACT(YEAR FROM ...)` 直接取前 4 位。）
- `length()` 是 `Step::Call` 里加一个函数；注意要按**字符数**算（与 `VARCHAR` 的检查一致）。
- **量级**：`DATE` 约 80 行（`DataType` 加一个变体 + 规范化 + CSV 往返）；`length()` 约 30 行。
- **验收**：三张 `forum_*` 表建得起来、数据装得进去；`length('中文') = 2`。

## 1.4 【中】查询层特性

`tests.sql` 与 `correctness/queries.sql` 里**逐条实测**出来的拒绝清单：

| 缺什么 | 出现在 |
|---|---|
| **`IN (列表)`** | tests 2.2 / 2.3 / 3.1 / 10.1b / 11.2；queries 0 / 3 / F / M |
| **`BETWEEN`** | queries 3 |
| **`LIKE` / `NOT ILIKE`** | queries 2 / 5 / 9 / A / D |
| **`~*` 正则匹配** | queries 6（`title ~* '\mman\M'`） |
| **`DISTINCT`** | tests 11.1 / 11.2 / 12.1a / 12.1b |
| **派生表（`FROM` 子查询）** | tests 10.1a / 10.2；queries J / K / L / M / N |
| **`COUNT(*) FILTER (WHERE …)`** | queries N |
| **`::numeric` 造型** | queries N |
| **`EXTRACT(YEAR FROM CURRENT_DATE)`、`CURRENT_DATE`** | queries 8 / C |
| **`UPPER()` / `LEFT()`** | queries B |
| **`CONCAT_WS()`** | tests 11.2 |

- **性价比最高的四个**：`IN`、`BETWEEN`、`LIKE`/`ILIKE`、`DISTINCT`——语法糖，各 30~60 行，
  但覆盖了绝大多数失败（`IN` 一处就影响 9 条查询）。
- **派生表**和 0.2 的子查询是同一件事，可以合并做：内层 `Plan` 直接当一个 `RowSource`
  用，**不必先物化**——拉取式之下这是免费的（内层源就是把管道往下接一节）。
- **`DISTINCT` 也是顺手的**：加一个流式的 `DistinctSource`（一个哈希集合记住见过的行），
  既不用先 `Sort`、也不用物化——又一个"一行一行过滤"的算子。
- `~*` 正则要引 `regex` 依赖（本项目一直避免新依赖，需先确认）。
- **量级**：`IN`/`BETWEEN`/`LIKE`/`ILIKE`/`DISTINCT` 合计约 200 行；
  其余标量函数与 `FILTER` 约 150 行；`::numeric` 约 30 行。
- **验收**：`tests.sql` 12 组全过；`correctness/queries.sql` 的 25 条与 `expect/` 逐字一致。

## 1.5 【中】事务与 schema 语句

| 缺什么 | 出现在 |
|---|---|
| **事务 `BEGIN` / `COMMIT`** | `init.sql`、`tests.sql`、`filmdb.sql`、`queries.sql` |
| **`CREATE SCHEMA` / `DROP SCHEMA … CASCADE`** | `init.sql` |
| **`SET search_path`** | `init.sql` |

- 注意：`study.movies` 这种限定名**已经能用**，所以 `CREATE SCHEMA` / `SET search_path`
  只需要**接受并当作无操作**（配合限定名就够跑）；`DROP SCHEMA` 在"一个会话一个进程"的模型里
  也可以只清掉该前缀的符号。
- 事务**不建议真做**（没有 WAL、没有回滚）。**接受 `BEGIN`/`COMMIT` 并当作无操作**是本项目
  一贯的"接受但如实记录"路线；`ROLLBACK` 要显式拒绝（不能假装回滚成功）。
- **量级**：三条语句各 20~40 行。
- **验收**：`init.sql` 一次跑通；连跑两次（`DROP SCHEMA IF EXISTS … CASCADE` 重入）也不报错。

## 1.7 【？】`tests.yql` 要不要支持

`ydbms-test/` 里还有一份 `tests.yql`，用的**不是 SQL**：

```text
AFFAIR test {
    LET m = study.movies;
    << m | JOIN co | SELECT *;
}
```

它与 `tests.sql` 是**同一套测试的两种写法**（Test 1.1 / 1.2 / 1.3 一一对应）。
这是另一门查询语言的实现，量级不小于整个 `lower.rs`。

**需要确认它是否在范围内。** 若在，应单独开一个阶段，不能混在 SQL 里做。

---

# 阶段 2 —— 正确性收尾

## 2.2 【中】`CASE` 与 `AND`/`OR` 的分支被提前求值

**现象**（实测，对照 PostgreSQL 18.6）：

| 查询 | PostgreSQL 18.6 | ydbms |
|---|---|---|
| `CASE WHEN 1=0 THEN 1/0 ELSE 42 END` | `42` | 错误：除以零 |
| `CASE WHEN true THEN 1 ELSE 1/0 END` | `1` | 错误：除以零 |
| `WHERE false AND (1/0 = 1)` | 0 行 | 错误：除以零 |
| `WHERE true OR (1/0 = 1)` | 1 行 | 错误：除以零 |
| `WHERE x <> 0 AND 10/x > 2`（x 含 0） | 2 行 | 错误：除以零 |

**根因**：

```rust
Step::Case { dst, cond, then_value: Reg, else_value: Reg }   // 装的是**已算好的寄存器**
```

`Prog` 是扁平的 `Vec<Step>`，没有跳转，所以求值就是一个 `for` 循环——两个分支必然先算完。
`AND`/`OR` 同理：`apply(op, l, r)` 的两个操作数也是算好的寄存器。

**与拉取式的关系**：拉取式（`ir/exec.rs`）解决的是**行**的惰性（上层不拉，下层不算），
而 `Prog` 是表达式层、仍然扁平无跳转，所以这两个问题一个都没少——修法 A 与 B 都还要做。
这两件事**不冲突**：一处管行的流动，一处管表达式内部的控制流。

**影响的性质**：表达式无副作用，所以**不会算错，只会拒绝掉 PG 会接受的查询**。但两处不同：

- **`CASE` 的惰性是 SQL 标准保证的** → 真 bug；
- **`AND`/`OR` 的求值顺序标准明确不保证**（PostgreSQL 文档原话：`WHERE x > 0 AND y/x > 1.5`
  不保证 `x > 0` 先算）→ 严格说依赖它不合规。但 PG 实际会短路、计划器还按代价重排，
  所以事实上能用；我们这边不能 → **事实上的兼容性缺口**。

### 修法 A（推荐）：分支做成自足子程序

```rust
Step::Case { dst: Reg, cond: Reg, then_prog: Prog, else_prog: Prog }  // 只在选中时求值
```

- 外层 `Prog` 仍是扁平数组 → **顶层的线性没丢**；
- **引入有界递归**（深度 = 表达式嵌套深度）。关键论据：`lower_expr` 本来就在表达式嵌套上递归，
  一万层嵌套的 `CASE` 在**降级阶段就已经爆栈**，求值阶段的递归不引入新的失败类别；
- 顺带能修 `AND`/`OR`：`Step::And { dst, left, right_prog }`（左侧非 FALSE 才算右侧）；
- **量级**：约 100~130 行（`ir/expr.rs` 的 Step / verify / eval / explain + `ir/lower.rs`）。

### 修法 B：给 `Prog` 加跳转

```rust
Step::JumpIfFalse { src: Reg, target: usize },  Step::Jump { target: usize }
```

- `Prog` 保持完全扁平、**零递归**，最符合线性 IR 的初衷；
- 但**破坏不变量 I2（define-before-use）**：某个寄存器可能只在一条路径上被写，
  `verify` 要从"一遍前向扫描"退化成**数据流分析**（definite assignment / 可达性）；
- `eval` 从 `for` 变成带 pc 的 `while`，要处理越界与死循环；
- **量级**：约 150~200 行，主要在重写验证器。

### 被排除的两条路

- **把除零改成 NULL**：PostgreSQL 单独 `SELECT 1/0` 是**报错**的（实测 `ERROR: division by zero`），
  改了会引入方向相反的新偏差。
- **只在死分支屏蔽错误**：判断"是不是死分支"本身就需要控制流。

**验收**：

```text
CASE WHEN 1=0 THEN 1/0 ELSE 42 END   → 42
CASE WHEN true THEN 1 ELSE 1/0 END   → 1
WHERE false AND (1/0 = 1)            → 0 行
WHERE true  OR  (1/0 = 1)            → 1 行
WHERE x <> 0 AND 10/x > 2（x 含 0）   → 2 行
```

---

# 阶段 3 —— 性能与架构

## 3.4 【难】连接重排 + 代价估算

需要统计信息、基数估算、顺序搜索、树重写。注意**一个新类型都省不掉也帮不上**——
`Op::Join` 已经能表达任意二叉树，拉取式也不再要求"树"或"先物化"，
缺的只是**决策过程**（以及决策要用的统计信息）。

- **量级**：一个项目（统计信息 + 代价 + 搜索）。
- **验收**：多表连接里，重排后的计划在实测上优于 `FROM` 的书写顺序。

## 3.5 【中】`UPDATE`

`ydbms` 拒绝 `UPDATE`。project1 的提示 3 要求用 `UPDATE` 把人名里的 `'e'` 全部改成 `'ee'`
——但那一步是在 **PostgreSQL 里**做的，`ydbms` 本身并不需要它。

- **先确认这是不是真的要求。** 若要：`UPDATE` 是第一个 DML 写操作，
  要决定事务语义（本项目没有事务）与约束强制的时机。
- **读路径不用新东西**：`Scan` 已经是一个流式 `RowSource`，逐行读、逐行改即可。
- **量级**：约 150 行。

## 3.6 【中】访问路径的两处收尾

3.1 ~ 3.3 落地之后剩下的两件小事：都不影响正确性，只是把「能用」变成「划算 / 更通用」。

**索引是查询期现建的**：用它的执行器自己扫一遍表建出来，于是单表 `WHERE x = 1` 要先把
表扫一遍——这条路径目前并不比重扫快，收益只在「连接的每行一次查找」与「内侧不物化」上。

- **做什么**：`CREATE INDEX` / `DROP INDEX` 进符号表（索引跟着表走，`INSERT` 时维护、
  `UPDATE`/`DELETE` 时失效）；或者退一步——会话级缓存「(表, 列) → 索引」，写表时整体失效。
- **验收**：同一张表上连着两条 `WHERE x = …` 的第二条不再重新建索引（用会数数的表能
  看见「只建一次」）；`filmdb.sql` 的 7.6 万行装载不被拖慢。

**可空侧现在一个谓词都不推**：`LEFT JOIN` 的可空侧会被外连接补出 NULL，`WHERE b.y IS NULL`
就是反例。但等值/比较这类**遇到 NULL 必不成立**的谓词（`b.y = 5`）推下去是保语义的，
白白少了一条优化。

- **做什么**：加一个「这个谓词是不是 null-rejecting」的判定（比较运算、`IS NOT NULL` 是；
  `IS NULL`、`NOT` 包住的不一定），只放行这一类的下推。
- **验收**：`EXPLAIN … LEFT JOIN … WHERE b.y = 5` 里出现 `Scan(b, filter=[y = 5])`，
  而 `WHERE b.y IS NULL` 仍然留在 Join 之上；两者的结果与现在一致。

---

# 附：外部依赖备忘

- 环境里有 **PostgreSQL 18.6**（`/usr/lib/postgresql/18`）。做这些改动时，
  可以像之前那样在 `/tmp` 起一个临时集群逐条对账——**不要碰用户自己在跑的 5432 集群**。
