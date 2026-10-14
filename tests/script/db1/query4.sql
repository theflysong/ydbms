-- query4.sql —— 子查询
-- 标量子查询、IN、EXISTS、派生表（FROM 里的子查询）、ALL。

-- [4-1] 标量子查询：绩点高于全校平均的记录
SELECT s.name AS student, c.code, g.gpa
FROM gpa_grades AS g
    JOIN students AS s ON s.student_id = g.student_id
    JOIN courses  AS c ON c.course_id  = g.course_id
WHERE g.gpa > (SELECT AVG(gpa) FROM gpa_grades)
ORDER BY g.gpa DESC, s.student_id
LIMIT 20;

-- [4-2] IN 子查询（两个子查询取并集）：至少有一门不及格的学生
SELECT s.student_id, s.name, s.major
FROM students AS s
WHERE s.student_id IN (
        SELECT g.student_id FROM gpa_grades AS g WHERE g.gpa < 1.00
        UNION
        SELECT p.student_id FROM pf_grades  AS p WHERE p.result = 'F'
      )
ORDER BY s.student_id;

-- [4-3] NOT EXISTS 相关子查询：从来没有不及格过的学生
SELECT s.student_id, s.name
FROM students AS s
WHERE NOT EXISTS (SELECT 1 FROM gpa_grades AS g
                  WHERE g.student_id = s.student_id AND g.gpa < 1.00)
  AND NOT EXISTS (SELECT 1 FROM pf_grades AS p
                  WHERE p.student_id = s.student_id AND p.result = 'F')
ORDER BY s.student_id;

-- [4-4] SELECT 列表里的相关标量子查询：逐行算每个学生的平均绩点
SELECT s.student_id, s.name, s.major,
       (SELECT ROUND(AVG(g.gpa), 2)
        FROM gpa_grades AS g
        WHERE g.student_id = s.student_id) AS avg_gpa
FROM students AS s
ORDER BY avg_gpa DESC, s.student_id
LIMIT 10;

-- [4-5] FROM 里的子查询（派生表）：平均绩点高于全校平均的课程
SELECT c.code, c.title, ROUND(t.avg_gpa, 2) AS course_avg
FROM courses AS c
    JOIN (SELECT course_id, AVG(gpa) AS avg_gpa
          FROM gpa_grades
          GROUP BY course_id) AS t ON t.course_id = c.course_id
WHERE t.avg_gpa > (SELECT AVG(gpa) FROM gpa_grades)
ORDER BY course_avg DESC;

-- [4-6] ALL：每一门 GPA 课都拿到 2.00 以上的学生
SELECT s.student_id, s.name
FROM students AS s
WHERE 2.00 <= ALL (SELECT g.gpa FROM gpa_grades AS g WHERE g.student_id = s.student_id)
ORDER BY s.student_id;
