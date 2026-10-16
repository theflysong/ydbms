# AGENTS.md -- `src/cmd`

## 职能

**执行引擎**:接受**任务**(SQL 与元命令混在一起的文本),驱动 `ir/` 里的 SQL 部分,
管住两路输出.任务的来源只有三种,在引擎里**没有区别**:

- 交互式终端敲进来的一段(`repl.rs`);
- `\i <文件>` 读进来的内容(`Session::run_file`)--`ysql -f <脚本>` 也走它;
- 元命令本身(`metacmd.rs`).

- `engine.rs` -- [`Session`](engine.rs):状态(符号表,两路输出,`\i` 递归深度,
  `\timing` / `ON_ERROR_STOP` / `VERBOSE` 三个开关,服务模式下的提交点),输入切分,
  元命令的**语义**,SQL 的执行与报告.
- `metacmd.rs` -- 元命令的**语法**:把一行拆成命令名与参数,认不出来的也照样返回
  `MetaCmd::Unknown`.命令名只有一份(`NAMES`),补全与错误提示都从它取.
- `repl.rs` -- 交互式终端:rustyline 接线,[`is_complete`](repl.rs)(多行判定),非 tty 回退.
- `complete.rs` -- Tab 补全的候选,**纯逻辑**,不认识终端.
- `cli.rs` -- `ysql` 的命令行:参数解析(纯函数),认证,决定连哪个库,退出码.

`main.rs` 只把控制权交给 `cmd::repl::run`(内存模式),`src/bin/ysql.rs` 交给
`cmd::cli::run`(服务模式);真正的状态都在这儿.

## 特殊规则

### 内存模式与服务模式:同一个 `Session`

- `Session::new` / `with_sinks` 是**内存**会话(数据退出即丢);`Session::open` /
  `open_with_sinks` 绑着磁盘上的一个库(`usr::service::Database`,已经上过单写者锁),
  并把库级默认值(`catalog.json` 的 `settings.search_path`)套成会话的初始
  `search_path`.两种模式在引擎里**只有 `store` 一个字段的差别**,别为服务模式另开一套循环.
- **一条语句 = 一个隐式事务**(`ir::stmt::execute_stmt_with` 那条路):`run_sql` 每跑完
  一条就调一次 `Session::commit`,事务里它自己空转,所以事务外是"写完就落盘",而
  `BEGIN; ... COMMIT;` 里一次都不落盘.**别把提交点挪回"一次 `run_with`"**:那会让
  `BEGIN` 变成一个没有意义的记号.
- **代价口径**:一条语句一个事务意味着没包事务的脚本里**每条写语句都整表重写一次**
  (`pers` 落盘是整表重写),几千条 INSERT 就看得出来.所以 `-f` / `\i` 的长装载要
  **包在 `BEGIN; ... COMMIT;` 里**(`filmdb.sql` 本来就是这样),这句话在 `ysql -h`,
  `engine.rs` 文件头与 `repl.rs` 的横幅里都要有.
- `Session::commit` 只写 `dirty` 里的表,并把"磁盘上有,符号表里已经没了"的表文件删掉
  (`DROP SCHEMA ... CASCADE` 靠这条落盘,**不需要语句报告删了谁**),最后交给
  `Database::commit`(影子分页 + `commit.json`,**一次提交是原子的**).**两张都空就
  一个字节的 IO 都不做**--少了这一句,一次纯 `SELECT` 也会去写 `commit.json`,
  库目录只读时会变成"查询成功却报提交失败".
- 脏标记只认 `Outcome::Created / Inserted / Updated` 三种(唯一的写路径),
  加新的写语句时**必须同时**把这个 `match` 补上,否则那条路悄悄不落盘.
- 提交失败**保留 `dirty`**(下次再试)并走 `Session::error`;磁盘上不会留下"改了一半"的
  库--失败点要么在暂存阶段,要么在推广阶段(`commit.json` 还在,下次打开时补完).
- 见到 `Outcome::Transaction { op: Rollback, applied: true, .. }` 才把 `dirty` 清掉:
  那些改动真的被撤销了,再写回去就是白写一整张表.**`applied: false` 的那种
  (没有事务时的 `ROLLBACK`,只发一句警告)不许清**--那会把上一次提交失败留下的
  重试集一并抹掉,那批改动就再也不会落盘了.
- `CREATE DATABASE` / `DROP DATABASE` / `ALTER DATABASE ... SET`,以及角色那几条
  (`CREATE/ALTER/DROP ROLE`,`GRANT <角色>`,`SET ROLE`)的文件系统那一半在 `usr`,
  引擎只把 `store` 作为 `ir::stmt::Server` 递进去;内存模式由 `NoServer` 明确拒绝,
  **事务里也一律拒绝**(撤销记录管不到服务目录;`SET ROLE` 是例外--它是会话状态,
  记在符号表里,能回滚).

