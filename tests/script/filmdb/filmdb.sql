-- ============================================================================
-- filmdb.sql -- 电影数据库的**缩微样本**(crate 内自带,自造)
--
-- 它是考试那套 filmdb.sql(7.6 万条 INSERT,3.8 MB,留在 crate 之外)的自造替身:
-- 表名,列名,类型与约束形状都对得上,数据只有 68 行,好让期望值能手算.
--
-- 刻意覆盖的 DDL 面:CHECK(列级 + 表级),NOT NULL,DEFAULT,CHAR(N),VARCHAR(N),
-- TEXT,INT,DATE,复合主键,PRIMARY KEY / UNIQUE / FOREIGN KEY(接受但不强制),
-- 以及一张**空表**(movie_title_ft_index2).
--
-- 整份装载包在 BEGIN; ... COMMIT; 里:一条语句一个事务的引擎下,这样只在末尾落盘.
-- 表名一律不带 schema 前缀,靠 init.sql 设的 search_path 落到 study.* 上.
-- ============================================================================

BEGIN;

-- ------------------------------------------------------------------ 国家
CREATE TABLE countries (
    country_code CHAR(2)     NOT NULL
                             CONSTRAINT "国家代码两位"
                               CHECK (length(country_code) = 2),
    country_name VARCHAR(50) NOT NULL
                             CONSTRAINT "国家名不超过 50"
                               CHECK (length(country_name) <= 50),
    continent    VARCHAR(20) NOT NULL
                             CONSTRAINT "大洲名不超过 20"
                               CHECK (length(continent) <= 20),
    PRIMARY KEY (country_code),
    UNIQUE (country_name)
);

INSERT INTO countries (country_code, country_name, continent) VALUES
    ('us', 'United States',  'North America'),
    ('fr', 'France',         'Europe'),
    ('jp', 'Japan',          'Asia'),
    ('br', 'Brazil',         'South America'),
    ('gb', 'United Kingdom', 'Europe'),
    ('it', 'Italy',          'Europe'),
    ('de', 'Germany',        'Europe'),
    ('es', 'Spain',          'Europe');

-- ------------------------------------------------------------------ 作品
-- runtime 可空(第 12 行就是 NULL);国家必须存在(外键只记不强制)
CREATE TABLE movies (
    movieid       INT          NOT NULL PRIMARY KEY,
    title         VARCHAR(100) NOT NULL
                               CONSTRAINT "片名不超过 100"
                                 CHECK (length(title) <= 100),
    country       CHAR(2)      NOT NULL,
    year_released INT          NOT NULL
                               CONSTRAINT "年份范围"
                                 CHECK (year_released BETWEEN 1888 AND 2100),
    runtime       INT
                               CONSTRAINT "片长范围"
                                 CHECK (runtime IS NULL OR (runtime > 0 AND runtime < 1000)),
    UNIQUE (title, country, year_released),
    FOREIGN KEY (country) REFERENCES countries (country_code)
);

INSERT INTO movies (movieid, title, country, year_released, runtime) VALUES
    ( 1, 'Alien',                 'us', 1979, 117),
    ( 2, 'Blade Runner',          'us', 1982, 117),
    ( 3, 'Le Samourai',           'fr', 1967, 105),
    ( 4, 'Seven Samurai',         'jp', 1954, 207),
    ( 5, 'City of God',           'br', 2002, 130),
    ( 6, 'Lawrence of Arabia',    'gb', 1962, 222),
    ( 7, 'Bicycle Thieves',       'it', 1948,  89),
    ( 8, 'Amelie',                'fr', 2001, 122),
    ( 9, 'Spirited Away',         'jp', 2001, 125),
    (10, 'Vertigo',               'us', 1958, 128),
    (11, '2001: A Space Odyssey', 'gb', 1968, 149),
    (12, 'La Strada',             'it', 1954, NULL);

-- ------------------------------------------------------------------ 人物
-- first_name / died 可空,gender 有 DEFAULT(第 8 个人不给 gender,靠它补 '?')
CREATE TABLE people (
    peopleid   INT         NOT NULL PRIMARY KEY,
    first_name VARCHAR(30)
                           CONSTRAINT "名不超过 30"
                             CHECK (first_name IS NULL OR length(first_name) <= 30),
    surname    VARCHAR(30) NOT NULL
                           CONSTRAINT "姓不超过 30"
                             CHECK (length(surname) <= 30),
    born       INT         NOT NULL
                           CONSTRAINT "出生年范围"
                             CHECK (born BETWEEN 1800 AND 2100),
    died       INT
                           CONSTRAINT "卒年不早于生年"
                             CHECK (died IS NULL OR died > born),
    gender     CHAR(1)     NOT NULL DEFAULT '?',
    UNIQUE (surname, first_name)
);

INSERT INTO people (peopleid, first_name, surname, born, died, gender) VALUES
    (1, 'Ridley',      'Scott',      1937, NULL, 'M'),
    (2, 'Akira',       'Kurosawa',   1910, 1998, 'M'),
    (3, 'Jean-Pierre', 'Melville',   1917, 1973, 'M'),
    (4, 'Fernando',    'Meirelles',  1955, NULL, 'M'),
    (5, 'Alfred',      'Hitchcock',  1899, 1980, 'M'),
    (6, 'Stanley',     'Kubrick',    1928, 1999, 'M'),
    (7, 'Federico',    'Fellini',    1920, 1993, 'M');

-- 不给 gender:它由 DEFAULT 补成 '?'
INSERT INTO people (peopleid, first_name, surname, born, died) VALUES
    (8, NULL, 'Anonymous', 1900, NULL);

