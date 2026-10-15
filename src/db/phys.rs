//! 物理表 [`PhysTab`].
//!
//! 物理表把行真实地存放在内存里,是第一种具体的 [`Table`] 实现.它由某个
//! [`TabShape`] 生成:构造时把形状复制一份留作自己的 schema,之后再往里面
//! 插入行.

use super::table::{Column, ColumnsShape, DbError, Result, Row, Table, TabShape, Value};

/// 把行直接存放在内存中的物理表.
///
/// # 例子
///
/// ```
/// use ydbms::db::table::{Column, ColumnsShape, DataType, Table, Value};
/// use ydbms::db::PhysTab;
///
/// // 先用一个 tabshape 描述形状
/// let shape = ColumnsShape::new(vec![
///     Column::new("x", DataType::Int),
///     Column::new("value", DataType::Int),
///     Column::new("drvd", DataType::Int),
/// ]);
///
/// // 再由这个 shape 生成物理表
/// let mut tab = PhysTab::new("f", &shape);
/// for x in -5..=5 {
///     tab.insert(vec![Value::from(x), Value::from(x * x + 1), Value::from(2 * x)]).unwrap();
/// }
///
/// assert_eq!(tab.nrows(), 11);
/// assert_eq!(tab.value(0, 1), Some(Value::Int(26))); // f(-5) = 26
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysTab {
    name: String,
    shape: ColumnsShape,
    rows: Vec<Row>,
}

impl PhysTab {
    /// 由一个 tabshape 生成一张**空**的物理表.
    ///
    /// 形状会被复制一份:此后修改原来那个 shape 不会影响已经建好的表.
    /// 参数是 `&S where S: TabShape + ?Sized`,因此 `&dyn TabShape` 也能直接传进来.
    pub fn new<S: TabShape + ?Sized>(name: impl Into<String>, shape: &S) -> Self {
        PhysTab {
            name: name.into(),
            shape: ColumnsShape::new(shape.columns().to_vec()),
            rows: Vec::new(),
        }
    }

    /// 本表的形状.
    pub fn shape(&self) -> &ColumnsShape {
        &self.shape
    }

    /// 所有行.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// 借用第 `r` 行,不发生拷贝;越界返回 `None`.
    ///
    /// 这是 [`Table::row`] 的零拷贝版本,只属于物理表(虚表没有可借用的存储).
    pub fn row_ref(&self, r: usize) -> Option<&[Value]> {
        self.rows.get(r).map(|row| row.as_slice())
    }

    /// 借用第 `r` 行第 `c` 列的值,不发生拷贝;越界返回 `None`.
    pub fn cell_ref(&self, r: usize, c: usize) -> Option<&Value> {
        self.rows.get(r).and_then(|row| row.get(c))
    }

    /// 清空所有行,保留形状.幂等.
    pub fn truncate(&mut self) {
        self.rows.clear();
    }
}

impl TabShape for PhysTab {
    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }
}

impl Table for PhysTab {
    fn name(&self) -> &str {
        &self.name
    }

    fn nrows(&self) -> usize {
        self.rows.len()
    }

    fn row(&self, r: usize) -> Option<Row> {
        self.rows.get(r).cloned()
    }

    /// 单格读取:只克隆**一个**值,不克隆整行.
    ///
    /// 默认实现走 [`Table::row`] (整行克隆).索引建表要逐行读**一列**
    /// (见 [`crate::db::index`]),所以这个覆盖是有意义的.
    fn value(&self, r: usize, c: usize) -> Option<Value> {
        self.cell_ref(r, c).cloned()
    }

    fn insert(&mut self, row: Row) -> Result<usize> {
        // 先校验再落库:校验失败不会留下半行脏数据.
        self.validate(&row)?;
        self.rows.push(row);
        Ok(self.rows.len() - 1)
    }

    /// 快照就是克隆自己:物理表没有任何装饰要保留,克隆比逐行物化快得多.
    fn snapshot(&self) -> Box<dyn Table> {
        Box::new(self.clone())
    }

    /// 改一格.**先组成候选整行,过 `validate`,再落库**--`validate` 是
    /// typmod 与 `NOT NULL` 的唯一入口,不能在这里另写一套检查(两套迟早分叉:
    /// 只看大同类的老写法会把合法的 `VARCHAR(30)` 值判成类型不符).
    fn update(&mut self, r: usize, c: usize, value: Value) -> Result<()> {
        let nrows = self.rows.len();
        let mut row = self
            .row(r)
            .ok_or(DbError::RowOutOfBounds { index: r, nrows })?;
        let arity = self.arity();
        if c >= arity {
            return Err(DbError::ColumnOutOfBounds { index: c, arity });
        }
        row[c] = value;
        self.validate(&row)?;
        self.rows[r] = row;
        Ok(())
    }

    /// 用一整行替换第 `r` 行(`UPDATE` 的落库口).同样先 `validate` 再落库.
    fn update_row(&mut self, r: usize, row: Row) -> Result<()> {
        let nrows = self.rows.len();
        if r >= nrows {
            return Err(DbError::RowOutOfBounds { index: r, nrows });
        }
        self.validate(&row)?;
        self.rows[r] = row;
        Ok(())
    }

    fn delete(&mut self, r: usize) -> Result<()> {
        let nrows = self.rows.len();
        if r >= nrows {
            return Err(DbError::RowOutOfBounds { index: r, nrows });
        }
        self.rows.remove(r);
        Ok(())
    }
}