### 权限判定(`Store` 实现 `Server` 的那几问)

"谁在连"与"**当前角色**"是两件事:前者是认证过的 `Session::user`,后者是
`SymbolTable::role()`(`SET ROLE` 之后不同).`Server` 的每个方法都收 `role` 参数,
所以"当前角色"只有一个来源,`Store` 不缓存它.

- 查权限前先读**当下那一份** `users.json`(`ServiceDir::users`):另一个进程刚删掉某个
  角色时,会话手里那份记录就是过期的--`role_record` 找不到就报错,不猜.
- **超级用户看当前角色自己**(`is_admin`),不沿成员关系继承;`owner` 身份与表权限
  才用 `ServiceDir::acts_as`(有效角色集).这条与 `usr::roles` 里的口径一一对应.
- 几种"能不能":
  - 动结构(`CREATE TABLE` 等)→ `check_ddl`:owner(含继承)或超级用户;
  - 建库 → `check_create_database`:`CREATEDB` 或超级用户;
  - 建 / 改 / 删角色,授成员 → 超级用户或 `CREATEROLE`(`require_role_manager`);
  - **自己改自己的口令**是个例外:谁都能换自己的(`ALTER USER 自己 PASSWORD ...`),
    但换成别人,改属性,改名都要 `CREATEROLE`;
  - `SET ROLE` → 超级用户,或者是目标的成员(`usr::can_set_role`,**不看** `INHERIT`).
- 改名与删角色**不许碰当前会话用户与当前角色**(会话里存的就是名字,改掉就成了幽灵):
  要改就先 `RESET ROLE` 或换个会话.
- 报错一律说清"缺什么,该谁来给":权限不足时报的是"`谁` 在表 `X` 上没有 `Y`,
  要 owner 或管理员执行 `GRANT ...`"这一类句子.

### 授权(表 / 列 / 库,6.4)

- **谁能授**:表 / 列权限要**本库** owner(含继承身份)或超级用户;库上的 `CONNECT`
  要**那个库**的 owner 或超级用户(`check_owner`);`DROP OWNED BY` 要超级用户或
  `CREATEROLE`;换库 owner 要超级用户或那个库的当前 owner.
- **受权人必须存在**(`ServiceDir::require_user`):给一个不存在的角色授权,只是在
  给以后留一个说不清的幽灵.
- 授权表就在**手里那个 `Database`** 里:本会话连着的库直接 `set_grants`;别的库走
  `ServiceDir::with_grants`(借它的单写者锁,拿不到就报"正被另一个进程使用").
- **表的寿命与授权的寿命对齐**,两处都由引擎驱动:提交点里表文件要删时
  `retire_grants`(**先清授权,再提交表文件**--万一提交失败,错的是"权限少了"而不是
  "旧授权落在同名新表上"),`CREATE TABLE` 走 `forget_grants`.
- **`\dt` / `\d` 是目录视图**:表名,列名与行数对所有连上来的人可见(与 PostgreSQL 的
  目录可见性一致),本版**不**按表权限过滤--要藏数据就 `REVOKE` 表权限(那管的是
  "读不读得到行",不是"看不看得见表").

### 文案:一律走 `tr!`

`engine.rs` / `cli.rs` / `repl.rs` / `metacmd.rs` 里那些给用户看的字(状态行,汇总,
错误,帮助,横幅)**不在这个文件里**:源文案在 `locales/zh-CN.yml`,译文在
`locales/en.yml`.改文案改那儿,别在代码里留硬编码中文--口径与键的规则见根
`AGENTS.md` 的 "六,文案与标点",`tests/i18n.rs` 会核对.

### 退出码与错误计数

- **报错一律走 `Session::error`**(它同时给 `failed` 计数);语句自己的失败走
  `Outcome::Err` 那条路,`run_sql` 里也要计入 `failed`.`ysql -f` 的退出码靠它,
  漏一处就会"报了错还退出 0".

### 与 `ir` 的分工:这里是引擎,那里是 SQL

- **引擎里不许有 SQL 知识**:解析,降级,执行都在 `ir/`(`Script` / `Plan` / `Prog`).
  这一层只管"哪一段是 SQL,哪一段是元命令,结果往哪一路写,出错要不要停".
- 依赖方向是 `cmd` → `ir`,**反过来不行**.要让引擎多做一件事,先问它是不是 SQL 的事.
- 想"逐条"干活(计时,出错就停)用 `ir::Script`:**解析一次,`run_one` 一条**.
  别在 `ir` 里塞回调,也别把 `cmd` 的策略塞进 `ir`.

### 两路输出是结构性的

`Session` 有两个独立的 [`Sink`](engine.rs),这是 `\o` 语义的核心:

- **`query_out`** -- 只装**查询结果表**(表头 + 数据行).
- **`messages`** -- 状态行,汇总,错误,以及 `\d`/`\echo`/`\ls` 等元命令的输出.

**`\o` 只换 `query_out`,永远不影响 `messages`.** 所以"`\o` 仅限于查询输出"这句话
是**结构上**成立的,不是靠约定.**不要把状态行,错误,汇总写进 `query_out`** --
那会立刻破坏 `\o` 的含义.

### `VERBOSE`:哪些算"状态行"

`\set VERBOSE on|off` 只管**引擎自己的聊天**,一律走 `Session::info`(off 就不打):

- `\i` 的"读入 ...(N 字节)";
- 每条语句的状态行(`report` 的六个成功分支:`Rows` / `Created` / `Inserted` /
  `Updated` / `NoOp` / `Transaction`);
- 每段 SQL 的汇总("成功 N 条,失败 M 条",含 `ON_ERROR_STOP` 打断时那一条);
- `\o` 的去向提示;
- `\set` 自己的回显(`ON_ERROR_STOP:开` / `VERBOSE:开`).所以 `\set VERBOSE on` 会报
  一声,`\set VERBOSE off` 是静悄悄的--它自己已经把状态行关掉了.

**不归它管的**有几样:错误(一律 `Session::error` / `report` 的 `Err` 分支,**永远打**--
否则脚本会静悄悄地什么都没干),查询结果表(`query_out`),`\timing` 的回显与
`\echo` / `\d` / `\dt` / `\l` / `\du` / `\c` / `\pwd` / `\ls` / `EXPLAIN` 这些用户点着名
要的输出,以及下面那一族**命令标签**.加新的状态行时**走 `info`**,别走 `msg`.

默认 **开**;只有 `ysql -f <脚本>` 由 `cli.rs` 在 `run_file` 之前关掉(**脚本模式默认
安静**,照 psql 的 `-f`).所以"`-f` 与 `\i` 输出逐字一致"这句话不再成立:同一条执行
路径,但 `-f` 默认安静(脚本里 `\set VERBOSE on` 随时能打开).

### 命令标签:psql 的**协议**,不是文案,也不归 `VERBOSE` 管

写语句与事务控制各多打一行标签(走 `Session::tag`,只进 `messages`):

| `Outcome` | 标签 |
|---|---|
| `Inserted { rows, .. }` | `INSERT 0 <rows>` |
| `Updated { rows, .. }` | `UPDATE <rows>` |
| `Transaction { op, applied: true, .. }` | `BEGIN` / `COMMIT` / `ROLLBACK` |

四条规矩:

- **照 psql 原样打,不走 `tr!`**--它是协议,不是给用户看的文案.别"顺手"把它翻译了,
  也别把它加进文案表(`tests/i18n.rs` 也拿它当机器可读的那一行钉着);
- **永远打**,`VERBOSE` 关着照样打:`ysql -f` 的脚本模式默认就是关的,而根目录的
  `summarize.py` 靠这几行(再加 `\timing` 的 `Time: x ms`)把写基准汇总成 JSON.
  所以它走 `Session::tag` 而**不是** `info`;
- **只覆盖写语句与事务控制**:`CREATE TABLE`,`SET`,`VACUUM` 之类仍然只有人类可读的
  状态行.要扩这一族,先把"谁真的需要机器可读"想清楚;
- **只发一句警告的那种没有标签**(没有事务时的 `COMMIT`,已经在事务里的 `BEGIN`)--
  与 psql 一致,那是 `applied == false`.

`\echo` 的**去引号**也在引擎里(`unquote_echo`),不在 `metacmd.rs`--那一层只拆词,
不解释参数.只处理"整行被一对单引号包住",psql 那种逐参数去引号不做(与参考实现
的已知偏差,写在函数注释里).

### 服务目录那三条元命令(`\l` / `\du` / `\c`,6.4)

- **`\l`**:列服务目录里的库与 owner(当前库带一个 `*`),**不过滤可见性**;
  某个库的 `catalog.json` 读不出来**就报错**--不糊一个 `?` 过去.
- **`\du`**:列角色(名字 / 属性 / 所属角色).属性是 `SUPERUSER` / `LOGIN` /
  `CREATEDB` / `CREATEROLE` / `NOINHERIT` 里打开的那几个.
- **`\c [库]`**:换库.**脚本里也能用**(`-f` 与 `\i` 是同一条路).顺序不许换:
  1. 事务开着就**拒绝**(与断开连接时的静默回滚不同,这里先说清楚);
  2. 旧库欠着的提交先补上(补不上就不切--那批改动不能丢);
  3. **先开新库再换句柄**:开不成 / 载入失败就留在原地,旧库的锁不撒手;
  4. 换的是**一张全新的符号表**(新库的表),当前角色回到会话用户(换库 = 换连接).
     旧库的单写者锁在这一步当场释放(`Database` 被换掉,内核回收 flock).
  已经连着的库只发一句提示,不算错;参数缺了给用法.
