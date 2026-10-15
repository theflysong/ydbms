# TODO

这份文件按**阶段**分组,组内按**难度**标注.每项都写清:为什么缺,缺在哪,改动量级,怎么验收.
**只留没做的**:做完的条目按惯例移出正文(事实落在代码的 `//!` 与各模块的 `AGENTS.md` 里).

阶段之间大致有依赖--**先让测试跑起来,再收正确性,最后动性能**.
阶段 4~6 是新开的一块(把引擎做成"带用户与库的服务化命令行"),依赖链是
**事务 → 持久化/目录 → 用户与 CLI**,与阶段 0~3 互不阻塞.
**阶段 4(真事务与原子提交),阶段 5(服务目录与库),阶段 6(用户系统与 `ysql`)
都已经做完**,这三节只剩下面几条收尾;其中 5.5 已经**明确不做**(理由写在那一节里).

难度标记:**[易]** 半天内,改动集中;**[中]** 一到两天,要动一两层;**[难]** 需要新机制或重写.

---

## 分组总览

| 阶段 | 目标 | 待办 | 已完成 |
|---|---|---|---|
| **0** | 跑通 `tests/script/db1/` 的 query4 / query5 | 0 | 0.1 `EXTRACT` / `ORDER BY <序号>`,0.2 子查询 |
| **1** | 跑通 project1 的测试套件(`ydbms-test/`) | 1 | 1.1 `\timing` / `\set`,1.6 REPL 多行,1.4 查询层特性,1.5 事务与 schema,1.2 DDL 约束与 `DEFAULT`,1.3 `DATE` + `length()` |
| **2** | 正确性收尾(与 PostgreSQL 的已知偏差) | 0 | 2.1 `CASE` 两分支放大,2.2 `CASE`/`AND`/`OR` 的惰性(修法 A) |
| **3** | 性能与架构(索引,哈希连接,下推,重排,UPDATE) | 1 | 3.0 拉取式执行模型,3.1 索引 / 索引嵌套循环,3.2 哈希连接,3.3 谓词下推,3.5 `UPDATE`,3.6 索引进目录 + 可空侧下推 |
| **4** | 真事务(`BEGIN` / `COMMIT` / `ROLLBACK`)与原子提交 | 0 | 4.1 写前整表快照 + 目录快照 + 逐条隐式提交,4.2 影子分页 + `commit.json` 重放 |
| **5** | 服务目录,数据库与"写完就落盘"(`--server-dist` / `-d`) | 0 | 5.1 库目录 / `catalog.json` / `CREATE`·`DROP DATABASE`,5.2 打开即载入 + 只写脏表的提交点,5.3 服务目录与单写者锁(随阶段 6 一起做),5.4 库级会话默认值(`ALTER DATABASE ... SET search_path`) |
| **6** | 用户系统(`src/usr/`)与 `ysql` 命令行 | 1 | 6.1 用户表与认证,6.2 `ysql` 命令行,6.3 端到端验收与文档 |

**当前基线**(改动前先确认还成立):

