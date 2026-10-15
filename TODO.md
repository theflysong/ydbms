# TODO

这份文件按**阶段**分组，组内按**难度**标注。每项都写清：为什么缺、缺在哪、改动量级、怎么验收。
阶段之间大致有依赖——**先让测试跑起来，再收正确性，最后动性能**。
阶段 4~6 是新开的一块（把引擎做成"带用户与库的服务化命令行"），依赖链是
**事务 → 持久化/目录 → 用户与 CLI**，与阶段 0~3 互不阻塞。

> 做阶段 6 时把阶段 5 的 5.1 / 5.2 / 5.3 **一起做了**（服务目录、库目录、
> `catalog.json`、只写脏表的提交点、单写者锁）——6.1 的"owner 才能连"与 6.3 的
> "再起一个进程查回来"离了它没法验收。**阶段 4（真事务）仍然没做**，所以提交点落在
> "一次 `run_with`"（一次提交）上，见阶段 5 的说明。

难度标记：**【易】** 半天内、改动集中；**【中】** 一到两天、要动一两层；**【难】** 需要新机制或重写。

---

## 分组总览

| 阶段 | 目标 | 待办 | 已完成 |
|---|---|---|---|
| **0** | 跑通 `tests/script/db1/` 的 query4 / query5 | 0 | 0.1 `EXTRACT` / `ORDER BY <序号>`、0.2 子查询 |
| **1** | 跑通 project1 的测试套件（`ydbms-test/`） | 1 | 1.1 `\timing` / `\set`、1.6 REPL 多行、1.4 查询层特性、1.5 事务与 schema、1.2 DDL 约束与 `DEFAULT`、1.3 `DATE` + `length()` |
| **2** | 正确性收尾（与 PostgreSQL 的已知偏差） | 0 | 2.1 `CASE` 两分支放大、2.2 `CASE`/`AND`/`OR` 的惰性（修法 A） |
| **3** | 性能与架构（索引、哈希连接、下推、重排、UPDATE） | 1 | 3.0 拉取式执行模型、3.1 索引 / 索引嵌套循环、3.2 哈希连接、3.3 谓词下推、3.5 `UPDATE`、3.6 索引进目录 + 可空侧下推 |
| **4** | 真事务（`BEGIN` / `COMMIT` / `ROLLBACK`） | 2 | — |
| **5** | 服务目录、数据库与"写完就落盘"（`--server-dist` / `-d`） | 0 | 5.3 服务目录与单写者锁、5.1 库目录 / `catalog.json` / `CREATE·DROP DATABASE`、5.2 打开即载入 + 只写脏表的提交点（随阶段 6 一起做） |
| **6** | 用户系统（`src/usr/`）与 `ysql` 命令行 | 0 | 6.2 `ysql` 命令行、6.1 用户表与认证、6.3 端到端验收与文档 |

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
用户与库：usr/ —— 服务目录（默认 ~/.ydbms）：users.json + 各库目录；
          users.json 存的是**加盐 SHA-256**（sha2 依赖，盐来自 /dev/urandom），没有明文；
          库就是目录，<库>/catalog.json 记 owner；每个库一把 flock（File::try_lock），
          第二个写者直接连不上；口径是只做认证 + "owner（或管理员）才能连"
命令行：main.rs（ydbms，内存模式，行为未变）与 src/bin/ysql.rs（服务模式：
        cmd/cli.rs 解析 -U / -d / -f / --server-dist / -h；密码从 PGPASSWORD 来；
        -f 与 \i 同一条路；退出码 0 / 1）
