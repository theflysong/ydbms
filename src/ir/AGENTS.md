# AGENTS.md -- `src/ir`

## 职能

把 SQL 变成"要查什么",然后执行它.**计划与程序两层都是扁平的**(边是下标,
指令是数组),只有 `CASE`/`AND`/`OR` 的分支是自足子程序,求值多一层**有界递归**
(深度 = 表达式嵌套深度):

```text
SQL 文本
   │ pg_query::parse(PostgreSQL 的真实解析器)
   ▼
pg_query 裸语法树
   │ lower()      ← irgen.rs   绑定:查表,解列名,类型检查
   ▼
Plan { nodes: Vec<Node> }        ← plan.rs   节点之间的边是"下标"
   │ 每个算子各带若干
   ▼
Prog { steps: Vec<Step> }        ← expr.rs   一小段寄存器程序
   ▼
RowSource(一行一行拉)           ← exec.rs   拉取式执行
   ▼
PhysTab                          ← db::source::materialize
```

- `value.rs` -- **值层**:[`Scalar`](value.rs) / `Tri` / `ScalarType`.`Value` 本身
  在 `db::table`(存储层的值,见那里的说明).
- `opt/` -- **优化器**(`ir` 内部模块):谓词推到哪张表(`opt/pushdown.rs`,含
  null-rejecting 判定),连接怎么执行(`opt/join.rs`).**这里只决定,不落笔**.
