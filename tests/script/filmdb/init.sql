-- ============================================================================
-- init.sql -- 建 study schema,设 search_path,再把 filmdb.sql 装进去
--
-- 形状与考试那套 init.sql 一致(那条链路才是这个 testbench 要验的):
--   DDL 包在显式事务里 -> `\o /dev/null` 丢掉装载期的结果集 -> `\i` 引子脚本
--   -> `\o` 收回输出去向 -> 自检一句 + `\dt`
--
-- `\i filmdb.sql` 是 **cwd 相对**的(不是 `\ir`),所以调用方必须把工作目录切到
-- 本文件所在的目录 -- tests/filmdb_suite.rs 就是这么做的.
-- 可重复执行:每次 DROP SCHEMA ... CASCADE 重建.
-- ============================================================================

\set ON_ERROR_STOP on

BEGIN;

-- 重置专用 schema
DROP SCHEMA IF EXISTS study CASCADE;
CREATE SCHEMA study;

-- 本次会话优先在 study 中查找/创建对象
SET search_path TO study, public;

COMMIT;

-- 装载:结果集全部丢弃,`filmdb.sql` 自己包了事务
\o /dev/null
\i filmdb.sql
\o

\echo 'study schema 初始化完成'
\dt study.*