```text
db.sql 52/0 · query1 4/0 · query2 5/0 · query3 8/0 · query4 6/0 · query5 9/0
(格式:成功 / 失败)
cargo test --offline → 584 通过(1 个 #[ignore] 的基准),0 warning;约 12 秒
(其中 tests/ydbms_suite.rs 的端到端装载 + 25 条查询约 9 秒)
执行模型:拉取式 -- ir::exec 的每个节点产出一个 RowSource,
          只有需要随机访问 / 重扫 / 哈希 build / 最终结果才物化(见 ir/exec.rs)
访问路径:WHERE 的合取项下推到单表 Scan(LEFT JOIN 的可空侧只放行 null-rejecting 的
          谓词,且连接之上仍留一份);索引是**目录对象**(CREATE INDEX / DROP INDEX
          建与删,INSERT 追加,UPDATE 作废重建),只有目录里真有的索引才走;
          连接有嵌套循环 / 哈希 / 索引嵌套循环三种策略(见 ir/plan.rs 的 JoinStrategy)
连接规模:两张 N 行的表等值连接(debug 档),N=2000 时嵌套循环 1.63 秒 →
          哈希 6.0 毫秒,索引嵌套循环 4.3 毫秒,N 翻倍后者约 ×2
          (跑 tests/ir_join.rs 里 #[ignore] 的 join_scaling_benchmark 可复现;
          手写计划的索引嵌套循环走"本地槽位"那条兜底,与目录里的索引同价)
子查询:内层是一份**自足**的 Plan(Step::Subquery / Step::Outer),执行时按外层行
        重开一份源,未相关的缓存内层**行**(见 ir/exec.rs 的 ExecCtx);
        派生表走 PlanBuilder::splice,UNION [ALL] 走 Op::Union
表达式惰性:CASE 的两个分支与 AND/OR 的右侧是**自足子程序**(Step::Case / And / Or),
        只有被选中时才求值(死分支里的除零不再冒出来);AND/OR 按书写顺序,
        左侧能定胜负就不算右侧(见 ir/expr.rs 与 tests/ir_lazy.rs)
DDL/DML:CREATE TABLE 的 NOT NULL / DEFAULT / CHECK **真强制**(CHECK 见 ir/checks.rs),
        PRIMARY KEY / UNIQUE / FOREIGN KEY 接受并**如实记录为未强制**;
        INSERT 缺列补 DEFAULT(没有就补 NULL),UPDATE 逐行改(值在旧行上求值,
        整行一次落库,先整体作废索引)
类型:INT / TEXT / CHAR / VARCHAR / TIMESTAMP / DATE / NUMERIC(date 定宽 YYYY-MM-DD)
模块:ir/value.rs(值层),ir/irgen.rs(降级),ir/opt/(优化器),ir/checks.rs(CHECK
        强制点),usr/(用户与库)
交互式终端:cmd/ -- rustyline 行编辑 + Tab 补全 + 多行累积,
          引擎与 SQL 部分的分工见"阶段 1"开头
用户与库:usr/ -- 服务目录(默认 ~/.ydbms):users.json + 各库目录;
          users.json 存的是**加盐 SHA-256**(sha2 依赖,盐来自 /dev/urandom),没有明文;
          库就是目录,<库>/catalog.json 记 owner 与**库级 settings.search_path**;每个库
          一把 flock(File::try_lock),第二个写者直接连不上;口径是只做认证 +
          "owner(或管理员)才能连";`ALTER DATABASE <库> SET search_path TO ...` 写的就是
          那条 settings,**打开库时**由引擎套成会话的初始 search_path(只影响以后的连接)
命令行:main.rs(ydbms,内存模式,行为未变)与 src/bin/ysql.rs(服务模式:
        cmd/cli.rs 解析 -U / -d / -f / --server-dist / -h;密码从 PGPASSWORD 来;
        -f 与 \i 同一条路;退出码 0 / 1)
文案:rust-i18n -- locales/zh-CN.yml 是**源文案**(515 条),locales/en.yml 是英文译文;
      调用点一律 `tr!("模块.文件.序号", 名字 = 表达式)`(占位符在文案表里写 `%{名字}`),
      默认语言 zh-CN,`YDBMS_LANG` -> `LC_ALL` -> `LC_MESSAGES` -> `LANG` 可以改;
      tests/i18n.rs 核对四件事:键与代码一一对应,中英两份的键与占位符一致,src 的
      字符串字面量里不许再有中文,注释/文案/我们自己的 *.md 里不许出现有半角对应体的
      全角标点(口径与映射表见根 AGENTS.md 的 "六,文案与标点")
事务:BEGIN / COMMIT / ROLLBACK **真生效**--撤销记录在符号表里:事务开始时快照目录
      (符号名,索引,schema,search_path),事务里**第一次**要写或要删某张表之前
      整表克隆一份(Table::snapshot),ROLLBACK 按快照重建(DDL 一起回滚,回滚后索引
      整体作废);没有显式 BEGIN 时**每条语句自成一个隐式事务**(成功即提交,失败即
      回滚这一句),BEGIN 会把这个隐式事务升级成显式;BEGIN 套 BEGIN 与无事务的
      COMMIT/ROLLBACK 照 PostgreSQL 只发警告(不是错误);没有 SAVEPOINT / 嵌套事务 /
      "事务已中止"状态,事务里语句失败不整句回滚(要撤销就 ROLLBACK),退出会话时
      事务还开着即丢弃(等于 ROLLBACK,与 PostgreSQL 断开连接一致)
提交点:**一条语句一次**(事务里空转,提交点是 COMMIT)--所以**整段装载要包在
      `BEGIN; ... COMMIT;` 里**:不包的话每条写语句都要把整张表重写一遍
      (`filmdb.sql` 本来就把整段包着).一次提交是**原子的**--
      先把每张表写成 <表>.csv.new 暂存文件,再落下 <库>/commit.json 提交意图,然后
      逐个 rename 进正式位置,删掉要删的表,删掉意图;打开库时(拿到锁之后,载入之前)
      有残留的 commit.json 就**补完**它,没有意图庇护的 .new 一律清扫--所以崩在
      中途只可能"全在提交前"或"全在提交后",不会是混合;不做 WAL,也不 fsync
      (只保证进程崩溃,不保证掉电).
      打开即载入(SymbolTable::extend),提交点只写脏表;CREATE/DROP DATABASE 与
      ALTER DATABASE 经 ir 的 Databases 口子落到 usr::service(内存模式由 NoDatabases
      明确拒绝,事务里也明确拒绝);脏表按**表自己的生效名**精确取(不按当时的
      search_path 再解一次),而**表名 / 库名要能当文件名主干**
      (db::table::is_file_stem_safe:空,`.`,`..`,含 `/`,`\` 一律拒绝)--否则
      `CREATE TABLE "../别人的库/evil"` 能写到库目录外面
```

