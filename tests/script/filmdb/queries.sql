-- ============================================================================
-- queries.sql -- 6 条查询,每条把结果写进一个文件(`\o ./queryN.txt`)
--
-- 形状与考试那套 correctness/queries.sql 一致(那也是这个 testbench 要验的一环:
-- `\o` 落文件 + 结果表的渲染),但期望值不再是 PostgreSQL 打出来的参照文件,
-- 而是 **tests/filmdb_suite.rs 里手算出来**的表头与数据行.
--
-- 覆盖:`JOIN` + 多键 `ORDER BY`;`GROUP BY`/`HAVING`/`COUNT(*)`;`LEFT JOIN` 的空
-- 可空侧 + `IS NULL`;`DISTINCT` + `IN (子查询)`;`CASE`/`ILIKE`/`LENGTH`;
-- `EXTRACT(YEAR FROM <date>)` + `BETWEEN` + `UNION ALL` + `ORDER BY` + `LIMIT`.
--
-- 这 6 条**只能用当时已经实现的写法**(不许出现 `||`,`COALESCE`,`LOWER`,
-- `CAST(... AS TEXT)`):fixture 是独立于那些特性的一层,它先落地.
-- ============================================================================

\set ON_ERROR_STOP on

BEGIN;

-- Query 0: 1960 年以后的作品,连出国家名(多键排序)
\o ./query0.txt

SELECT m.movieid, m.title, c.country_name
FROM movies AS m
    JOIN countries AS c
        ON m.country = c.country_code
WHERE m.year_released >= 1960
ORDER BY m.year_released, m.movieid;

-- Query 1: 拍片超过两部的洲(分组 + HAVING)
\o ./query1.txt

SELECT c.continent, COUNT(*) AS films
FROM movies AS m
    JOIN countries AS c
        ON m.country = c.country_code
GROUP BY c.continent
HAVING COUNT(*) > 2
ORDER BY c.continent;

-- Query 2: 片长为空或超过 200 分钟的作品,左连接带出署名(空的可空侧)
\o ./query2.txt

SELECT m.movieid, m.title, cr.credited_as
FROM movies AS m
    LEFT JOIN credits AS cr
        ON m.movieid = cr.movieid
WHERE m.runtime IS NULL
    OR m.runtime > 200
ORDER BY m.movieid, cr.credited_as;

-- Query 3: 有过作品的国家(去重 + IN 子查询)
\o ./query3.txt

SELECT DISTINCT c.country_code, c.country_name
FROM countries AS c
WHERE c.country_code IN (SELECT m.country FROM movies AS m)
ORDER BY c.country_code;

-- Query 4: 片名里带 a 的作品,按片长分类(ILIKE + LENGTH + CASE)
\o ./query4.txt

SELECT m.movieid, m.title, LENGTH(m.title) AS title_length,
    CASE
        WHEN m.runtime IS NULL THEN 'unknown'
        WHEN m.runtime >= 200 THEN 'long'
        WHEN m.runtime >= 100 THEN 'standard'
        ELSE 'short'
    END AS runtime_class
FROM movies AS m
WHERE m.title ILIKE '%a%'
ORDER BY m.movieid;

-- Query 5: 2024 年注册的成员 + 2025 年开的主题(EXTRACT + BETWEEN + UNION ALL)
\o ./query5.txt

SELECT fm.name AS label, EXTRACT(YEAR FROM fm.registered) AS joined_year
FROM forum_members AS fm
WHERE fm.registered BETWEEN '2024-01-01' AND '2024-12-31'
UNION ALL
SELECT ft.message, EXTRACT(YEAR FROM ft.post_date)
FROM forum_topics AS ft
WHERE ft.post_date >= '2025-01-01'
ORDER BY joined_year DESC, label
LIMIT 3;

\o

COMMIT;
