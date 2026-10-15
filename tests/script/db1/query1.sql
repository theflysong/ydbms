-- 课程清单(含授课教师与评分方式)
SELECT c.code, c.title, t.name AS teacher, c.credits, c.grade_mode
FROM courses AS c
    JOIN teachers AS t ON c.teacher_id = t.teacher_id
ORDER BY c.course_id;

-- A 档明细:3.67 及以上
SELECT s.student_id, s.name, c.code, g.gpa
FROM gpa_grades AS g
    JOIN students AS s ON g.student_id = s.student_id
    JOIN courses  AS c ON g.course_id  = c.course_id
WHERE g.gpa >= 3.67
ORDER BY s.student_id, c.code;

-- 不及格的 GPA 记录(低于 1.00)
SELECT s.student_id, s.name, c.code, g.gpa
FROM gpa_grades AS g
    JOIN students AS s ON g.student_id = s.student_id
    JOIN courses  AS c ON g.course_id  = c.course_id
WHERE g.gpa < 1.00
ORDER BY s.student_id, c.code;

-- 不及格的 P/F 记录
SELECT s.student_id, s.name, c.code, p.result
FROM pf_grades AS p
    JOIN students AS s ON p.student_id = s.student_id
    JOIN courses  AS c ON p.course_id  = c.course_id
WHERE p.result = 'F'
ORDER BY s.student_id, c.code;