> **测试台只数成败,不比对结果.** 所以"某条查询不报错"不等于"算对了"--
> 0.1 的 `ORDER BY <序号>` 曾经是活例子:它不报错,但排的是常量.
> 现在它真的有测试盯着(`tests/db1_suite.rs` 的
> `order_by_an_output_column_number_sorts_by_that_column` 连顺序与 `EXPLAIN`
> 一起核).

> **`ydbms-test/` 现在真的跑起来了**:`tests/ydbms_suite.rs` 走
> `init.sql → filmdb.sql → correctness/queries.sql`,断言零失败,11 张表的行数
> (`movies` 9204,`credits` 46850,`people` 16489...),并且把 25 条查询的结果与
> 仓库自带的**参照结果** `correctness/expect/query*.txt`(PostgreSQL 18.6 的 psql
> 输出)逐条比对--只归一化对齐,`NULL` 的呈现与行序(那些查询本来就没有
> `ORDER BY`),值必须一致.
> 只有 `tests.sql` 没进默认测试:它的 Test 1.2 是 `movies × countries`(约 170 万
> 行),而引擎会把整个结果表物化再渲染成一个字符串--那要几百 MB 内存,
> 属于手动验收.

---

# 阶段 1 -- 跑通 project1 的测试套件

目标:`ydbms-test/` 能跑完 -- `init.sql`(建库)→ `filmdb.sql`(7.6 万条数据)→
`tests.sql`(12 组)与 `correctness/queries.sql`(25 条,带 `expect/` 文件).

**分层**(已经摆正):`cmd/` 是**执行引擎**--接受任务(交互式终端敲进来的
一段,`\i` 读进来的内容),驱动 SQL,管两路输出;`ir/` 是引擎里的 **SQL 部分**--解析,
降级,执行.交互式终端(行编辑,Tab 补全,多行)也在 `cmd/` 下.

**已实测确认能用的**(别重做):

- schema 限定名 `study.movies`,以及 `CREATE SCHEMA` / `SET search_path`(**真生效**:
  非限定名按 `path[0]` 加前缀)/ `DROP SCHEMA ... CASCADE`;`BEGIN`/`COMMIT`/`ROLLBACK`
  真生效(见下面的基线);
- `CROSS JOIN`(Test 1.2 要);
- 4 表连接,内/左外连接,自连接,`GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT`,聚合与 `COUNT(DISTINCT)`,
  `IS NULL`/`IS NOT NULL`,`CASE`,`ROUND`,`died - born` 这类整数运算;
- 查询层那一整张清单:`IN` 列表,`BETWEEN`,`LIKE`/`ILIKE`,`~*`,`DISTINCT`,派生表,
  `COUNT(*) FILTER`,`::numeric`,`EXTRACT(YEAR FROM ...)`/`CURRENT_DATE`,`UPPER`/`LEFT`/`CONCAT_WS`,
  以及子查询(标量 / `IN` / `EXISTS` / 相关 / `UNION` / `ANY`/`ALL`);
- 元命令 `\i`,`\o <文件>`,`\echo`,`\dt`,`\timing`,`\set ON_ERROR_STOP`;
- **装载可行**:1 万条单行 INSERT 用 0.43 秒 → 7.6 万条约 **3.3 秒**.

## 1.7 [?]`tests.yql` 要不要支持

`ydbms-test/` 里还有一份 `tests.yql`,用的**不是 SQL**:

```text
AFFAIR test {
    LET m = study.movies;
    << m | JOIN co | SELECT *;
}
```

