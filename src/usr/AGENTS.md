# AGENTS.md -- `src/usr`

## 职能

**用户与库**:谁能进,进哪个库,库在磁盘上的什么位置.

- `mod.rs` -- [`User`](mod.rs)(一个**角色**:口令,属性,成员关系)/
  [`UserStore`](mod.rs)(`users.json`),[`UserStore::authenticate`](mod.rs),
  `AuthError`,[`RoleAttrs`](mod.rs) / [`PasswordAction`](mod.rs) /
  [`RoleAttrChange`](mod.rs)(`CREATE / ALTER ROLE` 的取值),
  [`Password`](mod.rs)(明文口令的 newtype,`Debug` 打不出来),`now_unix`.
- `roles.rs` -- **成员关系**的纯函数:[`members_of`](roles.rs)(可达集合,不看
  `INHERIT`),[`effective_roles`](roles.rs)(看 `INHERIT`,**逐边**判断),
  [`can_set_role`](roles.rs),[`would_cycle`](roles.rs)."谁能做什么"这句口径
  只有这一处落点.
- `hash.rs` -- 口令的加盐哈希:`sha256(salt ‖ password)`,`/dev/urandom` 取盐,
  十六进制的进出.
- `service.rs` -- 服务目录 [`ServiceDir`](service.rs)(`users.json` + 各库目录),
  库元数据 [`Catalog`](service.rs)(`<库>/catalog.json`),打开着的库
  [`Database`](service.rs)(目录 + 元数据 + **单写者锁**),以及**角色那几条**语句的
  读-改-写(`create_role` / `alter_role` / `drop_roles` / `grant_roles`).

`ysql` 的命令行入口在 [`cmd::cli`](../cmd/cli.rs);引擎侧的提交点与权限判定在
[`cmd::engine::Session`](../cmd/engine.rs).

## 磁盘契约

```text
<server-dist>/                    默认 ~/.ydbms(--server-dist 可换)
├── users.json                    {version, users:[{name, salt, password_hash, is_admin, created_unix,
│                                                  can_login, can_create_db, can_create_role,
│                                                  inherit, member_of}]}
├── .users.lock                   改 users.json 时的 flock(读-改-写期间持有)
└── <库>/
    ├── catalog.json              {version, name, owner, created_unix, settings}
    ├── .lock                     单写者锁(flock)
    ├── manifest.json             提交记录:每张表**已提交到哪儿,是哪种格式**(见下)
    ├── <表>.heap                 基表之一:页式堆文件(默认格式,见 `pers/AGENTS.md`)
    ├── <表>.log                  堆表:行位置日志,挪过窝的行的新地址(见下)
    ├── <表>.csv                  基表之一:标准 CSV(表名就是文件名主干,可能带 schema 前缀)
    ├── <表>.schema.json          模式
    └── <表>.heap.new / <表>.csv.new / <表>.schema.json.new   暂存(整表重写时的现场)
```

- **`users.json` 与 `catalog.json` 的字段名,`version`,`is_admin`,`settings`
  都属于磁盘契约**:改名或改结构就是改文件格式.**新增字段一律 `#[serde(default)]`**,
  保证老文件读得回来.
- 后加的五个字段(`can_login` / `can_create_db` / `can_create_role` / `inherit` /
  `member_of`)**不许让老文件的行为变化**:`can_login` 与 `inherit` 的默认值是
  `true`(老文件里的人照旧能登录,照旧继承),`member_of` 默认空.**`version` 不升**
  --升它就没有回头路,而默认值已经够读回来了.
- 版本不认识就**报错**,别按当前格式硬读.`manifest.json` 也一样--**坏了就不许开库**,
  静默忽略它等于把"上次提交崩在中途"这件事抹掉.
- 库目录里**缺 `catalog.json` 不自动补**:owner 无从知道,补一个等于伪造 owner.
- 表的落盘/读回**只在 [`pers`](../pers/AGENTS.md)**:这里只把"库目录 + 表名"接起来,
  文件名一律由 `TablePath::new(dir, 表名)` 推(含暂存名的 `.new` 后缀).
