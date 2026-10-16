//! [`HeapTab`] 与 [`BufferPool`] 的 testbench(TODO 7.5).
//!
//! 主线与 `pers_csv.rs` 一样是"存到磁盘再读回来逐格核对",外加页式堆特有的几件事:
//! 行目录随写入走,写只追加(旧版本留到整理),缓冲池按预算淘汰,以及**页缓存的键
//! 必须带文件**(两张表共用一份池时不能互相串页).

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::db::table::{Column, ColumnsShape, DataType, Table, Value};
use ydbms::pers::heap::{BufferPool, HeapFile, HeapTab, PAGE_SIZE};
use ydbms::pers::TablePath;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 自清理的临时目录(与别的 testbench 同一个写法,不引入依赖).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-heap-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        TempDir { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn shape() -> ColumnsShape {
    ColumnsShape::new(vec![
        Column::new("x", DataType::Int),
        Column::new("note", DataType::Text),
    ])
}

/// 打开一份堆表(池很小,顺便让淘汰天天发生).
fn open(dir: &TempDir, name: &str, pool: &Rc<BufferPool>, bytes: u64, rows: usize) -> HeapTab {
    let path = dir.path().join(format!("{name}.heap"));
    let file = Rc::new(HeapFile::open(&path, bytes, Rc::clone(pool)).expect("打开堆文件"));
    HeapTab::open(name, shape(), file, rows).expect("打开堆表")
}

/// 往一张表里塞 `n` 行,`x` 是行号.
fn fill(table: &mut HeapTab, n: usize) {
    for x in 0..n {
        table
            .insert(vec![Value::Int(x as i32), Value::Text(format!("n{x}"))])
            .expect("插入");
    }
}

#[test]
fn a_heap_table_round_trips_through_the_file() {
    let dir = TempDir::new("round-trip");
    let pool = Rc::new(BufferPool::new(PAGE_SIZE));

    {
        let mut table = open(&dir, "t", &pool, 0, 0);
        fill(&mut table, 200);
        table
            .update_row(7, vec![Value::Int(7), Value::Text("changed".into())])
            .expect("改一行");
        assert_eq!(table.nrows(), 200);
        assert_eq!(table.row(7).expect("第 7 行")[1], Value::Text("changed".into()));
    }

    // 提交点记下"写到多少字节,有多少行":这就是重启之后认的那两个数
    let (bytes, rows) = {
        let mut table = open(&dir, "t", &pool, 0, 0);
        fill(&mut table, 200);
        (table.footprint(), table)
    };
    let (bytes, rows) = (bytes.0, rows.nrows());

    let reopened = open(&dir, "t", &pool, bytes, rows);
    assert_eq!(reopened.nrows(), 200);
    assert_eq!(reopened.row(0).expect("第 0 行")[0], Value::Int(0));
    assert_eq!(reopened.row(199).expect("第 199 行")[1], Value::Text("n199".into()));
}

#[test]
fn an_extra_record_outside_the_committed_length_is_invisible() {
    // 追加式写的原子性:账本之外的字节不算数(打开时截掉)
    let dir = TempDir::new("uncommitted");
    let pool = Rc::new(BufferPool::new(PAGE_SIZE));

    let (bytes, rows) = {
        let mut table = open(&dir, "t", &pool, 0, 0);
        fill(&mut table, 10);
        (table.footprint().0, table.nrows())
    };

    // 手工追加一条"同 rid"的新记录(模拟提交崩在半路)
    {
        let mut table = open(&dir, "t", &pool, bytes, rows);
        table
            .update_row(3, vec![Value::Int(3), Value::Text("lost".into())])
            .expect("改一行");
        // 不调 `mark_committed`:文件长了,账本没长
    }

    let table = open(&dir, "t", &pool, bytes, rows);
    assert_eq!(
        table.row(3).unwrap()[1],
        Value::Text("n3".into()),
        "没越过提交点的那条不算数"
    );
    assert_eq!(std::fs::metadata(dir.path().join("t.heap")).unwrap().len(), bytes);
}

#[test]
fn a_truncated_table_reopens_without_the_dropped_rows() {
    let dir = TempDir::new("truncate");
    let pool = Rc::new(BufferPool::new(PAGE_SIZE));

    let (bytes, rows) = {
        let mut table = open(&dir, "t", &pool, 0, 0);
        fill(&mut table, 50);
        table.truncate_to(20).expect("截断");
        (table.footprint().0, table.nrows())
    };

    let table = open(&dir, "t", &pool, bytes, rows);
    assert_eq!(table.nrows(), 20);
    assert_eq!(table.row(19).unwrap()[0], Value::Int(19));
}

#[test]
fn the_buffer_pool_evicts_by_budget() {
    let dir = TempDir::new("pool");
    // 装得下 2 页的池
    let pool = Rc::new(BufferPool::new(2 * PAGE_SIZE));
    assert_eq!(pool.bytes(), 2 * PAGE_SIZE);

    let mut table = open(&dir, "t", &pool, 0, 0);
    // 写够 3 页的短行:每行约 20 字节
    fill(&mut table, 3 * PAGE_SIZE / 20);

    // 全部读一遍(顺序扫)
    let nrows = table.nrows();
    for r in 0..nrows {
        assert!(table.row(r).is_some(), "第 {r} 行");
    }
    assert!(pool.loaded() <= 2, "池里不该超过预算:{}", pool.loaded());

    // 再读一遍:有命中也有未命中(池装不下整张表)
    let (hits, misses) = pool.counts();
    for r in 0..nrows {
        assert!(table.row(r).is_some());
    }
    let (hits2, misses2) = pool.counts();
    assert!(misses2 > misses, "池装不下,第二遍还要读盘");
    assert!(hits2 >= hits, "命中数不该倒退");
}

#[test]
fn the_pool_keys_pages_by_file() {
    // 两张表共用一份池:它们的第 0 页不能互相串(实测踩过这个坑)
    let dir = TempDir::new("keys");
    let pool = Rc::new(BufferPool::new(8 * PAGE_SIZE));

    let mut first = open(&dir, "a", &pool, 0, 0);
    let mut second = open(&dir, "b", &pool, 0, 0);
    fill(&mut first, 5);
    fill(&mut second, 5);
    second
        .update_row(0, vec![Value::Int(0), Value::Text("b0".into())])
        .expect("改一行");

    assert_eq!(first.row(0).unwrap()[1], Value::Text("n0".into()));
    assert_eq!(second.row(0).unwrap()[1], Value::Text("b0".into()));
}

#[test]
fn a_long_field_can_cross_a_page_boundary() {
    // 一条记录可以跨页:读的时候按页拼起来
    let dir = TempDir::new("cross");
    let pool = Rc::new(BufferPool::new(PAGE_SIZE));
    let long = "z".repeat(4 * PAGE_SIZE);

    let (bytes, rows) = {
        let mut table = open(&dir, "t", &pool, 0, 0);
        // 先垫几行,让下一条记录正好骑在页边界上
        fill(&mut table, 30);
        table
            .insert(vec![Value::Int(7), Value::Text(long.clone())])
            .expect("插入");
        (table.footprint().0, table.nrows())
    };

    let table = open(&dir, "t", &pool, bytes, rows);
    assert_eq!(table.nrows(), 31);
    assert_eq!(table.row(30).unwrap()[1], Value::Text(long));
}

#[test]
fn the_data_file_is_written_once_and_read_many_times() {
    // 页式堆的常驻内存是"缓冲池 + 行目录":写完 500 行之后,行目录与文件对得上
    let dir = TempDir::new("footprint");
    let pool = Rc::new(BufferPool::new(PAGE_SIZE));
    let mut table = open(&dir, "t", &pool, 0, 0);
    fill(&mut table, 500);

    let (bytes, rows) = table.footprint();
    assert_eq!(rows, 500);
    assert_eq!(
        std::fs::metadata(dir.path().join("t.heap")).unwrap().len(),
        bytes,
        "文件长度就是交出去的那个数"
    );

    // 文件就一份(`<库>/<表>.heap`),不是每个会话一份
    let at = TablePath::new(dir.path(), "t");
    assert!(at.heap_path().is_file());
}
