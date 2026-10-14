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
| **3** | 性能与架构（索引、哈希连接、下推、重排、UPDATE） | 5 | 3.0 拉取式执行模型 |

**当前基线**（改动前先确认还成立）：

```text
db.sql 52/0 · query1 4/0 · query2 5/0 · query3 8/0 · query4 0/6 · query5 8/1
（格式：成功 / 失败。query5 唯一那条失败是 EXTRACT——它在降级期就被
 unsupported 拒绝，与执行模型无关；query4 的 6 条同理，全是"没实现"）
cargo test --offline → 334 通过，0 warning
执行模型：拉取式 —— ir::exec 的每个节点产出一个 RowSource，
          只有需要随机访问 / 重扫 / 最终结果才物化（见 §3.0）
交互式终端：cmd/ —— rustyline 行编辑 + Tab 补全 + 多行累积，
          引擎与 SQL 部分的分工见 §1.1 开头
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
  **拉取式执行模型（§3.0）已经把这件事变成自然的事**：内层计划就是一个 `RowSource`，
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

**分层**（§1.1 落地时顺手摆正）：`cmd/` 是**执行引擎**——接受任务（交互式终端敲进来的
一段、`\i` 读进来的内容）、驱动 SQL、管两路输出；`ir/` 是引擎里的 **SQL 部分**——解析、
降级、执行。交互式终端（行编辑、Tab 补全、多行）也在 `cmd/` 下，`main.rs` 只剩一行。

**已实测确认能用的**（别重做）：

- schema 限定名 `study.movies` —— 限定名被当作**不透明的标识符**拼起来用，**不需要真的 schema 系统**；
- `CROSS JOIN`（Test 1.2 要）；
- 4 表连接、内/左外连接、自连接、`GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT`、聚合与 `COUNT(DISTINCT)`、
  `IS NULL`/`IS NOT NULL`、`CASE`、`ROUND`、`died - born` 这类整数运算；
- 元命令 `\i`、`\o <文件>`、`\echo`、`\dt`、`\timing`、`\set ON_ERROR_STOP`；
- **装载可行**：1 万条单行 INSERT 用 0.43 秒 → 7.6 万条约 **3.3 秒**。

## 1.1 【已做】两个元命令：`\timing` 与 `\set ON_ERROR_STOP`

原来两者都报"未知元命令"，`ON_ERROR_STOP` 也就从来没生效过（脚本遇错一直继续跑）。

**做了什么**：

- **`\timing [on|off]`**（无参切换，与 psql 一致）：引擎在每条语句前后取 `Instant`，
  在该条的状态行之后往**消息那一路**打 `耗时 0.423 ms`。只对"真的执行了语句"计时，
  元命令、解析失败、`EXPLAIN` 都不计。
- **`\set ON_ERROR_STOP on|off|1|0|true|false`**（大小写不敏感，不带值就是开）：
  置位后**遇到第一条错误就停**——SQL 语句失败、`\i` 读不了文件、`\o` 打不开、
  未知元命令都算。停下时打 `（ON_ERROR_STOP：已停止后续语句）`。
- **"停"要穿过 `\i` 递归**：`\i` 里设的开关管到会话结束，最内层出错时**外层脚本
  后面的语句也不再跑**（psql 语义）。这是一条显式的 `Flow::Stop` 一路往上传。
- 只认 `ON_ERROR_STOP` 这一个变量；变量名或取值写错就报错，**绝不悄悄当成关着**。
- **与 psql 的一处有意差别**：交互终端里"停"的是**当前这一段提交**（刚敲的那条语句/
  那几行），不会像 psql 那样直接**退出会话**——本库的数据只在内存里，为一次笔误把整个
  库丢掉太狠。脚本那一路（`\i`、`ydbms < s.sql`）与 psql 完全一致：**从出错处整体停**，
  后面的语句一条都不读（管道那一路拿 psql 18.6 实测对过）。

**量级**：引擎里的驱动循环改造 + 两个元命令，约 180 行（含测试）。

**验收**（`tests/session.rs` 12 条新用例 + `tests/repl.rs` 3 条管道用例）：

```text
\timing on 之后每条语句都打「耗时」，\timing off 之后不再打
\timing yes 报「只认 on / off」，且不会把它打开
\set ON_ERROR_STOP on 后：3 条语句里第 2 条失败 ⇒ 第 3 条没跑（表里 0 行）
\i 里的语句失败 ⇒ 外层脚本后面的语句也没跑
off 时行为与过去一致：报错照报，后面的照样跑
管道喂脚本（ydbms < s.sql）：出错处整体停（psql 18.6 实测同此）
```

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

## 1.6 【已做】REPL 支持多行 SQL

原来 `main.rs` **逐行**读、逐行交给引擎，于是把多行 `CREATE TABLE` 敲进终端会被拆成几段、
每段都报语法错（跑测试用 `\i` 不受影响，交互体验是坏的）。

**做了什么**：

- 行编辑换成 **rustyline**（补全、历史、光标都在它那儿），多行判定挂在它的
  **`Validator`** 上：每次回车问一次 [`is_complete`](src/cmd/repl.rs)，不够就继续收行。
- 判定规则见 `cmd::repl::is_complete`：空白输入、**最后一个非空行以 `\` 开头**、
  第一行是 `EXPLAIN`、或 SQL 里出现**顶层分号**——四者之一就算"够一句"。
- 顶层分号的扫描在 `ir::has_top_level_terminator`（与 `statement_text` 那类词法活同处）：
  `'...'`、`"..."`、`-- ...`、可嵌套的 `/* ... */`、`$$...$$` / `$tag$...$tag$`
  里的分号都**不算**（TODO 的验收点）。
- 非 tty（管道/重定向）走 `run_plain`：同样按 `is_complete` 累积，**EOF 时把攒着的交出去**，
  所以 `printf 'SELECT 1\n' | ydbms` 照旧能用。

**量级**：约 260 行（`cmd/repl.rs` + 词法扫描），另有 16 条纯逻辑测试 + 6 条真 pty 测试。

**验收**：

```text
CREATE TABLE m (\n  a INT\n);   终端里三行敲完 → 建表成功：m（中途不提交）
SELECT ';' ;                   字符串里的分号不会提前切句
EXPLAIN SELECT 1               不需要分号
tests/repl.rs  is_complete / has_top_level_terminator 的 8 条用例
tests/repl_pty.rs  用 `script` 开真 pty，分次送键，端到端跑上面几条
```

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

## 2.1 【已做】`CASE` 两分支的类型统一成 `NUMERIC`

原来的现象（对照 PostgreSQL 18.6）：

| 查询 | PostgreSQL 18.6 | ydbms（改前） |
|---|---|---|
| `CASE WHEN true THEN 1 ELSE 1.5 END` | `1`（类型 `numeric`） | 错误：两分支类型不同 |

**做了什么**：按 PG 的 `select_common_type` 补上唯一那条隐式放大——一侧 `INT`、一侧
`NUMERIC` 就统一成 `NUMERIC`。**两处都要改**，少一处就会以另一种方式失败：

1. `unify_branches`（类型检查）复用既有的 `numeric_pair`；
2. `Step::Case` 求值时把取到的那一侧**真的转**（`Int` → `Numeric`，即 `widen_branches`）。
   只改第 1 处的话，投影列是 `NUMERIC`、值还是 `Int`，物化时会被 `TypeMismatch` 顶回来
   ——这与 `widen_numeric`（比较侧）是同一条规矩。

**与 PostgreSQL 18.6 逐条对账过**（`/tmp` 临时集群，`pg_typeof` 一起比）：

| 查询 | PG 18.6 | ydbms |
|---|---|---|
| `CASE WHEN true THEN 1 ELSE 1.5 END` | `1` / `numeric` | `1` / `NUMERIC` |
| `CASE WHEN false THEN 1 ELSE 1.5 END` | `1.5` | `1.5` |
| `CASE WHEN true THEN 1.5 ELSE 2 END` | `1.5` | `1.5` |
| `CASE WHEN true THEN 1 ELSE NULL END` | `1` / `integer` | `1` / `INT`（**不**放大） |
| `CASE WHEN true THEN 1 ELSE 'x' END` | 报类型错 | 报类型错 |

**量级**：约 30 行（含测试）。**验收**：`tests/ir_types.rs` 的 5 条新用例，列类型也断言。

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

**与拉取式的关系**：§3.0 的拉取式解决的是**行**的惰性（上层不拉，下层不算），
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

## 3.0 【已做】拉取式执行模型

**做了什么**：执行从"每个算子物化成一张 `PhysTab`"改成**一行一行拉**（Volcano）。
每个节点产出一个 `RowSource`（`columns` / `next` / `error`），
`ir::exec` 里不再是 `done: Vec<Option<PhysTab>>`，而是每个节点一个**游标**。

- **入口**：`execute_source(&plan, &symbols) -> Box<dyn RowSource>`——拉多少、什么时候停
  由调用方决定；`execute(...) -> PhysTab` 抽干 root。两者的**公开类型都没变**
  （`Outcome::Rows(PhysTab)`、`stmt`、`session` 一行没改）。
- **物化点恰好四处**：`Sort` / `Aggregate`（要随机访问）；连接的**内侧**（嵌套循环要重扫）；
  被**多个父节点**引用的节点（物化一次，`Rc` + 每个消费者一个游标）；**最终结果**。
  `Scan` / `Filter` / `Project` / `Limit` 全程流式。
- **错误通道**：`next()` 返回 `None` 既可能是结束也可能是出错，原因在 `error()`；
  复合算子不抄输入的错误，而是往 `input.error()` 问（没拉过就不会误报）。
- **分裂输入求值**：`Prog::eval_split(左行, 右行)`——程序里的下标按 `左 ++ 右` 编，
  两半各取各的，连接**只在命中时才拼整行**。

**顺带吸收掉的原 3.1**：连接不再为每一对候选行 `lrow.clone() + extend` 拼完整 `Row`，
所以"分裂输入"不再单列一项。

**两处行为变化**（都往 PostgreSQL 靠，`tests/ir_source.rs` 把它们钉住了）：

1. **`LIMIT` 之下的行不再被求值**：`SELECT 1/x … LIMIT 1`（第二行 `x = 0`）不再报错。
   **例外是 `Sort`**：它要随机访问，必须看完全部输入。
2. **从 root 走不到的节点不再执行**（静态校验 `analyze` 仍然覆盖它们）。

**已知不做**：共享节点的物化发生在**构建期**，所以 DAG 里 `LIMIT` 之下的共享子树
仍会被整个算出来（当前 `lower` 从不产出 DAG，只有手搓的 `Plan` 碰得到）。

## 3.1 【难】索引 / index nested loop

基表上的访问路径。**它还是一件独立的事，但不再是"表类型"的事**：

- 索引就是**又一个 `RowSource`**：`next()` 顺着 B 树/哈希表吐行。
  旧版设想的 `JoinTab: Table` 那条随机访问硬约束**已经不存在**——拉取式下没有任何算子
  要求"第 i 行是什么"。
- 索引嵌套循环 = 把连接内侧从"物化整个右支"换成"按左行的键去索引里取"，
  于是内侧那个物化点消失，右支也不必是基表。
- **量级**：200 行起，主要在 `db/`（索引结构）与 `ir/exec.rs`（内侧策略）。
- **验收**：`WHERE a.x = 1` 能走索引；"每行一次索引查找"的代价不再随 N 二次增长。

## 3.2 【中】哈希连接

**现状**：连接还是朴素嵌套循环——内侧（右支）整个物化，外侧逐行去配。
实测代价（两张 N 行的表等值连接；**这些数字是拉取式之前测的**，改动后应复测填回）：

| N | 配对次数 | 耗时 |
|---|---|---|
| 250 | 62 500 | 0.04 秒 |
| 500 | 250 000 | 0.14 秒 |
| 1000 | 1 000 000 | 0.53 秒 |
| 2000 | 4 000 000 | 2.10 秒 |

N 翻倍 → 耗时**恰好 ×4**，干净的二次方。db1 的四表连接 ≈ 37 440 次配对、整份 query2 0.10 秒
——那是**数据小的运气**。`filmdb.sql` 是 7.6 万条数据，这个二次方迟早撞上。
（拉取式让左支不再物化、`ON` 不再拼整行，但**配对次数没变**，所以这条仍然是待办。）

- **做法**：给 `JoinSource` 加一种策略——把 **build 侧**抽干成
  `HashMap<Vec<Value>, Vec<Row>>`（键 = `ON` 里的等值列），**probe 侧**流式。
  没有新类型要造：连接本来就是"又一个 `RowSource`"，换的只是"内侧怎么找"。
- **绑定器**：认出 `ON` 里的等值合取项才能选哈希；认不出就退回嵌套循环（正确性不变）。
- **量级**：`ir/exec.rs` 的哈希策略约 120 行；绑定器挑等值键约 60 行。
- **验收**：N=2000 的等值连接显著下降且 N 翻倍不再 ×4；
  同一份期望数据在两种策略下**逐行一致**。

## 3.3 【中】谓词下推

**机制现成、只是没人用**：`Op::Scan` 有 `filter` 槽位、`PlanBuilder::scan_with_filter`
存在、执行器（拉取式之后就是一个流式源）也认——但**降级从不调用它**。

- **做法**：绑定器把 `WHERE` 拆成合取项，只引用单表列的推到那张表的 `Scan.filter`；
  引用了多表的留在 `Filter` 里。拉取式之下这一步的收益是**少拉行**：被推掉的行
  根本不会进入上层管道。
- 注意 project1 的 Test 4.x 正是**故意对比 pre-filter（写在 `ON`）与 post-filter
  （写在 `WHERE`）**，所以下推不是它的正确性要求，而是它的观察对象——推下去以后
  `EXPLAIN` 的形状会变，对 `EXPLAIN` 的断言要跟着看。
- **量级**：约 80 行，全在 `lower.rs`。
- **验收**：`EXPLAIN SELECT … FROM a JOIN b ON a.x = b.x WHERE a.x = 1` 里
  `Filter` 出现在 Join **之下**（也就是变成 `Scan(a, filter=[x = 1])`）。

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

---

# 附：外部依赖备忘

- 1.4 的 `~*` 正则若要做，需要引 `regex`——本项目一直**避免新增依赖**（`NUMERIC` 自己写，
  只在引入 `bigdecimal` 时破过一次例）。**先确认。**
- 环境里有 **PostgreSQL 18.6**（`/usr/lib/postgresql/18`）。做这些改动时，
  可以像之前那样在 `/tmp` 起一个临时集群逐条对账——**不要碰用户自己在跑的 5432 集群**。
