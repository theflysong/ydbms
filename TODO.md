# TODO

这份文件按**阶段**分组，组内按**难度**标注。每项都写清：为什么缺、缺在哪、改动量级、怎么验收。
阶段之间大致有依赖——**先让测试跑起来，再收正确性，最后动性能**。

难度标记：**【易】** 半天内、改动集中；**【中】** 一到两天、要动一两层；**【难】** 需要新机制或重写。

---

## 分组总览

| 阶段 | 目标 | 待办 | 已完成 |
|---|---|---|---|
| **0** | 跑通 `tests/script/db1/` 的 query4 / query5 | 0 | 0.1 `EXTRACT` / `ORDER BY <序号>`、0.2 子查询 |
| **1** | 跑通 project1 的测试套件（`ydbms-test/`） | 1 | 1.1 `\timing` / `\set`、1.6 REPL 多行、1.4 查询层特性、1.5 事务与 schema、1.2 DDL 约束与 `DEFAULT`、1.3 `DATE` + `length()` |
| **2** | 正确性收尾（与 PostgreSQL 的已知偏差） | 0 | 2.1 `CASE` 两分支放大、2.2 `CASE`/`AND`/`OR` 的惰性（修法 A） |
| **3** | 性能与架构（索引、哈希连接、下推、重排、UPDATE） | 1 | 3.0 拉取式执行模型、3.1 索引 / 索引嵌套循环、3.2 哈希连接、3.3 谓词下推、3.5 `UPDATE`、3.6 索引进目录 + 可空侧下推 |

**当前基线**（改动前先确认还成立）：

```text
db.sql 52/0 · query1 4/0 · query2 5/0 · query3 8/0 · query4 6/0 · query5 9/0
（格式：成功 / 失败）
cargo test --offline → 501 通过（1 个 #[ignore] 的基准），0 warning；约 13 秒
（其中 tests/ydbms_suite.rs 的端到端装载 + 25 条查询约 9 秒）
执行模型：拉取式 —— ir::exec 的每个节点产出一个 RowSource，
          只有需要随机访问 / 重扫 / 哈希 build / 最终结果才物化（见 ir/exec.rs）
访问路径：WHERE 的合取项下推到单表 Scan（LEFT JOIN 的可空侧只放行 null-rejecting 的
          谓词，且连接之上仍留一份）；索引是**目录对象**（CREATE INDEX / DROP INDEX
          建与删，INSERT 追加、UPDATE 作废重建），只有目录里真有的索引才走；
          连接有嵌套循环 / 哈希 / 索引嵌套循环三种策略（见 ir/plan.rs 的 JoinStrategy）
连接规模：两张 N 行的表等值连接（debug 档），N=2000 时嵌套循环 1.63 秒 →
          哈希 6.0 毫秒、索引嵌套循环 4.3 毫秒，N 翻倍后者约 ×2
          （跑 tests/ir_join.rs 里 #[ignore] 的 join_scaling_benchmark 可复现；
          手写计划的索引嵌套循环走「本地槽位」那条兜底，与目录里的索引同价）
子查询：内层是一份**自足**的 Plan（Step::Subquery / Step::Outer），执行时按外层行
        重开一份源、未相关的缓存内层**行**（见 ir/exec.rs 的 ExecCtx）；
        派生表走 PlanBuilder::splice，UNION [ALL] 走 Op::Union
表达式惰性：CASE 的两个分支与 AND/OR 的右侧是**自足子程序**（Step::Case / And / Or），
        只有被选中时才求值（死分支里的除零不再冒出来）；AND/OR 按书写顺序、
        左侧能定胜负就不算右侧（见 ir/expr.rs 与 tests/ir_lazy.rs）
DDL/DML：CREATE TABLE 的 NOT NULL / DEFAULT / CHECK **真强制**（CHECK 见 ir/checks.rs），
        PRIMARY KEY / UNIQUE / FOREIGN KEY 接受并**如实记录为未强制**；
        INSERT 缺列补 DEFAULT（没有就补 NULL）、UPDATE 逐行改（值在旧行上求值、
        整行一次落库，先整体作废索引）
类型：INT / TEXT / CHAR / VARCHAR / TIMESTAMP / DATE / NUMERIC（date 定宽 YYYY-MM-DD）
模块：ir/value.rs（值层）、ir/irgen.rs（降级，原 ir/lower.rs）、ir/opt/（优化器）、
        ir/checks.rs（CHECK 强制点）
交互式终端：cmd/ —— rustyline 行编辑 + Tab 补全 + 多行累积，
          引擎与 SQL 部分的分工见「阶段 1」开头
```

