//! 符号表：名字 → 符号的映射。
//!
//! 它是整个系统里"名字"的唯一权威。设计上要承接两件事：
//!
//! * **数据库加载时批量导入**：持久化层把表读回来之后，一次性
//!   [`SymbolTable::extend`] 进符号表，之后 `Scan` 就能按名字取到它。
//! * **`AS` 之类的构造往表里加东西**：给已有符号起个别名——
//!   这就是 [`SymbolTable::alias`]，`Symbol::Alias` 是它的形状。
//!
//! 符号表**持有表实例**（`Box<dyn Table>`），而不只是形状，所以 `Scan` 按名字
//! 就能直接拿到数据；形状则通过 [`crate::db::table::TabShape`] 从表上取。

use std::fmt;

use crate::db::table::{Column, DbError, Result, Table};

/// 符号表里的一条记录。
pub enum Symbol {
    /// 一个关系：限定名 + 数据（数据同时提供形状）。
    Relation {
        /// 限定名，例如 `study.people`。
        name: String,
        /// 绑定的表。
        table: Box<dyn Table>,
    },
    /// 别名：新名字指向一个**已经存在**的名字。
    ///
    /// 这就是将来 `AS` 的落点。
    Alias {
        /// 别名。
        name: String,
        /// 被指向的符号名。
        target: String,
    },
}

impl Symbol {
    /// 造一个关系符号。接受 `impl Table` 省掉调用方手写 `Box::new`。
    pub fn relation<T: Table + 'static>(name: impl Into<String>, table: T) -> Self {
        Symbol::Relation {
            name: name.into(),
            table: Box::new(table),
        }
    }

    /// 造一个别名符号。
    pub fn alias(alias: impl Into<String>, target: impl Into<String>) -> Self {
        Symbol::Alias {
            name: alias.into(),
            target: target.into(),
        }
    }

    /// 符号的名字。
    pub fn name(&self) -> &str {
        match self {
            Symbol::Relation { name, .. } => name,
            Symbol::Alias { name, .. } => name,
        }
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Symbol::Relation { name, table } => f
                .debug_struct("Relation")
                .field("name", name)
                .field("columns", &table.columns())
                .finish(),
            Symbol::Alias { name, target } => f
                .debug_struct("Alias")
                .field("name", name)
                .field("target", target)
                .finish(),
        }
    }
}

/// 名字 → 符号的符号表。
///
/// 内部是一个插入有序的 `Vec`，线性查找。符号数量是个位数到几十，线性扫描比
/// 哈希表更快也更好读，而且顺序稳定、打印可预期。要换成 map 只是改这里。
#[derive(Debug, Default)]
pub struct SymbolTable {
    symbols: Vec<Symbol>,
}

impl SymbolTable {
    /// 空表。
    pub fn new() -> Self {
        SymbolTable {
            symbols: Vec::new(),
        }
    }

    /// 注册一个关系。重名 → [`DbError::DuplicateSymbol`]。
    pub fn define<T: Table + 'static>(
        &mut self,
        name: impl Into<String>,
        table: T,
    ) -> Result<()> {
        self.extend([Symbol::relation(name, table)])
    }

    /// 批量导入。
    ///
    /// 这就是"数据库被加载时初始化一批符号"的入口：持久化层读回一批表之后，
    /// 一次调用把它们全部装进符号表。整批是**原子的**——任何一条重名就整体失败，
    /// 不会留下装了一半的符号表。
    pub fn extend(&mut self, symbols: impl IntoIterator<Item = Symbol>) -> Result<()> {
        let mut pending: Vec<Symbol> = Vec::new();

        for symbol in symbols {
            let name = symbol.name().to_string();
            let clash = self.symbols.iter().any(|s| s.name() == name)
                || pending.iter().any(|s| s.name() == name);
            if clash {
                return Err(DbError::DuplicateSymbol { name });
            }
            pending.push(symbol);
        }

        self.symbols.extend(pending);
        Ok(())
    }

    /// 给一个**已经存在**的名字起别名（`AS` 的落点）。
    ///
    /// 目标必须已存在，且别名不能重名——这两条保证别名链天然无环，不需要环检测。
    pub fn alias(&mut self, alias: impl Into<String>, target: &str) -> Result<()> {
        if self.lookup(target).is_none() {
            return Err(DbError::UnknownSymbol {
                name: target.to_string(),
            });
        }
        self.extend([Symbol::alias(alias, target)])
    }

    /// 名字 → 符号（不穿透别名）。
    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|s| s.name() == name)
    }

    /// 名字 → 关系（**穿透别名**）。
    pub fn relation(&self, name: &str) -> Option<&dyn Table> {
        match self.resolve(name)? {
            Symbol::Relation { table, .. } => Some(&**table),
            // resolve 只会返回 Relation，这里只是让类型穷尽
            Symbol::Alias { .. } => None,
        }
    }

    /// 名字 → **可变**的表句柄（穿透别名）。
    ///
    /// `INSERT` 要改表内容，而 [`relation`](SymbolTable::relation) 是只读的。
    /// 实现上分两步：先用只读借用解出下标，再按下标取 `&mut`——这也是为什么
    /// 别名穿透要有一个返回下标的版本。
    pub fn relation_mut(&mut self, name: &str) -> Option<&mut Box<dyn Table>> {
        let index = self.resolve_index(name)?;
        match self.symbols.get_mut(index)? {
            Symbol::Relation { table, .. } => Some(table),
            Symbol::Alias { .. } => None,
        }
    }

    /// 名字 → 列定义（穿透别名）。
    pub fn columns(&self, name: &str) -> Option<&[Column]> {
        Some(self.relation(name)?.columns())
    }

    /// `(关系, 列名)` → 列下标（穿透别名）。
    ///
    /// 将来的 binder 解列名用它；testbench 也用它。
    pub fn column_index(&self, relation: &str, column: &str) -> Option<usize> {
        self.columns(relation)?
            .iter()
            .position(|c| c.name == column)
    }

    /// 已经注册的名字（含别名），按插入顺序。
    pub fn names(&self) -> Vec<&str> {
        self.symbols.iter().map(|s| s.name()).collect()
    }

    /// 顺着别名链找到最终的关系符号。
    ///
    /// 别名一定指向已存在的符号且名字不重复，所以链一定终止；这里的步数上限
    /// 只是防御性的，不依赖上面那个不变量。
    fn resolve(&self, name: &str) -> Option<&Symbol> {
        let index = self.resolve_index(name)?;
        self.symbols.get(index)
    }

    /// 同 [`resolve`](SymbolTable::resolve)，但返回下标而不是引用——
    /// 于是可以再拿它取可变借用。
    fn resolve_index(&self, name: &str) -> Option<usize> {
        let mut index = self.symbols.iter().position(|s| s.name() == name)?;
        for _ in 0..=self.symbols.len() {
            match &self.symbols[index] {
                Symbol::Relation { .. } => return Some(index),
                Symbol::Alias { target, .. } => {
                    index = self.symbols.iter().position(|s| s.name() == target)?;
                }
            }
        }
        None
    }
}