- **提示符每轮现取**(`Session::prompt`):`<库> => `,`SET ROLE` 之后是
  `<库>[<角色>] => `--免得借了别人的身份还以为自己是他.所以 `\c` 之后下一条
  提示符就变了(`repl::interact` 里每轮问一次).
- **内存模式**三条都报"这个会话没有服务目录";补全的库名也来自会话(`Catalog::databases`).

### 切分与元命令

- 元命令**只在行首**(trim 之后)才算.行中间的 `\` 是普通 SQL 文本.
- 元命令的**语法**只放 `metacmd.rs`(只拆词,不校验),**语义**只放 `engine.rs`
  (校验参数,写输出).不要在 `metacmd.rs` 里做路径检查或报错输出--那里没有
  输出去向可用;也不要在那里解释 `\timing yes` 这种取值(装原样的字符串即可).
- **`\!`(执行 shell)在脚本里禁用**,只在交互终端放行.这是有意的安全边界,
  不要为了方便打开.
- `\i` 有**递归深度上限**,防止脚本自我包含.放宽之前先想清楚会不会死循环.
- **`\i` 与 `\ir` 是同一条路,只差相对的基准**:`\i` 按进程的工作目录,`\ir` 按
  **当前脚本所在目录**.`-f` / `\i` / `\ir` 读进来的文件都会成为"当前脚本"
  (`Session::script_dir` 在 `include_file` 的递归前后换进换出,于是它天然是一层栈;
  存的是**绝对**路径,`\cd` 改不动它);不在任何脚本里时 `\ir` 退化成 `\i`.
  这两句都是 psql 的实测口径,别"顺手"改成只认最外层脚本.

### 错误与 `ON_ERROR_STOP`

- **报错一律走 `Session::error`**,别直接 `msg`:只有走它,`\set ON_ERROR_STOP on`
  才能真的"遇错就停".顺手 `msg` 一句错误,就会在那里漏掉停止.
- `Flow::Stop` 必须**一层层往上传**(`run_with` → `include_file` → `exec_meta`).
  `\i` 里设的开关要影响到外层脚本,这是 psql 的语义.
- **脚本那一路命中 `Stop` 之后不许再读输入**:`run_plain` 靠 `Session::stopped()`.
  交互终端里则只停"当前这一段提交",**不退出会话**--这是与 psql 的**有意差别**
  (数据只在内存里,为一次笔误把库丢掉太狠),要如实写进文档,别哪天"修"成退出.
- **事务是 `ir` 的事,引擎只管落盘**:`Flow::Stop` 打断的是"后面的语句",不是事务--
  事务还开着时 `Session::commit` 空转,数据留在内存里等用户 `COMMIT` / `ROLLBACK`
  (退出会话就等于丢掉它,与 PostgreSQL 断开连接一致).事务里失败的语句**不会**
  整句回滚(本版没有 `SAVEPOINT`),文档与输出都得如实说.
- 测试里"某条语句有没有跑"最可靠的证据是**表里的行数**,不是消息文本.

### 交互式终端(`repl.rs`)

- **终端只做引擎管不了的事**:行编辑与"这一句够了吗".别把 SQL 判断搬到这儿.
- 多行的判定([`is_complete`](repl.rs))有且只有这一处,行尾扫描在 `ir` 里
  (`has_top_level_terminator`,与 `statement_text` 那些词法活放一起).
- **非 tty 必须有回退**:管道/重定向没有终端可编辑,走 `run_plain`,并且 EOF 时
  要把攒着的交出去.`printf 'SELECT 1\n' | ydbms` 得能用.
- rustyline 在 `TERM=dumb` 上主动降级(没有补全)--这是它的行为,不要去"修".
- `main.rs` 只留一行调用:**不要把 REPL 状态放回 `main.rs`.**

### Tab 补全(`complete.rs`)

- **补全逻辑必须是纯函数**(`suggest(line, cursor, catalog)`):终端那边只转发候选,
  rustyline 自己决定"唯一候选直接写,多个补公共前缀,再按 Tab 列出来".于是补全能单测.
- 下标一律是**字节**下标,且必须落在**字符边界**上(中文表名不能切坏).
- 补全的候选**只能是引擎真正支持的东西**(关键字表,`metacmd::NAMES`,
  `ON_ERROR_STOP`).补出一个本版不认的写法比不补更糟.
- 候选每次现算(`Session::catalog`):上一条语句刚建出来的表,下一条就该能补出来.

### 元命令与 `EXPLAIN`

`EXPLAIN` 不是元命令(不以 `\` 开头),它只是交互终端的一个显示便利,所以脚本里
用不了.新增这类"终端便利"时,想清楚它该不该在脚本里可用.
