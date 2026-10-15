-- query2.sql -- 多表查询
-- 涉及 2~4 张表的连接:内连接,左外连接,自连接.

-- [2-1] 四表连接:成绩明细(学生 ⋈ 成绩 ⋈ 课程 ⋈ 教师)
SELECT s.student_id, s.name AS student, c.code, c.title,
       t.name AS teacher, g.gpa
FROM gpa_grades AS g
    JOIN students AS s ON s.student_id = g.student_id
    JOIN courses  AS c ON c.course_id  = g.course_id
    JOIN teachers AS t ON t.teacher_id = c.teacher_id
WHERE s.student_id <= 3
ORDER BY s.student_id, c.code;

-- [2-2] 教师开课一览(教师 ⋈ 课程)
SELECT t.teacher_id, t.name AS teacher, t.department,
       c.code, c.title, c.credits, c.grade_mode
FROM teachers AS t
    JOIN courses AS c ON c.teacher_id = t.teacher_id
ORDER BY t.teacher_id, c.code;

-- [2-3] P/F 成绩明细(学生 ⋈ P/F 成绩 ⋈ 课程)
SELECT s.student_id, s.name AS student, c.code, p.result
FROM pf_grades AS p
    JOIN students AS s ON s.student_id = p.student_id
    JOIN courses  AS c ON c.course_id  = p.course_id
WHERE s.student_id <= 5
ORDER BY s.student_id, c.code;

-- [2-4] 左外连接 + IS NULL:没有任何 GPA 记录的课程(即 P/F 制的课)
SELECT c.course_id, c.code, c.title, c.grade_mode
FROM courses AS c
    LEFT JOIN gpa_grades AS g ON g.course_id = c.course_id
WHERE g.course_id IS NULL
ORDER BY c.course_id;

-- [2-5] 自连接:同一门课里绩点相同的学生对(只看 CS101)
SELECT a.student_id AS student_a, b.student_id AS student_b, a.gpa
FROM gpa_grades AS a
    JOIN gpa_grades AS b
      ON b.course_id  = a.course_id
     AND b.gpa        = a.gpa
     AND a.student_id < b.student_id
WHERE a.course_id = 1
ORDER BY a.gpa DESC, a.student_id, b.student_id
LIMIT 15;