- `checks.rs` -- `CHECK` 的强制点:一层 [`CheckedTable`](checks.rs) 装饰器.
- `symbol.rs` -- 名字 → 表/别名/索引的映射 [`SymbolTable`](symbol.rs).
- `stmt.rs` -- 语句与脚本:[`Script`](stmt.rs)**解析一次,`run_one` 一条**(降级仍在
  执行时做--后面的语句依赖前面建出来的表);`run_script` 就是它的循环包装.
  还放着一个给 REPL 用的词法小工具 `has_top_level_terminator`("这段输入里有没有
  一句说完"),与 `statement_text` / `strip_leading_comments` 同属一类活.

## 特殊规则

### 执行是拉取式的,惰性边界就这几处

`execute_source` 造管道,`execute` 抽干 root.每个节点产出的是一个 `RowSource`
(见 [`../db/AGENTS.md`](../db/AGENTS.md)),**不是物化的表**.只有下面这几处会攒行:

1. 需要**随机访问**的算子(`Sort`,`Aggregate`);
2. 嵌套循环连接的**内侧**(要重扫它),哈希连接的 **build 侧**(抽干成"键 → 行");
   索引嵌套循环**不攒行**--它只查一份"键 → 行号"的索引(目录里的,或者按需建的
   一份),行按需从基表取;
3. 被**多个父节点**引用的节点(物化一次,各消费者拿一个游标);
4. **最终结果**.

其它算子(`Scan`/`Filter`/`Project`/`Limit`)必须**流式**实现:`next()` 里一次只拉
输入的一行,别顺手 `drain`.拉取式的收益(`LIMIT` 之下不被求值,连接不为每一对
候选行拼行)全在这儿.

**管道只从 root 建**:从 root 走不到的节点不建源也不执行(静态校验仍然覆盖它们).

`next()` 返回 `None` 是"结束**或**出错",出错必须能由 `error()` 问到;复合算子不
抄输入的错误,而是 `self.input.error()` 往下问.

`Prog::eval_split` 是连接用的分裂输入求值:程序里的下标按 `左 ++ 右` 编,两半各取
各的,**不为每对候选行拼整行**.

### 索引是目录对象(TODO 3.6)

索引**只来自 `CREATE INDEX`**:`SymbolTable` 里记着名字,表与一个
[`IndexSlot`](../db/index.rs)("列 + 当前那一份索引").三条规矩:

- 优化器只挑**目录里真有**的索引(没有就顺序扫描)--本版不为"也许有用"在建计划时
  先把表扫一遍,那比顺序扫描还贵;
- `INSERT` 把新键**追加**进去(`SymbolTable::index_keys_of` + `maintain_indexes`),
  `UPDATE` **整体作废**(`invalidate_indexes`,下次用到时重建一次).作废必须
  **先做**:中途出错也不能留下拿旧行号回答新键的索引;
- `DROP SCHEMA ... CASCADE` 连同表一起清;索引**不落盘**.

### 连接键 / 索引键必须同族,索引键不能读列

哈希与索引都把 [`Value`](crate::db::table::Value) 当键,而 `Value::Int(1)` 与
`Value::Numeric("1")` **不是同一个值**--当键用会**安静地漏掉**本该匹配的行(嵌套循环
里的比较有 `widen_numeric` 兜着,键没有).所以绑定器只挑**同族**的等值键,
[`Plan::analyze`](plan.rs) 再强制一遍;索引的键还**不许引用列**(它在没有输入行的情况下
求值).这两条都属于"宁可不用这条路径,也不给安静的错答案".

下推也有同样性质的两条规矩:谓词只能推进**一个** FROM 项的基表;`LEFT JOIN` 的
**可空侧只放行 null-rejecting 的谓词**--`b.y = 5`,`b.y IS NOT NULL` 推得下去
(补出来的 NULL 行本来就不满足它们),`b.y IS NULL` 不行(补出来的 NULL 也满足).
而且可空侧推下去的那一份**不能取代连接之上那一份**:补出来的 NULL 行根本没被那张表
扫过,只有上面的谓词丢得掉它们.所以 `EXPLAIN` 里 `Scan(b, filter=[y = 5])` 与
`Filter([y = 5])` 会**同时**出现.(PostgreSQL 走另一条路:把这种 `LEFT JOIN`
简化成内连接,于是谓词可以彻底搬下去.本版不做连接类型重写.)

见 `irgen.rs` 的 `Range::nullable` 与 `opt/pushdown.rs`.

### 两条不变量,不许破

- **I1(拓扑序)**:`Node::inputs` 里的下标必须**严格小于**节点自身的下标.它保证
  图无环,并让 schema 推导与执行都是一次正向 `for` 循环--深计划因此不递归,不爆栈.
  **不要引入"回头边"**,哪怕看起来方便.
- **I2(先定义后使用)**:`Step` 读到的寄存器必须在此前被写过.

两条都由 [`Plan::analyze`](plan.rs) 静态检查.**降级出来的计划一定要过 `analyze`.**
它不一致时会报 `IrInvalid` 而不是安静地读错列--绕过它就等于放弃这道保护.

### 类型与求值

- **布尔与真值是 IR 私有的**:`Tri`,`ScalarType::Bool` 都**不进** `DataType`,
  `Value`,磁盘格式.真值不能作结果列.
- **谓词的结果是三值**(`Tri::True` / `False` / `Unknown`),不是 `bool`.
  `WHERE`/`HAVING`/`CASE` 只认 `Tri::True`;`IS NULL` 是唯一永远给确定真值的东西.
- **比较只放行同族,`NULL`,以及 `INT`↔`NUMERIC`**.这是 PostgreSQL 里唯一的隐式
  放大(`int → numeric`);`text = int`,`timestamp = text` 必须继续拒绝.
  放宽比较时**必须同时在运行时把 `Int` 转成 `Numeric`**--只改类型检查会让比较掉进
  `Value::cmp` 的"按大类排名"兜底,给出安静的错答案.
- **`||` 只放行文本**:两侧必须是 `TEXT`(或 `NULL`),结果是 `TEXT`,任一侧 NULL 就得
  NULL.非文本的一侧**不隐式转文本**--这是与 PostgreSQL 的**已知偏差**(那边有
  `text || anynonarray`,于是 `'a' || 1` 是 `a1`);脚本里用到的两处都是文本接文本.
  这条规则与 `LIKE` 族共用 `require_text_pair`(见 `expr.rs`).
- **`CASE` 的两分支走同一条放大规则**:`INT` 与 `NUMERIC` 统一成 `NUMERIC`,
  一侧是 `NULL` 就取另一侧(**不放大**).分支变惰性之后,另一侧的值不再是现成的,
  所以这条决策搬到了**降级**:`Step::Case::widen` 由那次类型探针算出来
  (`irgen.rs` 的 `widen_flag`),`Prog::verify` 再强制它与 `unify_branches` 的结果
  一致--对不上报 `IrInvalid`,**绝不放过一个"类型是 NUMERIC,值是 Int"的程序**.

### `CAST` / `::`:两个方向,以及"字面量定型"那一条捷径

`cast_type` 放行的是:

- **同族恒等**(`x::int`,`name::varchar(3)`);
- `INT → NUMERIC`(PostgreSQL 里唯一的隐式放大);
- **数值 / 日期 / 时间戳 → `TEXT`**:呈现就是 `Value` 的 `Display`,所以与"直接查
  那一列"渲染出来的字面**逐字相同**(`CAST(9.5 AS TEXT)` 与 `SELECT 9.5`);
- **`TEXT` → 数值 / 日期 / 时间戳**:类型上放行,**值到运行期才解析**
  (`DataType::parse`,于是 `NUMERIC(N,M)` 的标度,`DATE` 的规范形式,`TIMESTAMP`
  的定宽规范化全都免费拿到);解析不出来报 `EvalError::BadText`--与 PostgreSQL
  一样是**运行期**错(`'abc'::int`);
- `NULL → 任意`.

仍然**拒绝**的:`TIMESTAMP`/`DATE` → `INT`,`INT` → `DATE`,以及 `DATE` ↔ `TIMESTAMP`
互转(最后一条是**记录在案的偏差**,PostgreSQL 允许;口径见 `db::table` 的 `TypeFamily`).

> **`'literal'::TYPE` 有一条捷径,而它必须与列上那条路一致**:能按目标类型**隐式
> 定型**的字面量走 `irgen::lower_const`(在**降级期**按目标类型解析,解析不了就是
> 降级期错误),落不进去的走 `Step::Cast` 真转.判据只有一处
> (`irgen::const_coercible`),`lower_const` 与 `TypeCast` 那一支共用它--
> 各写一份就会出现"`1994::text` 在字面量上被当成整数,在列上被转成 `'1994'`"
> 这种怪事(实测踩过).

`UPPER` / `LOWER` 是同一族严格文本函数(`to_uppercase` / `to_lowercase`,按**字符**
折);参数不是文本就在降级期拒绝(PostgreSQL 也是 `function lower(integer) does not exist`).
  放行之后运行时得**真的转**(`widen_branches`),否则物化时会被 `TypeMismatch`
  顶回来.这条对照过 PostgreSQL 18.6:
  `pg_typeof(case when true then 1 else 1.5 end)` 就是 `numeric`.
- `Scope` 解出来的**扁平下标**必须与 `Plan::analyze` 推出来的 schema 完全一致.
  别名只影响限定名匹配,**不改列名**--改了就会不一致.

### DDL:什么真强制,什么只记录

`CREATE TABLE` 的口径(`filmdb.sql` 的 11 张表全靠它):

- **真强制**:列级 `NOT NULL`(`Table::validate`),`DEFAULT`(`Column::default`,
  INSERT 缺列时补,见 `lower_insert`),`CHECK`(`checks.rs` 的 `CheckedTable`);
- **接受但如实记进 `unenforced`**:`PRIMARY KEY` / `UNIQUE` / `FOREIGN KEY ...
  REFERENCES ...`(不做唯一性与引用检查;`ON DELETE` 的动作也没有语义--没有 DELETE);
- **拒绝**:`DEFERRABLE` / `INITIALLY DEFERRED` / `NOT VALID`,`IDENTITY`,
  `GENERATED`,`COLLATE`.

`CHECK` 与索引**不落盘**(它们不是列定义:`CHECK` 是程序,索引是会话级目录对象).
`DEFAULT` 在 [`Column`](crate::db::table::Column) 里,跟着模式文件走.

### `VACUUM` / `ANALYZE`:接受,但是**空转**

本版**没有统计信息**(基数估算与代价模型还是 3.4 的待办),也**没有要回收的空间**
(`UPDATE` 是原地替换,没有死元组),所以这两条语句只能空转.收下的是全部变体:
`VACUUM` / `VACUUM ANALYZE` / `VACUUM (ANALYZE)` / `ANALYZE [VERBOSE]`,**选项列表
一律接受并忽略**(`FULL`,`VERBOSE`,`FREEZE` 都是空转),不带表(整库)与多表列表都收.

三条必须守住的:

- **空转不等于安静**:点名的表逐张在符号表里认一遍,认不出来报
  `UnknownSymbol`--否则 `VACUUM typos` 会成功地骗人(`Stmt::Vacuum::tables`);
- **报文要说清是空转**(`ir.stmt.019`):将来 3.4 落地时,同一个入口用来收真实统计,
  那时候改的是这条报文,而不是"偷偷开始做事";
- **不查权限**,也**不在事务里拒绝**:它是空转,没有要保护的东西--后一条与
  PostgreSQL 有意不同(`VACUUM` 在 PostgreSQL 里不能在事务块里跑).

### 库与角色(`CREATE/DROP/ALTER DATABASE`,`CREATE/ALTER/DROP ROLE`,`GRANT`):一个口子,不是文件系统

语句层**不认识文件系统**:`Stmt::CreateDatabase` / `DropDatabase` /
`AlterDatabase` / `CreateRole` / `AlterRole` / `DropRole` / `GrantRole` / `SetRole`
只把名字(与 `search_path` 的取值,或者角色名与口令)交给
[`Server`](stmt.rs),返回**给用户看的一行状态**--owner 是谁,`users.json` 长什么样
都不归语句层管.建目录,写 `catalog.json` / `users.json`,以及**权限判定**,都在实现方
(`cmd` 的 `Store`,背后是 `usr::service` / `usr::roles`).六条规矩:

- **当前角色要显式传进去**:每个方法都收一个 `role: &str`,它是
  `SymbolTable::role()`--`SET ROLE` 之后它与"认证过的会话用户"不是一个人.
  **只有一个来源**(符号表),别让实现方自己缓存一份;
- **`resolve_role` 与 `check_*` 是"问",别的都是"做"**:问的那些在内存模式下一律
  放行(没有用户),做的那些一律 `Denied`("这个会话没有服务目录");
- **内存模式明确拒绝**:`NoServer` 报 `DbError::Denied`.不假装成功,也不"接受但记下来"
  --库不在任何人的磁盘上,那样会让同一份脚本在 `ysql` 里行为完全不同;
- **事务里一律拒绝**(`reject_in_transaction`):它们改的是**服务目录**,而撤销记录只管
  符号表--`ROLLBACK` 盖不回一个已经改过的目录.判的是**显式**事务(每条语句都被引擎
  包了一个隐式事务,用 `in_transaction()` 会把事务外的语句也一起挡掉);
- **现有签名不动**:`execute_stmt` / `Script::run_one` / `run_script` 都保持原样
  (内部转调 `*_with`,默认实现就是 `NoServer`),于是几十处调用方与测试一行不用改;
  加新的"外部世界"语句时照这个办法办;**口令不许回显**:`Script::run_one_with` 把
  `CREATE/ALTER ROLE ... PASSWORD` 的回显文本换成 `PASSWORD '***'`
  (语句原文会被引擎逐条打出来,而那里面就是口令);
- **选项一律显式拒绝**(`ENCODING` / `OWNER` / `RENAME` / `WITH FORCE`...):本版只建
  **空库**,建出来的库目录里只有 `catalog.json`;`ALTER DATABASE` 只认
  `SET search_path TO ...` 与 `RESET search_path`.
- **角色那几条的选项也是显式拒绝的**:`PASSWORD` / `SUPERUSER` / `CREATEDB` /
  `CREATEROLE` / `INHERIT` / `IN ROLE` 之外(`VALID UNTIL`,`CONNECTION LIMIT`,
  `SYSID`,`ADMIN`,`IN GROUP`...)一律 `Unsupported`;`ALTER ROLE r RENAME TO s` 在
  语法树里是 **`RenameStmt`**(不是带 `action` 的 `AlterRoleStmt`),别的 `RENAME`
  照旧拒绝;`GRANT <角色> TO <成员>` 的 `granted_roles` 里那半个名字**实测是
  `AccessPriv` 而不是 `RoleSpec`**,两种都得认(见 `granted_role_name`).

### 语句里查权限:一处入口,只查该查的

权限检查全在 `run_stmt` 里(执行的唯一入口),**动数据之前**:

| 语句 | 查什么 |
|---|---|
| `SELECT` | 计划里扫到的每张表,按 [`Plan::used_columns`](plan.rs) 给出的**列**要 `SELECT` |
| `INSERT` | `Stmt::Insert::columns`(写了哪几列)要 `INSERT` |
| `UPDATE` | `SET` 写到的列要 `UPDATE`;`SET` 与 `WHERE` **读到**的列要 `SELECT` |
| `CREATE TABLE` / `CREATE INDEX` / `DROP INDEX` / `CREATE SCHEMA` / `DROP SCHEMA` | `check_ddl`(owner 含继承,或超级用户) |
| `CREATE DATABASE` | `check_create_database`(`CREATEDB` 或超级用户) |
| `CREATE TABLE` **建完** | `forget_grants`:同名新表不许继承旧授权 |
| 库与角色 / `GRANT` / `DROP OWNED BY` / `ALTER DATABASE ... OWNER TO` | 判定在实现方(它们本来就只走那个口子) |

`check_table(role, table, want, columns)` 的 `columns` 是**必须覆盖的列名**;
**空切片有专门的意思**:这张表上至少有一列的该权限(整表授权或者任意一列的授权)--
`SELECT COUNT(*) FROM t` 这种一列都没读具体值的查询走它.**空切片不是"不用检查"**.

### 列使用分析(`Plan::used_columns`,6.4)

列级授权要问"这条查询**读到了**哪些列",而计划里的列下标是相对每个算子的输入
schema 的,所以只有从 root 往回推才算得出来([`Plan::used_columns`](plan.rs)):

- `Plan::analyze_shallow` 给出每个节点的输出 schema 与程序输入;`for_each_program`
  给出每个节点的程序;`Prog::columns()` 给出程序引用到的输入列(含子程序);
- **I1(输入下标一定更小)**让"从后往前扫一趟"就够:轮到某个节点时,消费它的那些节点
  的需求都已经推进来了;
- 逐算子的传法:`Scan` 收下自己的集合(外加 `filter` 与索引路径的列);`Filter`/`Sort`/
  `Limit` 1:1 传;`Project` 只展开**被用到的那几个** `progs`;`Aggregate` 按
  "键在前,聚合在后"取对应的程序;`Join` 按 `左 ++ 右` 劈开;`Distinct` 与
  **去重的 `UNION`** 按"输入那一行的全部列"算(`UNION ALL` 才 1:1);
- **相关子查询**处保守:程序里有 `spec.correlated` 的子计划时,它所在节点的输入
  **整层**算被读(`Step::Outer` 读外层行,`Prog::columns()` 看不到);
- 子计划是另一棵计划,递归进去各算各的;
- 方向永远是**宁可多要**--少要一次就是一次安静的越权读.

用 `analyze_shallow` 而不是 `analyze`:子计划的外层引用要拿**外层链**才校得动,那是
`check_subqueries` 的事;在这里用 `analyze` 会把一个合法的相关子计划当成"没有外层行".

内存模式([`NoServer`](stmt.rs))放行 `check_*` 与 `forget_grants`--**这不是"忘了查"**,
是内存里没有用户,键盘前的人就是主人;而改服务目录的语句在内存模式下一律拒绝.

### 事务:撤销记录在符号表里,每条语句自成一个隐式事务(4.1)

`BEGIN` / `COMMIT` / `ROLLBACK` 是真的,撤销记录(`symbol.rs` 里私有的 `Tx`)跟着
**符号表**走--因为要撤销的东西全在那儿:表数据,有哪些符号,索引,schema 名,
`search_path`.三条规矩:

- **目录快照在事务开始时取一份**(只有名字,不含数据,所以廉价);**整表快照按需**
  --事务里第一次要改或被删某张表之前,`Table::snapshot()` 克隆一份.于是大表事务的
  代价是"一次整表克隆",不是"整个库";
- **任何会改数据 / 删符号的路径都必须先 `snapshot_before_write`**:今天只有两处
  (`relation_mut` 与 `drop_schema`).漏一处,回滚就会**静默丢表**(`undo` 里那句
  `panic!` 就是给这件事准备的);
- **回滚之后必须把所有索引槽作废**:索引是"键 → 行号",行号可能已经变了,
  留着就是拿旧行号回答新键--一个安静的错答案.

**每条语句都是一个隐式事务**(`execute_stmt_with` 包的那一层):没有显式事务时
成功即提交,失败即回滚,于是语句级原子性是真的(`INSERT` 第三行过不了 `CHECK`,
前两行也不会留下).`BEGIN` 会把隐式事务**升级**成显式(不升级就会被它自己那个
隐式事务提交掉).**事务里不做逐语句回滚**:那要每条语句都克隆一张表,
`filmdb.sql` 的 7.6 万条 INSERT 会变成 O(n²)--要撤销就 `ROLLBACK`.

`ROLLBACK` 等语句在降级层不再被拒;`SAVEPOINT` / 嵌套事务 / 两阶段提交仍然
`unsupported`,`BEGIN` 套 `BEGIN` 与无事务的 `COMMIT`/`ROLLBACK` 照 PostgreSQL 的
口径**只发一句警告**(`Outcome::Transaction` 的 `message`,`applied: false`),
不算错误,不计入 `failed`.**`applied` 不是装饰**:引擎靠它区分"真回滚了"(该丢掉
脏表标记)与"只警告了一声,什么都没撤销"(脏表标记要留着,那是上次提交失败的重试集).

### 降级(`irgen.rs`)

- **不认识的写法一律显式拒绝**(`DbError::Unsupported`),绝不猜.宁可拒绝,也不要
  给一个安静的错误答案.文件头列着当前仍拒绝哪些写法.
- `UPDATE` 的值与 `WHERE` 都是**不建计划**直接 `eval` 的程序(顺序扫一遍,逐行改):
  所以那里**显式拒绝子查询**(`reject_subqueries`)--没有求值环境.所有 `SET`
  表达式在**旧行**上求值,整行算完再 `Table::update_row` 一次落库(约束看到的是
  最终那一行).`UPDATE` 之前**先**把目标表上的索引整体作废.
- `lower_expr_in` 的 `agg` 参数是**分组模式**:`Some` 时聚合调用变成对聚合输出的列
  引用,裸列引用去 `GROUP BY` 列表里找.
- **聚合抽取是一趟做的**:用 AST 节点地址认同一个聚合.不要改回"先收集再改写"的
  两趟写法--那要求两次遍历顺序完全一致,错了会**静默串位**.
- 分组查询里,**`SELECT` 列表与 `HAVING` 都必须在建 `Aggregate` 节点之前降级完**,
  因为 `HAVING` 里可能第一次出现某个聚合.
- **`contains_aggregate` 必须走进 `CASE WHEN` 的条件/值,`ORDER BY` 的键**:
  它决定"这是不是一个分组查询",判错会让合法的聚合(`CASE WHEN COUNT(*) > 0 THEN ...`,
  `ORDER BY COUNT(*)`)被当成"出现在了不能出现的位置".
- **`contains_aggregate` 必须走进 `CASE WHEN` 的条件/值,`ORDER BY` 的键**:
  它决定"这是不是一个分组查询",判错会让合法的聚合(`CASE WHEN COUNT(*) > 0 THEN ...`,
  `ORDER BY COUNT(*)`)被当成"出现在了不能出现的位置".

### 表达式内部也有惰性:分支是自足子程序

拉取式管**行**,`CASE`/`AND`/`OR`/`COALESCE` 的惰性管**表达式内部**,两件事不冲突:

- `Step::Case` 的 `then_prog` / `else_prog`,`Step::And` / `Step::Or` 的
  `right_prog`,`Step::Coalesce` 的每个 `args` 都是**自足子程序**(自己的寄存器空间),
  只有被选中时才求值;
- 于是 `CASE WHEN 1 = 0 THEN 1/0 ELSE 42 END` 是 42,`WHERE false AND (1/0 = 1)`
  是 0 行,`COALESCE(1, 1/0)` 是 1--死分支/死参数里的运行期错误不再冒出来;
- **`AND`/`OR` 的顺序就是书写顺序**(左侧先算),`COALESCE` 按参数顺序;
  降级不重排,规划也不重排;
- `COALESCE` 的"这一侧是 NULL 吗"与 `Step::IsNull` 同一条判据:
  `Scalar::Value(Value::Null)` 与 `Scalar::Truth(Tri::Unknown)` 都算 NULL;
- 代价是求值多了一层**有界递归**(深度 = 表达式嵌套深度).降级阶段本来就在这个
  嵌套上递归,所以没有引入新的失败类别;
- 遍历程序时别忘了子程序:校验,求值,渲染,`columns()`/`has_subquery()`/
  `has_outer()`/`rebase_columns()`/`plan::program_has_outer`/`plan::check_program`
  都要走进去(`Prog::and` 也只是一个 `Step::And`).

`Step::Binary` 的 `And`/`Or` **保留**(手写程序用),语义仍是"两侧都算完".

### `COALESCE(a, b, ...)`:参数类型按 `CASE` 那条规则统一

语法树里它是 `CoalesceExpr`(**不是 `FuncCall`**),而且裸语法树里**不带类型**
(`coalescetype` 是 0),所以类型统一只能自己做:

- 逐个 `unify_branches` 折叠(同族 / NULL / INT↔NUMERIC),从 `ScalarType::Null` 起;
- 统一不了报 `ir.expr.046`(**不借 `CASE` 的文案**--那是另一条语句的说法);
- `Step::Coalesce::widen` 与 `Step::Case::widen` 同一个约定:统一成 `NUMERIC` 且取到
  `Int` 时要**真的转**,`Prog::verify` 负责对账(对不上报 `IrInvalid`);
- 降级时每个参数走 `branch_prog`(`CASE` 的分支用的同一个函数),`widen` 用扩展后的
  `irgen::widen_flag`(它现在收一串程序,`CASE` 传两个分支,`COALESCE` 传全部参数);
- `irgen::contains_aggregate` 要走进 `CoalesceExpr`,否则 `COALESCE(COUNT(*), 0)` 会把
  "是不是分组查询"判错;
- `GROUP BY COALESCE(...)` 能当分组键:`AggLowering::match_group` 按**程序相等**认同,
  所以投影里写同一个表达式就落到同一个键上(不是"另算一遍").

### 子查询:表达式层的一个 `Step` + 执行期的一层环境

内层查询是**自足**的一份 `Plan`(`SubPlan { plan, mode, outers, correlated, ... }`),
装在 `Step::Subquery` 里;相关引用是 `Step::Outer { depth, index }`.所以:

- **降级**:子查询就是"再开一个查询"--`lower_select(inner, symbols, outers)`,
  `outers` 是外层作用域链(`outers[0]` 是直接外层).解列名沿着它往外走,解到外层
  就记成 `Step::Outer`(`Scope::resolve` 的 `Resolved::Outer`).
  **派生表**不新增算子:`PlanBuilder::splice` 把内层计划的节点整体接进外层数组
  (只改 `inputs` 的偏移,程序一个字都不动).
- **执行**:`ExecCtx`(`exec.rs`)持有**外层行栈**与两个缓存.求值 `Step::Subquery`
  时把"本程序面对的那一行"压进栈,造一份内层源,收成一个标量,弹回来--
  "为外层每一行重开一份内层源"就是多调一次 `build_pipeline`.
- **缓存的是内层的行,不是最终标量**:`x IN (SELECT ...)` 的左半边是外层表达式,
  逐行不同;把最终真值缓存下来会安静地给错答案(实测踩过).相关的每行重开,
  未相关的(`correlated == false`)只算一次.
- **两条闸门**:含子查询 / 外层引用的谓词**不下推**,也不当索引键--它们面对的行
  必须是降级时那个 schema(推给单表 `Scan` 就换了一行),而
  **`outers[0] == 该节点的 `program_input`** 是 `Plan::analyze` 强制的不变量.
  分组查询里的相关子查询直接拒绝(`outers` 留空,内层引用外层会解不到名字).

### 两条不变量,不许破
