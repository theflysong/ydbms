-- query3.sql —— 聚合
-- COUNT / SUM / AVG / MIN / MAX，GROUP BY 与 HAVING。
--
-- 注意：本数据集里**每个学生都修了全部 20 门课**（13 门 GPA 制 + 7 门 P/F 制），
-- 所以按课程分组的 COUNT(*) 恒为 40、按学生分组的 COUNT(*) 恒为 13。
-- 计数列在这份数据里是常量，真正有区分度的是 AVG / MIN / MAX 与通过率。

-- [3-1] 每个学生的 GPA 课数、平均/最高/最低绩点
SELECT s.student_id, s.name,
       COUNT(*)             AS gpa_courses,
       ROUND(AVG(g.gpa), 2) AS avg_gpa,
       MIN(g.gpa)           AS min_gpa,
       MAX(g.gpa)           AS max_gpa
FROM students AS s
    JOIN gpa_grades AS g ON g.student_id = s.student_id
GROUP BY s.student_id, s.name
ORDER BY avg_gpa DESC, s.student_id
LIMIT 10;

-- [3-2] 每门 GPA 制课程的选课人数与平均绩点
SELECT c.code, c.title,
       COUNT(*)             AS enrolled,
       ROUND(AVG(g.gpa), 2) AS avg_gpa
FROM courses AS c
    JOIN gpa_grades AS g ON g.course_id = c.course_id
GROUP BY c.course_id, c.code, c.title
ORDER BY avg_gpa DESC;

-- [3-3] HAVING（按平均绩点筛组）：平均绩点 2.65 以上的课程
SELECT c.code, c.title,
       COUNT(*)             AS enrolled,
       ROUND(AVG(g.gpa), 2) AS avg_gpa
FROM courses AS c
    JOIN gpa_grades AS g ON g.course_id = c.course_id
GROUP BY c.course_id, c.code, c.title
HAVING AVG(g.gpa) >= 2.65
ORDER BY avg_gpa DESC;

-- [3-4] HAVING（按算出来的比例筛组）：P/F 通过率低于 90% 的课程
SELECT c.code, c.title,
       COUNT(*) AS enrolled,
       ROUND(100.0 * SUM(CASE WHEN p.result = 'P' THEN 1 ELSE 0 END) / COUNT(*), 1) AS pass_rate
FROM courses AS c
    JOIN pf_grades AS p ON p.course_id = c.course_id
GROUP BY c.course_id, c.code, c.title
HAVING 100.0 * SUM(CASE WHEN p.result = 'P' THEN 1 ELSE 0 END) / COUNT(*) < 90.0
ORDER BY pass_rate ASC;

-- [3-5] 按性别汇总
SELECT s.gender,
       COUNT(DISTINCT s.student_id) AS students,
       ROUND(AVG(g.gpa), 2)         AS avg_gpa
FROM students AS s
    JOIN gpa_grades AS g ON g.student_id = s.student_id
GROUP BY s.gender
ORDER BY s.gender;

-- [3-6] P/F 课程的通过率（CASE WHEN 分类计数）
SELECT c.code, c.title,
       COUNT(*)                                        AS enrolled,
       SUM(CASE WHEN p.result = 'P' THEN 1 ELSE 0 END) AS passed,
       SUM(CASE WHEN p.result = 'F' THEN 1 ELSE 0 END) AS failed,
       ROUND(100.0 * SUM(CASE WHEN p.result = 'P' THEN 1 ELSE 0 END) / COUNT(*), 1) AS pass_rate
FROM courses AS c
    JOIN pf_grades AS p ON p.course_id = c.course_id
GROUP BY c.course_id, c.code, c.title
ORDER BY pass_rate DESC, c.code;

-- [3-7] 不带 GROUP BY 的整体汇总
SELECT COUNT(*)          AS records,
       ROUND(AVG(gpa), 3) AS avg_gpa,
       MIN(gpa)          AS min_gpa,
       MAX(gpa)          AS max_gpa
FROM gpa_grades;

-- [3-8] 各院系教师人数
SELECT t.department, COUNT(*) AS teachers
FROM teachers AS t
GROUP BY t.department
ORDER BY teachers DESC, t.department;