它与 `tests.sql` 是**同一套测试的两种写法**(Test 1.1 / 1.2 / 1.3 一一对应).
这是另一门查询语言的实现,量级不小于整个 `irgen.rs`.

**需要确认它是否在范围内.** 若在,应单独开一个阶段,不能混在 SQL 里做.

---

# 阶段 3 -- 性能与架构

## 3.4 [难]连接重排 + 代价估算

需要统计信息,基数估算,顺序搜索,树重写.注意**一个新类型都省不掉也帮不上**--
`Op::Join` 已经能表达任意二叉树,拉取式也不再要求"树"或"先物化",
缺的只是**决策过程**(以及决策要用的统计信息).

- **量级**:一个项目(统计信息 + 代价 + 搜索).
- **验收**:多表连接里,重排后的计划在实测上优于 `FROM` 的书写顺序.

# 阶段 5 -- 服务目录与"写完就落盘"的收尾

5.1 / 5.2 / 5.3 / 5.4 已经做完(库就是目录 + `catalog.json`,打开即载入,只写脏表的
提交点,单写者锁,**库级 `search_path` 默认值**),口径与磁盘布局写在
`src/usr/AGENTS.md` 与 `cmd/engine.rs` 的文件头里.
下面这条是当时**有意留着不做**的,现在**明确不做**.

## 5.5 [中]数据文件:追加 + 定期整理(明确不做)

**缺在哪**:`CsvPersister::save` 每次都**整表重写**(暂存 + rename).提交点只写脏表
之后已经够用(一次 `-f filmdb.sql` 只在末尾写 11 张表),但"改一行就重写整张表"在写多
的场景下仍是 O(n²).

- **要点**:数据文件改成"追加 + 定期整理"(段文件或 WAL,读到一定量再整理一次),
  磁盘格式与 `catalog.json` 一起升 `version`.
- **为什么不做**:真要做是一个独立项目(格式 + 整理 + 迁移 + 一整套读写测试),而且要动
  `pers` 的**磁盘格式契约**(见 `src/pers/AGENTS.md` 的"格式是契约")--那只该由一个
  独立的格式版本去做.写多的场景眼下有更省事的正路:把写入包在 `BEGIN; ... COMMIT;` 里
  (提交点只写脏表这条已经能省掉绝大部分重写).
- **做之前要先定**:段文件怎么整理,崩在中途算什么(现在"一次提交是原子的"这条保证
  是格式换掉之后最容易丢的).
- **量级**:一个项目.
- **验收**:10 万行的表里改一行,提交只写那一行(对比:现在是一次整表重写).

# 阶段 6 -- 用户系统的收尾

6.1 / 6.2 / 6.3 已经做完(`src/usr/` 的用户表与认证,`ysql` 的
`-U` / `-d` / `-f` / `--server-dist`,跨进程端到端与文档),口径写在
`src/usr/AGENTS.md` 与 `src/cmd/cli.rs` 的文件头里.下面这条是当时**明确列为不做**的.

## 6.4 [难]多用户共享:`CREATE USER` / `GRANT` / `REVOKE` 与 `\c` / `\l`

**缺在哪**:本版只做认证 + "owner(或管理员)才能连".用户只能靠 `UserStore::add`
或手工编辑 `users.json` 加;权限只有"连不连得上"这一档,库里没有表级授权;换库要退出
重连(没有 `\c`),列库只能靠 `-d` 报错时打出来的那一串(没有 `\l`).

- **要点**:先定**授权信息存哪儿**(`<库>/grants.json`,还是 `users.json` 里的角色),
  再谈表级权限;`\c <库>` 换库时要连**单写者锁**一起换(旧库得能被别人连上),
  `\l` 列出服务目录下的库与 owner.
- **需要决定**:权限粒度(库级 / 表级 / 列级)与"管理员"的边界;本版把
  `GRANT`/`REVOKE` 列为不做,就是因为还没想清楚这两件事.
- **量级**:一个项目(授权模型 + 元命令 + 端到端).
- **验收**:表级 `GRANT SELECT` 之后另一个人 `SELECT` 得过,`INSERT` 被拒;
  `\c filmdb` 之后旧库的锁已释放(另一个进程能连上).

# 附:外部依赖备忘

- 环境里有 **PostgreSQL 18.6**(`/usr/lib/postgresql/18`).做这些改动时,
  可以像之前那样在 `/tmp` 起一个临时集群逐条对账--**不要碰用户自己在跑的 5432 集群**.
