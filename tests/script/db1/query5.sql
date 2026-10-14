-- query5.sql —— ORDER BY
-- 单列/多列、升降序混排、按表达式、按输出列序号、按聚合值、LIMIT/OFFSET 分页。

-- [5-1] 单列升序
SELECT s.student_id, s.name, s.major
FROM students AS s
ORDER BY s.name
LIMIT 10;

-- [5-2] 单列降序
SELECT s.student_id, s.name, s.birth_date
FROM students AS s
ORDER BY s.birth_date DESC
LIMIT 5;

-- [5-3] 多列、方向混排：先按专业升序，同一专业内按出生日期降序
SELECT s.major, s.name, s.birth_date
FROM students AS s
ORDER BY s.major ASC, s.birth_date DESC
LIMIT 12;

-- [5-4] 按表达式排序：入学年份倒序，再按姓名（排序列不出现在结果里）
SELECT s.student_id, s.name, s.enrolled_at
FROM students AS s
ORDER BY EXTRACT(YEAR FROM s.enrolled_at) DESC, s.name
LIMIT 8;

-- [5-5] 按输出列的位置排序（3 = credits，1 = code）
SELECT c.code, c.title, c.credits, c.grade_mode
FROM courses AS c
ORDER BY 3 DESC, 1
LIMIT 8;

-- [5-6] 按不在 SELECT 列表里的列排序
SELECT s.name, s.major
FROM students AS s
ORDER BY s.student_id DESC
LIMIT 5;

-- [5-7] 按聚合值排序
SELECT c.code,
       COUNT(*)             AS enrolled,
       ROUND(AVG(g.gpa), 2) AS avg_gpa
FROM courses AS c
    JOIN gpa_grades AS g ON g.course_id = c.course_id
GROUP BY c.course_id, c.code
ORDER BY avg_gpa DESC, enrolled DESC
LIMIT 5;

-- [5-8] LIMIT + OFFSET 分页：第 3 页（每页 8 条）
SELECT s.student_id, s.name
FROM students AS s
ORDER BY s.student_id
LIMIT 8 OFFSET 16;

-- [5-9] 排序后取每门课的高分记录
SELECT c.code, s.name AS student, g.gpa
FROM gpa_grades AS g
    JOIN students AS s ON s.student_id = g.student_id
    JOIN courses  AS c ON c.course_id  = g.course_id
ORDER BY c.code, g.gpa DESC
LIMIT 10;