- **表名就是文件名主干**(`<表>.csv`),所以 [`is_file_stem_safe`](../db/table.rs) 判为
  不安全的名字(空,`.`,`..`,含 `/`,`\`,NUL)在这里**再挡一次**.`ir` 建表时已经拒过
  一遍,这一遍是**有意的**,不是可以合并的重复:`ir` 那一道是为了早报错(建表时就告诉
  用户),这一道是**安全边界**--真正 `Path::join` 的是这里,放过去就等于允许
  `CREATE TABLE "../别人的库/evil"` 把文件写到别人的库目录(乃至服务目录)里去.
  库名(目录名)同理,见 `checked_name`.

## 特殊规则

### 授权表(`<库>/grants.json`,6.4)

```text
{ "version": 1,
  "connect": ["alice"],
  "tables": [ { "grantee": "alice", "table": "movies", "privilege": "SELECT",
                "columns": ["title"] } ] }
```

- **一条记录 = (受权人, 表, 权限)**:`columns` 空 = 整表,非空 = 列级.扁平写法是为了
  同一条语句里不同权限能带不同的列清单(`GRANT SELECT (a), UPDATE (b) ON t`);
- **`connect` 是库级的那一个权限**(非 owner / 非超级用户的人要连这个库就得在这儿),
  它不给任何表权限,表权限也不给连接的能力;
- 缺文件 = 没有授权;**坏了 / 版本不认识 = 报错**(授权表读不出来时不许当成"没有授权");
- 写盘**先规范化**(排序,去重,列级记录归并),于是同一份授权只有一种写法;
- `REVOKE <权限> ON t`(不给列)**只删整表的那条**,列级那条留着;按列收才动列级那条;
- 授权表跟着**库目录**走,所以受那个库的单写者锁保护:本会话连着的库直接改手里那份
  (`Database::set_grants`),**别的库**要借一次它的锁([`ServiceDir::with_grants`](service.rs))
  --拿不到就不改(人家正连着,它手里那份才是最新的);
- **表的寿命与授权的寿命对齐**:表文件被删时(`Session::commit` 里,提交表文件**之前**)
  授权记录一并清;新建同名表时(`CREATE TABLE`)也先清一次残留.两处都只往"少权限"错.

### 存储模式:两种格式,跟着表走

基表有**两种格式**,由 [`Storage`](../pers/mod.rs) 选(会话级,两个入口:命令行
`--storage heap|csv`,以及 `ysql-csv` 那个预设成 CSV 的可执行文件).四条规则:

1. **格式跟着表走**:一张表用哪种格式记在提交记录里(`TableFootprint` 的 `heap` /
   `csv` 两个标记),打开库时按记录分派--`load_tables` 既认 `*.heap` 也认 `*.csv`;
2. **新建表跟会话的模式**:`Server::new_table` 按模式造堆表(`HeapTab`)或内存表
   (`PhysTab`,提交时整表写成 CSV);
3. **没有格式记录的 CSV 基表 = 老库**(TODO 7.5 之前):堆模式打开时**迁进**堆文件
   (迁完删掉 CSV);CSV 模式打开时**就地认领**,一个字节都不搬,位置记录留到下一次
   提交时补上;
4. **混用合法**:两种格式可以在同一个库里并存,两支二进制都能打开同一个库.模式只
   影响"以后新建的表"与"怎么对待老库",**不改已有表**.

两个标记**都是 `false`** 才是"没有格式记录";CSV 表另记 `csv_rows`(文件里的行数),
打开库时与文件对账--与堆表的 `heap_bytes` / `heap_rows` 同一个口径:对不上就是有人
手工动过目录,宁可开不了库.

`manifest.json` 只**新增**了两个 `#[serde(default)]` 字段,**`version` 不升**(升了
新二进制就拒开所有现有库,代价远大于收益).代价说清楚:**旧版本的二进制**读到新清单
会把 CSV 表当成老库迁进堆文件--别拿两个版本夹着开同一个库.

### 提交是原子的:追加式的基表 + 提交记录(TODO 7.5)

**堆表**(默认格式)是追加式写的:数据在 `<表>.heap` 里,只往文件尾追加;**CSV 表**
是整表重写(暂存 + 推广).两条路的共同点是"提交记录说了算".提交点做三件事,
**顺序不许换**:

1. **堆表**把挪过窝的行的新地址追加进 `<表>.log`(只追加);**CSV 表**(以及别的不
   自己落盘的表)整表写成暂存文件.两者都先把模式文件写一遍(几百字节,表的结构
   跟着表走);