落盘：打开即载入（SymbolTable::extend）、**提交点只写脏表**；一次 run_with = 一次提交
      （一个 piece：敲进来的一段、\i 或 -f 读进来的一个文件）；CREATE/DROP DATABASE
      经 ir 的 Databases 口子落到 usr::service（内存模式由 NoDatabases 明确拒绝）；
      脏表按**表自己的生效名**精确取（不按当时的 search_path 再解一次），
      而**表名 / 库名要能当文件名主干**（`db::table::is_file_stem_safe`：空、`.`、`..`、
      含 `/`/`\` 一律拒绝）——否则 `CREATE TABLE "../别人的库/evil"` 能写到库目录外面
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

# 阶段 4 —— 真事务

目标：`BEGIN` / `COMMIT` / `ROLLBACK` 真生效。现在前两个只是**如实记录的空操作**、
`ROLLBACK` 在降级时就被拒绝（见 `ir/irgen.rs` 的 `lower_transaction` 与 `ir/stmt.rs`
的 `Stmt::Transaction`），而 `Table::insert` / `update_row` 都是**就地改**内存表，
没有任何撤销记录。

> 这一段是「写完就落盘」（阶段 5）的前提：`pers::CsvPersister::save` 每次都**整表
> 重写**（tmp + rename，单张表是原子写 ✓），所以"每条 INSERT 都存一次"在 7.6 万行的
> 装载上是 **O(n²)**（约 30 亿次行写入）。而 `filmdb.sql` 本来就把整段包在
> `BEGIN TRANSACTION; … COMMIT;` 里——有真事务之后，那次装载只在提交点落一次盘。

## 4.1 【难】事务：写前快照 + 脏表集合

**缺在哪**：`Stmt::Transaction { begin }` 只回一行状态；`ROLLBACK` 直接报"本版没有
事务"；DML 全是就地改。

- **要点**：事务里**第一次**写某张表之前，把它整表克隆一份进 `undo`；`ROLLBACK` 用
  克隆盖回去；`COMMIT` 落盘并丢掉 `undo`。**DDL 也要能回滚**——`CREATE TABLE` /
  `CREATE INDEX` / `DROP SCHEMA` 改的是符号表，undo 里要连目录一起留一份：
  只回滚数据、不回滚目录，是比不实现事务更糟的一种错。
- **需要决定**：① 行级 undo 日志 vs 整表快照（前者省内存、后者三十行就能写对；
  **建议先整表快照**，并在文档里写明"大表事务的代价 = 一次克隆"）；② 自动提交口径
  （**建议**：没有显式 `BEGIN` 时**一条语句 = 一个隐式事务**，成功即提交——这才是
  用户要的"写完就落盘"）；③ `BEGIN` 套 `BEGIN`、无事务时 `COMMIT` / `ROLLBACK` 的
  报错口径（照 PostgreSQL 发 WARNING，还是本版一贯的显式报错）。
- **量级**：约 350 行（`stmt.rs` 的事务状态 + `engine.rs` 的隐式提交 + 符号表的撤销点）。
- **验收**：`BEGIN; INSERT …; ROLLBACK` 之后那行不在、`\dt` 的行数回到原样；
  `BEGIN; CREATE TABLE x …; ROLLBACK` 之后 `x` 不存在；没有事务时 `ROLLBACK` 的报错
  明确；`filmdb.sql` 的装载**只落盘一次**（用阶段 5 的口径验）。

## 4.2 【中】提交的原子性与崩溃口径

**缺在哪**：`pers` 一次只写一张表（单张表原子 ✓），多表提交**崩在中间**会留下"改了
一半"的库；磁盘上也没有任何"这次提交进行到哪"的痕迹。

- **要点**：提交前先写一份**提交意图**（`<库>/commit.json`：这次要写哪几张表），
  逐表 rename 之后再删掉它；启动时看到残留就按"重放"或"报错要求人工处理"处理
  （二选一，写进文档）。
- **需要决定**：要不要 WAL。**建议不做**：本版口径定成"提交即持久，崩溃只可能丢掉
  最后一次没写完的提交"；WAL 是另一个项目（把"为什么不做"写清楚，比含糊留着好）。
- **量级**：约 150 行 + 一个"提交到一半 `kill -9`"的手工验收。
- **验收**：提交过程中 `kill -9`，重启后库要么全在提交前、要么全在提交后，**不会**
  是两者之间的混合；11 张表的 `filmdb.sql` 装载不会留下半截。

# 阶段 5 —— 服务目录、数据库与"写完就落盘"（`--server-dist` / `-d`）

目标：`ysql --server-dist ~/.ydbms -d filmdb` 打开的是**磁盘上的一个库**；
`INSERT` / `UPDATE` 之后数据真的落盘；`~/.ydbms/` 下放着用户表与各库的数据。

> **已完成**（5.1 / 5.2 / 5.3 随阶段 6 一起做的）。落地的口径：
> 库就是目录（`TablePath` 直接复用）、`-d` **显式**指向不存在的库报错并列出已有的库、
> 没有 `-d` 时默认库（与用户同名）不存在就自动建、`--server-dist` 默认 `~/.ydbms`
> （`$HOME` 取不到就报错）、库目录一把 `flock`（第二个写者直接连不上）。
> **"写完就落盘"的代价口径要记牢**：阶段 4（真事务）没做，所以提交点是
> **一次 `run_with`**——交互式敲进来的一段、`\i` 或 `-f` 读进来的一个文件。
> 提交点只写**脏表**，于是 `ysql -f filmdb.sql` 那种一整段装载只在末尾落一次盘；
> 而内存模式（`ydbms`、`ydbms-test/` 那套 7.6 万行装载）**一行 IO 都不做**。
> **没做**：`commit.json`（见 4.2）与"追加 + 定期整理"的数据文件——照本节的建议留着。

> **地基已经有了，接上就行**：`pers` 的布局本来就是"**一个目录 = 一个库**"
> （`<dir>/<表名>.csv` + `<表名>.schema.json`，见 `src/pers/mod.rs` 的文件头），写入
> 是 tmp + rename；`SymbolTable::extend` 就是为"装载时批量导入"准备的入口。
> **现状是 `CsvPersister` 只有测试在调，引擎一行都没接。**

## 5.1 【中】库目录、目录发现与 `CREATE/DROP DATABASE`

- **要点**：扫一个库目录（`*.schema.json`）得到表清单 → `CsvPersister::load` →
  `SymbolTable::extend`；`CREATE DATABASE` / `DROP DATABASE`（语法树里是
  `CreatedbStmt { dbname, options }` / `DropdbStmt { dbname, missing_ok, options }`，
  已核对）在服务目录下建/删子目录；库自己的元数据写 `<库>/catalog.json`
  （owner、创建时间、格式版本），**别**和表文件混在一起。
- **需要决定**：① 库就是目录（**建议是**：与 `pers` 的既有口径一致，也方便手工检查）；
  ② `-d` 指向不存在的库是报错还是自动创建（**建议报错**，建库只有 `CREATE DATABASE`
  一条路）。
- **量级**：约 300 行（`pers` 加一个目录扫描 + `ir/stmt.rs` 两条语句 + `engine.rs`
  的"打开一个库"）。
- **验收**：`CREATE DATABASE d` 之后 `~/.ydbms/d/` 出现；建表插数据、退出、再启动
  `-d d`，`\dt` 与行数都在；`DROP DATABASE d` 之后目录没了；`-d` 指向不存在的库
  明确报错。

## 5.2 【中】"写完就落盘"：只写脏表

- **要点**：`Session` 记住"这一轮事务写了哪几张表"（写就标脏，DDL 让相应表也脏）；
  **提交点**把脏表逐张 `save`，并把 DDL 的结果同步到磁盘（新表的 schema 文件、
  被 `DROP SCHEMA … CASCADE` 清掉的表文件）。只写脏表是关键——不然每次提交都是
  整库重写。
- **需要决定**：数据文件要不要改成"追加 + 定期整理"（省掉整表重写）。**建议先不做**：
  事务把装载收敛成一次提交、提交只写脏表，已经够用；真要改是另一个项目。
- **量级**：约 250 行（`engine.rs` 的提交点 + 脏标记 + 删文件）。
- **验收**：`INSERT` 之后**立刻** `kill -9`，重启数据还在（跨进程，不是同一进程里换
  会话）；`filmdb.sql` 的 7.6 万行装载不被落盘拖垮（对照：现在约 3.3 秒）。

## 5.3 【易】服务目录的初始化与单写者锁

- **要点**：`--server-dist` 默认 `~/.ydbms/`（`HOME` 取不到就报错，**别猜**）；首次
  使用自己建出目录树；库目录上放一把**单写者锁**（`std::fs::File::try_lock`，Rust
  1.89+ 的标准库能力，**不用加依赖**——本机 1.99 实测两把句柄互斥），第二个进程连
  同一个库直接报错。本版没有多进程协调：两个写者会互相踩，宁可连不上，也不要写坏
  数据。
- **量级**：约 120 行。
- **验收**：同一个库开两个 `ysql`，第二个明确报错并非零退出；`--server-dist /tmp/x`
  首次使用能自己建出目录树；`HOME` 缺失时给出明确错误。

# 阶段 6 —— 用户系统（`src/usr/`）与 `ysql` 命令行

目标：`ysql -U bob -d filmdb --server-dist ~/.ydbms -f script.sql` 能跑；密码从
`PGPASSWORD` 环境变量来；没有 `-f` 就是交互式。

> **已完成**（6.1 / 6.2 / 6.3），验收都对过了：跨进程"建库 → 装数据 → 再起一个进程
> 查回来"、错密码/未知用户/缺 `PGPASSWORD`（非交互）各自非零退出、`users.json` 里
> 搜不到明文、同一个库的第二个写者连不上、`ysql -f` 与 REPL 里 `\i` 的输出一致。
> 落地时的几个口径（都写进了文档注释与 `src/usr/AGENTS.md`）：
>
> * 服务目录布局：`<server-dist>/users.json` + `<库>/catalog.json|.lock|<表>.csv|.schema.json`；
> * **首次启动 bootstrap**：`users.json` 不存在时把这次 `-U` 建成管理员，
>   并把默认库（与用户同名）一起建出来——否则新服务目录里谁都进不去；
> * **没有 `CREATE USER` / `GRANT` / `REVOKE`**（本节明确列的不做）：第二个用户靠
>   `UserStore::add` 这个库 API 或手工编辑 `users.json` 加进去；
> * 空口令与重名都拒绝；`search_path` **不落盘**，跨进程查询要写限定名或自己 `SET`。

> **本版没有做**：真事务（阶段 4）、`commit.json`（4.2）、库级 `search_path` 持久化、
> 多用户共享与授权、`\c` / `\l` 这类"切库/列库"的元命令（库列表在 `-d` 报错时会打出来）。

> 用户系统放在**新的 `src/usr/`**（与 `cmd/` `db/` `ir/` `pers/` 并列；这里按
> "`usr/` = crate 内的 `src/usr/`"理解）。它与阶段 4 / 5 几乎不耦合：可以先做 6.2 的
> 参数骨架，再回头接上认证。

## 6.1 【中】用户表与认证

- **要点**：`User { name, password_hash, salt, is_admin, created_at }` 存
  `<server-dist>/users.json`（serde 已有）；`authenticate(name, password)`；
  **首次启动的 bootstrap**：`users.json` 不存在时，把这次 `-U` 指定的用户建成管理员
  并打一行状态（否则谁都进不去）；库有 owner（阶段 5 的 `catalog.json`）。本版口径
  **只做认证 + "owner 才能连"**，`GRANT` / `REVOKE` 与多用户共享**明确列为不做**。
- **需要决定**：密码怎么存——① 加一个小依赖（`sha2` + 每用户随机盐）② 手写
  SHA-256（纯算法、有公开测试向量，但"不要自己造密码学"）。**建议 ①**，理由与取舍
  写进 `usr/mod.rs` 的文件头。**不要存明文**：`users.json` 就在用户主目录里，明文比
  加盐哈希危险得多。
- **量级**：约 300 行（含哈希与它的测试向量）。
- **验收**：对密码进得去、错密码进不去（非零退出）；`users.json` 里看不到明文；
  没有 `PGPASSWORD` 时的行为明确（**建议**：交互式提示输入、非交互直接报错）。
- **已完成**：取了 **①**（`sha2 = "0.10"`，解析到 0.10.9，`cargo fetch` 之后
  `--offline` 照旧可跑）；盐是 `/dev/urandom` 的 16 字节（不为它再加 `getrandom`），
  哈希是 `sha256(salt ‖ password)` 的小写十六进制，比较走常量时间。
  **它不是 KDF**（没有迭代拉伸）——这句话写在 `src/usr/hash.rs` 的文件头，
  升级的落点是 `users.json` 的 `version`。盐与口令之外还拒绝了空口令与空用户名。

## 6.2 【中】`ysql` 命令行：`-U` / `-d` / `-f` / `--server-dist`

**缺在哪**：`main.rs` 只有一行 `cmd::repl::run()`，**完全不解析参数**；REPL 入口自己
`Session::new()`（内存里的空库）；"跑完就退"的脚本路径现在只能靠 `printf … | ydbms`。

- **要点**：手写参数解析（现有依赖里没有 `clap`，AGENTS.md 说"能不加就不加"）：
  `-U <用户>` / `-d <库>` / `-f <脚本>` / `--server-dist <目录>` / `-h`（`--server-dist`
  是本项目自己的说法——服务目录，psql 没有这个选项；`-U` / `-d` / `-f` 则照 psql 的
  习惯）；`-f` 走 `Session::run_file`（与 `\i` 同一条路，非交互），没有 `-f` 才进
  rustyline。
  `-U` 默认 `$USER`、`-d` 默认与 `-U` 同名（照 psql 的习惯，写进 `--help`）。
- **需要决定**：二进制叫什么——① `Cargo.toml` 加 `[[bin]] name = "ysql"`（lib 仍叫
  `ydbms`）② 改现有 bin 的名字、留 `ydbms` 当别名。**建议 ①**：`cargo build` 直接得到
  `target/debug/ysql`，测试里也能用 `CARGO_BIN_EXE_ysql`。
- **量级**：约 250 行（`main.rs` 变薄，解析与 `--help` 放 `cmd/cli.rs`）。
- **验收**：`ysql --help` 列出四个参数与默认值；未知参数报错并非零退出；
  `ysql -U bob -d filmdb -f <脚本>` 与在 REPL 里 `\i` 同一份脚本结果一致。
- **已完成**：取了 **①**，但**没有改 `Cargo.toml`**——`src/bin/ysql.rs` 由 cargo 自己
  发现就是 `ysql`，lib 仍叫 `ydbms`，`CARGO_BIN_EXE_ysql` 直接可用。
  `ydbms` 那个二进制**一个字没动**（内存模式的实验终端），现有测试与 `ydbms-test/`
  因此都没受影响。解析与默认值是**纯函数**（`cmd::cli::{parse, resolve}`，环境用闭包
  传进去），于是"没有 `$USER` / 没有 `$HOME`"也能单测。退出码：0 正常；1 用法 /
  认证 / 打开库失败，或 `-f` 的脚本里报过错。

## 6.3 【易】端到端验收与文档

- **要点**：`tests/ysql_cli.rs` 用 `env!("CARGO_BIN_EXE_ysql")` 起**真进程**、把
  `--server-dist` 指到临时目录、用 `PGPASSWORD` 灌密码，跑一遍"建库 → 装数据 →
  **再起一个进程查回来**"；文档：`AGENTS.md` 加一句"用户 / 库 / 服务目录在
  `src/usr/`"，用法写进 `main.rs` 的文件头（现在那里只有 `\i` 的几条例子）。
- **量级**：约 200 行测试 + 文档。
- **验收**：`cargo test --offline` 里那几条真的**跨进程**；`~/.ydbms/` 的目录树在
  文档里画出来（`users.json` + `<库>/catalog.json` + `<库>/<表>.csv|.schema.json`）。
- **已完成**：`tests/ysql_cli.rs`（12 条真进程，含跨进程单写者锁与 `kill -9` 之后
  锁被内核释放）+ `tests/usr_auth.rs`（用户与口令，9 条）+ `tests/usr_service.rs`
  （服务目录、锁、落盘往返、`search_path` 挪位之后提交点仍存对表，19 条）+
  `tests/ir_database.rs`（5 条）+ `tests/ysql_args.rs`（9 条）：共 **54 条**新用例
  （`cargo test --offline` 从 501 条到 555 条，0 warning）。目录树画在 `src/main.rs` 的文件头；`AGENTS.md`
  加了"用户 / 库 / 服务目录在 `src/usr/`"这一句指路；`src/usr/AGENTS.md` 是新写的。
  测试**一律**用临时服务目录，不碰 `~/.ydbms`。

# 附：外部依赖备忘

- 环境里有 **PostgreSQL 18.6**（`/usr/lib/postgresql/18`）。做这些改动时，
  可以像之前那样在 `/tmp` 起一个临时集群逐条对账——**不要碰用户自己在跑的 5432 集群**。