-- ---------------------------------------------------------------- 演职员
-- 复合主键;作品 12(La Strada)故意没有任何一行,给 LEFT JOIN 留一个空的可空侧
CREATE TABLE credits (
    movieid     INT     NOT NULL,
    peopleid    INT     NOT NULL,
    credited_as CHAR(1) NOT NULL
                        CONSTRAINT "署名只有三种"
                          CHECK (credited_as IN ('A', 'D', 'W')),
    PRIMARY KEY (movieid, peopleid, credited_as),
    FOREIGN KEY (movieid) REFERENCES movies (movieid),
    FOREIGN KEY (peopleid) REFERENCES people (peopleid)
);

INSERT INTO credits (movieid, peopleid, credited_as) VALUES
    ( 1, 1, 'D'), ( 1, 5, 'A'),
    ( 2, 1, 'D'), ( 2, 6, 'A'),
    ( 3, 3, 'D'), ( 3, 3, 'W'),
    ( 4, 2, 'D'), ( 4, 2, 'W'),
    ( 5, 4, 'D'),
    ( 6, 1, 'D'),
    ( 7, 7, 'D'),
    ( 8, 3, 'W'),
    ( 9, 2, 'W'),
    (10, 5, 'D'),
    (11, 6, 'D');

-- ------------------------------------------------------------ 论坛(日期列)
CREATE TABLE forum_members (
    memberid   INT         NOT NULL PRIMARY KEY,
    name       VARCHAR(30) NOT NULL,
    registered DATE        NOT NULL,
    UNIQUE (name)
);

INSERT INTO forum_members (memberid, name, registered) VALUES
    (1, 'ada',    '2023-01-05'),
    (2, 'brian',  '2024-02-11'),
    (3, 'cleo',   '2024-03-20'),
    (4, 'dmitri', '2023-06-01');

CREATE TABLE forum_topics (
    topicid   INT      NOT NULL PRIMARY KEY,
    post_date DATE     NOT NULL,
    memberid  INT      NOT NULL,
    message   TEXT     NOT NULL,
    FOREIGN KEY (memberid) REFERENCES forum_members (memberid)
);

INSERT INTO forum_topics (topicid, post_date, memberid, message) VALUES
    (1, '2024-01-06', 1, 'first topic'),
    (2, '2025-02-12', 2, 'second topic'),
    (3, '2024-03-21', 3, 'third topic');

CREATE TABLE forum_posts (
    postid    INT      NOT NULL PRIMARY KEY,
    topicid   INT      NOT NULL,
    post_date DATE     NOT NULL,
    memberid  INT      NOT NULL,
    ancestry  VARCHAR(1000),
    message   TEXT     NOT NULL,
    FOREIGN KEY (memberid) REFERENCES forum_members (memberid),
    FOREIGN KEY (topicid) REFERENCES forum_topics (topicid)
);

INSERT INTO forum_posts (postid, topicid, post_date, memberid, ancestry, message) VALUES
    (1, 1, '2024-01-06', 1, NULL,  'opening post'),
    (2, 1, '2024-01-07', 2, '1',   'a reply'),
    (3, 1, '2024-01-08', 3, '1.2', 'another reply'),
    (4, 2, '2025-02-13', 2, NULL,  'answer'),
    (5, 3, '2024-03-22', 4, '3',   'late reply');

-- ------------------------------------------------------------- 法语片 / 别名
CREATE TABLE films_francais (
    titre VARCHAR(100) NOT NULL,
    annee INT          NOT NULL,
    PRIMARY KEY (titre, annee)
);

INSERT INTO films_francais (titre, annee) VALUES
    ('Le Samourai',            1967),
    ('Amelie',                 2001),
    ('Les Enfants du Paradis', 1945),
    ('La Regle du Jeu',        1939);

-- movieid 可空(第 5 行就是 NULL);外键带 ON DELETE CASCADE 的写法(只是记下来)
CREATE TABLE alt_titles (
    titleid INT          NOT NULL PRIMARY KEY,
    movieid INT,
    title   VARCHAR(250) NOT NULL,
    UNIQUE (movieid, title),
    FOREIGN KEY (movieid) REFERENCES movies (movieid)
        ON DELETE CASCADE
);

INSERT INTO alt_titles (titleid, movieid, title) VALUES
    (1, 1,    'Alien: The Eighth Passenger'),
    (2, 1,    'Star Beast'),
    (3, 3,    'The Samurai'),
    (4, 4,    'Shichinin no Samurai'),
    (5, NULL, 'Orphan Title'),
    (6, 8,    'Le Fabuleux Destin d Amelie Poulain');

-- ------------------------------------------------------- 两张"形状表"
-- 纯表:没有主键,也没有 CHECK
CREATE TABLE merge_people (
    id           INT,
    should_be_id INT
);

INSERT INTO merge_people (id, should_be_id) VALUES
    (1, 10),
    (2, 20),
    (3, 30);

-- **空表**:一个 0 行的表也要能建出来,`\dt` 里看得见
CREATE TABLE movie_title_ft_index2 (
    title_word VARCHAR(50) NOT NULL,
    movieid    INT         NOT NULL,
    titleid    INT         NOT NULL DEFAULT 1,
    PRIMARY KEY (title_word, movieid, titleid),
    FOREIGN KEY (movieid) REFERENCES movies (movieid)
        ON DELETE CASCADE,
    FOREIGN KEY (titleid) REFERENCES alt_titles (titleid)
);

COMMIT;