2. 落下提交记录 `manifest.json`:每张表的位置(堆表:堆文件字节数 / 行数 / 位置日志
   条数;CSV 表:格式标记 + 文件行数)连同"要推广谁 / 要删谁" -- **过了这一点,这次
   提交就必须完成**;
3. 推广暂存文件(整理与迁移用),删掉要删的表,再把记录里那两栏清空重写一遍.

CSV 表走的就是老的 `pers::csv` 那条路(暂存 + 推广),位置记成"CSV + 行数".

`Database::recover`(在 [`ServiceDir::open`](service.rs) **拿到锁之后,谁读表之前**跑)
四步,顺序也不能换:

1. 记录里还有 `promoted` / `removed` 的,先做到底(新数据早就在暂存文件里了),再清空;
2. **老库搬进堆文件**(只有堆模式做):写 `<表>.heap.new` -> 记录里记新位置(提交点)
   -> 推广 -> 删掉 CSV.崩在中间时记录里还没有这张表的位置,下次接着搬(幂等).
   CSV 模式**不搬**:老库就地当 CSV 用.迁移之前还要看一眼 `<表>.log`:非空说明那是
   已废弃的**行日志**格式(数据在里面),本版重放不了,**拒绝迁移**而不是悄悄丢掉;
3. 每张表截到记录里记的位置:行位置日志截到记的条数(堆文件在
   [`HeapFile::open`](../pers/heap.rs) 里截到记的字节数);
4. **整理**死字节太多的堆表(按当前行目录密集重写一遍),再清扫没有记录庇护的
   `.new` 残渣.

哪一步都不会留下"改了一半".

- **账本在记录里,不在文件里**:堆文件与位置日志都是追加式的,"到哪儿算数"由
  `manifest.json` 说.所以追加之前要先截到已提交的位置(`truncate_moves`)--不先截,
  上一次崩在半路的尾巴(或者上一次整理之后残留的旧地址)就会被算进去.
- **每次提交都重写记录**:它只有几百字节,而"每张表写到了哪儿"必须是一个**原子**的
  单文件写入才能当提交点.
- **整理只在打开库时做**:那一刻还没人拿着这份文件读写.提交途中换掉正在写的文件会
  让文件句柄指错地方(见 `pers::heap` 的文件头).

- **清扫必须在补完之后**:记录里还记着的时候,那些 `.new` 正是要搬进正式位置的数据.
- 恢复是**幂等**的,因为它就是 `commit` 的第三段--崩几次都能接着补.
- 不做 WAL,也不 `fsync`:这条保证只覆盖**进程崩溃**(含 `kill -9`),**不覆盖掉电**.
  这一句要一直写在文档里,别把它说成比实际更强的保证.

### 库级会话默认值(`catalog.json` 的 `settings`)

`ALTER DATABASE ... SET search_path TO ...` 写的不是会话状态,而是**库的元数据**
([`Settings`](service.rs)),新连接打开库时由 `cmd` 套成初始 `search_path`.
两条规矩:

- **只影响以后的连接**:写它的那个会话自己的 `search_path` 一个字都不变(PostgreSQL 也是);
- **读-改-写,每次都以磁盘上的那份为准**(`set_database_search_path`),免得覆盖掉
  别人刚改的字段;owner 检查在调用方(`cmd` 的 `Store`),因为"谁在连"是会话的事.

### 口令:不存明文,也不假装它是 KDF

- `users.json` 就在用户主目录里,**明文比加盐哈希危险得多**--这条不许破.
- 存的是 `sha256(salt ‖ password)` 的小写十六进制,盐每用户 16 字节,来自
  `/dev/urandom`;读不到随机源就**报错**,不要退化成时间戳那种可预测的"随机".
- 这是**加盐单轮**哈希,**没有迭代拉伸**,抗暴力枚举不如 bcrypt / argon2.
  要升级就换算法并借 `users.json` 的 `version` 迁移--**别只把这段注释删掉**.
- 比较走 `hash::verify`(常量时间),别写成 `==` 直接比字符串.

### 角色与成员关系(6.4)

**用户就是角色**:`CREATE USER` 建的默认 `LOGIN`,`CREATE ROLE` / `CREATE GROUP`
建的默认 `NOLOGIN`;`SUPERUSER` / `CREATEDB` / `CREATEROLE` / `INHERIT` 都是属性.
四条规矩:

- **超级用户不是继承来的**:只有当前角色自己 `is_admin` 才算(与 PostgreSQL 一致).
  普通权限(连库,`owner` 身份)才沿成员关系继承.
- **`INHERIT` 逐边判断**:一条 `child → parent` 的边只有 `child` 自己 `INHERIT` 时
  才走下去([`effective_roles`](roles.rs));`SET ROLE` 借身份**不看**它
  ([`can_set_role`](roles.rs) 用 [`members_of`](roles.rs)).两个函数的差别就在这里,
  别合并.
- **成环直接拒绝**([`would_cycle`](roles.rs)):图是用户自己改出来的,不能假设它无环.
- **最后一个超级用户**不能删也不能降级(`UserStore::is_last_superuser`):
  与 bootstrap 那句"否则谁都进不去"同一个道理.

角色那几条语句的**结构**检查在这里(`role_traces` 之外都很轻):

- **改名要过"干净角色"**([`ServiceDir::role_traces`](service.rs)):`users.json` 之外
  还记着这个名字的地方(库的 `catalog.json` 的 owner,库里的授权)分散在各库目录里,
  而那些库可能正被别的进程锁着--**改不干净就不改**,报错里点名列出来.
- **删角色同理**:有库 / 有授权就不删,并点名卡在哪几个库.
- **判断"谁有资格"不在这里**:那要看会话的当前角色,由 `cmd` 的 `Store` 先判
  (与 `owner` 检查的既有分工一致).

### `users.json` 的写锁

- 角色那几条(建 / 改 / 删 / 授成员)是**读-改-写** `users.json`,而它是**全局**的:
  两个 `ysql` 进程(连着不同的库)同时改就会丢更新.
- 所以写之前先拿 `<server-dist>/.users.lock`(一把 `flock`,`try_lock`),
  **拿不到就报错,不排队**--与库的单写者锁同一个口径.
- 用户表还**不许凭空造**:`users.json` 不在时那几条语句明确拒绝(不是当成空表),
  否则下一次启动会以为早就 bootstrap 过,谁都进不来.首次启动的 bootstrap 在
  `ysql` 那一条路上([`cmd::cli`](../cmd/cli.rs)).

### 库与用户的口径

- **连一个库**:owner(或 `owner` 在它的**有效角色集**里)或超级用户.
  授权表(`<库>/grants.json` 里的 `CONNECT`)是第三个放行条件,见下面那节.
- **一个目录 = 一个库**:库名只能是一个目录名(空,`.`,`..`,含 `/` 或 `\` 一律拒绝,
  否则就跑到服务目录外面去了).
- `search_path` 本身是**会话状态,不落盘**--但现在库里可以记一个**库级默认值**
  (`catalog.json` 的 `settings.search_path`,靠 `ALTER DATABASE ... SET search_path` 写,
  见上).没有这个默认值时,新进程连回一个库之后非限定表名解析不到,要写限定名或自己
  `SET search_path`.

### 单写者锁

- 每个库目录一把 `flock`(`File::try_lock`,标准库能力,**不加依赖**).
- **先上锁,再载入表**:反过来的话载入会在半途被另一个写者改掉文件.
- 锁随句柄走,进程退出(含 `kill -9`)内核就释放--**没有陈旧锁文件要清理**,
  也不要做"锁文件里有 pid 就删掉"那种危险的"修复".
- 本版**不做多进程协调**:第二个写者直接连不上.宁可连不上,也不要写坏数据.
- `DROP DATABASE` 之前要**探一次锁**(别人正连着就不删),并拒绝删自己正在用的库.

### 报错

- 服务目录这一层的错误统一用 [`DbError`](../db/table.rs):
  `UnknownDatabase` / `DuplicateDatabase` / `Denied{message}`(正文写中文缘由),
  文件坏了用 `Format`,IO 用 `Io`.
- **认证失败用 `AuthError`**(`Display` 直接是中文,`ysql` 原样打出来):
  `UnknownUser` / `BadPassword` / `CannotLogin`(NOLOGIN)/ `NoPassword`(没口令)/
  `Corrupt`.
- 拒绝的时候要把**为什么**说清楚(谁的库,owner 是谁,缺哪个文件),
  这是这一层唯一的使用者体验.