> **测试台只数成败，不比对结果。** 所以"某条查询不报错"不等于"算对了"——
> 0.1 的 `ORDER BY <序号>` 曾经是活例子：它不报错，但排的是常量。
> 现在它真的有测试盯着（`tests/db1_suite.rs` 的
> `order_by_an_output_column_number_sorts_by_that_column` 连顺序与 `EXPLAIN`
> 一起核）。

> **`ydbms-test/` 现在真的跑起来了**：`tests/ydbms_suite.rs` 走
> `init.sql → filmdb.sql → correctness/queries.sql`，断言零失败、11 张表的行数
> （`movies` 9204、`credits` 46850、`people` 16489…），并且把 25 条查询的结果与
> 仓库自带的**参照结果** `correctness/expect/query*.txt`（PostgreSQL 18.6 的 psql
> 输出）逐条比对——只归一化对齐、`NULL` 的呈现与行序（那些查询本来就没有
> `ORDER BY`），值必须一致。
> 只有 `tests.sql` 没进默认测试：它的 Test 1.2 是 `movies × countries`（约 170 万
> 行），而引擎会把整个结果表物化再渲染成一个字符串——那要几百 MB 内存，
> 属于手动验收。

---

# 阶段 1 —— 跑通 project1 的测试套件

目标：`ydbms-test/` 能跑完 —— `init.sql`（建库）→ `filmdb.sql`（7.6 万条数据）→
`tests.sql`（12 组）与 `correctness/queries.sql`（25 条，带 `expect/` 文件）。

**分层**（已经摆正）：`cmd/` 是**执行引擎**——接受任务（交互式终端敲进来的
一段、`\i` 读进来的内容）、驱动 SQL、管两路输出；`ir/` 是引擎里的 **SQL 部分**——解析、
降级、执行。交互式终端（行编辑、Tab 补全、多行）也在 `cmd/` 下，`main.rs` 只剩一行。

**已实测确认能用的**（别重做）：

- schema 限定名 `study.movies`，以及 `CREATE SCHEMA` / `SET search_path`（**真生效**：
  非限定名按 `path[0]` 加前缀）/ `DROP SCHEMA … CASCADE`；`BEGIN`/`COMMIT` 接受为无操作、
  `ROLLBACK` 显式拒绝（TODO 1.5）；
- `CROSS JOIN`（Test 1.2 要）；
- 4 表连接、内/左外连接、自连接、`GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT`、聚合与 `COUNT(DISTINCT)`、
  `IS NULL`/`IS NOT NULL`、`CASE`、`ROUND`、`died - born` 这类整数运算；
- 1.4 那一整张清单：`IN` 列表、`BETWEEN`、`LIKE`/`ILIKE`、`~*`、`DISTINCT`、派生表、
  `COUNT(*) FILTER`、`::numeric`、`EXTRACT(YEAR FROM …)`/`CURRENT_DATE`、`UPPER`/`LEFT`/`CONCAT_WS`，
  以及子查询（标量 / `IN` / `EXISTS` / 相关 / `UNION` / `ANY`/`ALL`）；
- 元命令 `\i`、`\o <文件>`、`\echo`、`\dt`、`\timing`、`\set ON_ERROR_STOP`；
- **装载可行**：1 万条单行 INSERT 用 0.43 秒 → 7.6 万条约 **3.3 秒**。

## 1.7 【？】`tests.yql` 要不要支持

`ydbms-test/` 里还有一份 `tests.yql`，用的**不是 SQL**：

```text
AFFAIR test {
    LET m = study.movies;
    << m | JOIN co | SELECT *;
}
```

它与 `tests.sql` 是**同一套测试的两种写法**（Test 1.1 / 1.2 / 1.3 一一对应）。
这是另一门查询语言的实现，量级不小于整个 `irgen.rs`。

**需要确认它是否在范围内。** 若在，应单独开一个阶段，不能混在 SQL 里做。

---

# 阶段 3 —— 性能与架构

## 3.4 【难】连接重排 + 代价估算

需要统计信息、基数估算、顺序搜索、树重写。注意**一个新类型都省不掉也帮不上**——
`Op::Join` 已经能表达任意二叉树，拉取式也不再要求"树"或"先物化"，
缺的只是**决策过程**（以及决策要用的统计信息）。

- **量级**：一个项目（统计信息 + 代价 + 搜索）。
- **验收**：多表连接里，重排后的计划在实测上优于 `FROM` 的书写顺序。

# 附：外部依赖备忘

- 环境里有 **PostgreSQL 18.6**（`/usr/lib/postgresql/18`）。做这些改动时，
  可以像之前那样在 `/tmp` 起一个临时集群逐条对账——**不要碰用户自己在跑的 5432 集群**。
