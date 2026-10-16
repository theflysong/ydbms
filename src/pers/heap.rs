//! 页式堆文件 + 缓冲池:基表**不再常驻内存**(TODO 7.5).
//!
//! 磁盘上一份文件,加一个"读到哪儿算数"的账本(账本在库目录的 `manifest.json` 里,
//! 见 [`crate::usr::service`]):
//!
//! ```text
//! <name>.heap          记录流:每条 = [len u32][rid u32][行编码](只追加)
//! <name>.log           行位置日志:挪过窝的行在这里改地址(固定 16 字节一条)
//! <name>.schema.json   模式(与别的表共用,格式归 [`crate::pers`])
//! ```
//!
//! 读的时候按 **8 KiB 页**取([`BufferPool`] 按预算缓存,满了按 LRU 淘汰),`row(r)`
//! 先查**行目录**(内存里,每行 12 字节:页 / 页内偏移 / 长度)再去池里取那一段.
//! 行目录在打开时**流式扫一遍记录头**建出来(只读头,不读行体),于是常驻内存是
//! "缓冲池预算 + 12 字节/行",而不是整库的行.
//!
//! # 写只追加,账本在清单里
//!
//! 槽式定长页要**原地改**(行变长了就得挪槽,页头与槽数组跟着改),而本版**没有
//! WAL**,只保证"一次提交要么全在崩之前,要么全在崩之后"(见 [`crate::pers`]).
//! 追加式记录把这条原子性**免费**拿到:写只在文件尾追加,而**已提交的长度**记在
//! 清单里--一条记录要么落在长度之内(算数),要么落在外(打开库时截掉),没有第三种.
//! 于是 `UPDATE` 也是"再写一条新记录",旧版本留到下次整理.
//!
//! 唯一的例外是**行的地址**:`rid` 小于已提交行数时,它的地址在 `<name>.log` 里
//! (目录文件里那一份不能改).日志同样是追加式的,条数记在清单里--崩在半路的尾巴
//! 不算数,这与整个库其余部分的口径一致.
//!
//! # 一行怎么编码
//!
//! 逐列:`tag u8`(0 = NULL,1 = 有值)+ `u32 长度` + 该值的**CSV 字段文本**
//! ([`Value::to_csv_field`](crate::db::table::Value::to_csv_field)).字段口径与
//! 数据文件**完全相同**(NULL 是 `\N`,以 `\` 开头的文本多写一个 `\`),于是"值怎么
//! 落盘"仍然只有一处说法.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::db::table::{Column, ColumnsShape, DbError, Paged, Result, Row, TabShape, Table, Value};

/// 页大小:缓冲池按它算预算,读文件也按它取.
pub const PAGE_SIZE: usize = 8 * 1024;

/// 一条记录的固定头:`[len u32][rid u32]`.
const RECORD_HEADER: usize = 8;

/// 行位置日志里一条记录的字节数:`[rid u32][page u32][offset u32][len u32]`.
pub const MOVE_RECORD: usize = 16;

/// 缓冲池的默认预算(字节);`YDBMS_BUFFER_POOL_MB` 可以覆盖(见 `cmd::cli`).
pub const DEFAULT_POOL_BYTES: usize = 64 * 1024 * 1024;

/// 堆文件小于这个大小就不整理(重写一遍是 O(表) 的,小表不值得).
pub const COMPACT_MIN_BYTES: u64 = 4 * 1024 * 1024;

/// 一行在文件里的位置.
///
/// `page` 是**页号**(绝对偏移 / `PAGE_SIZE`),`offset` 是页内偏移,`len` 是行编码的
/// 字节数.一条记录可以跨页,读的时候按页拼起来.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Loc {
    page: u32,
    offset: u32,
    len: u32,
}

/// 堆文件的编号计数(页缓存的键要带它,见 [`BufferPool`]).
static NEXT_FILE_ID: AtomicU32 = AtomicU32::new(1);

/// 页缓存:按预算装页,满了按 **LRU** 淘汰.
///
/// 页一旦写下去就**不再改**(只追加),所以这里全是干净页--没有脏页回写,也没有
/// "淘汰时丢数据"的风险.池只是把读过的页留着,省掉重复的 `pread`.
///
/// **键是"哪份文件的第几页"**:一个库里的表共用一份池,只按页号做键会让两张表的
/// 第 0 页互相串(实测踩过:表 `u` 读出了表 `t` 的第 0 页).
#[derive(Debug)]
pub struct BufferPool {
    /// 页数预算(至少 1 页).
    capacity: usize,
    /// (文件编号, 页号) → (页内容, 最后一次用它的序号).
    pages: RefCell<HashMap<(u32, u32), (Rc<Vec<u8>>, u64)>>,
    /// 用过的顺序(序号, 键):**允许重复**,淘汰时惰性跳过过期的那些.
    ///
    /// 不用"每用一次就在表里搬一次位置"的那种写法:那要扫一遍全表,一次大扫描就是
    /// 十亿次操作(实测把打开库拖到 133 秒).这里每次访问只推一个队尾,淘汰才扫.
    queue: RefCell<VecDeque<(u64, (u32, u32))>>,
    /// 单调递增的访问序号.
    tick: Cell<u64>,
    /// 命中 / 未命中(测试与文档用).
    hits: Cell<usize>,
    misses: Cell<usize>,
}

impl BufferPool {
    /// 一份按字节预算的池.
    pub fn new(bytes: usize) -> Self {
        BufferPool {
            capacity: (bytes / PAGE_SIZE).max(1),
            pages: RefCell::new(HashMap::new()),
            queue: RefCell::new(VecDeque::new()),
            tick: Cell::new(0),
            hits: Cell::new(0),
            misses: Cell::new(0),
        }
    }

    /// 预算(字节).
    pub fn bytes(&self) -> usize {
        self.capacity * PAGE_SIZE
    }

    /// 现在装了几页.
    pub fn loaded(&self) -> usize {
        self.pages.borrow().len()
    }

    /// 命中了多少次 / 没命中多少次.
    pub fn counts(&self) -> (usize, usize) {
        (self.hits.get(), self.misses.get())
    }

    /// 取一页:池里有就用池里的,没有就读进来(读满一页,不够就是短页).
    fn page(&self, file_id: u32, file: &File, id: u32) -> Result<Rc<Vec<u8>>> {
        let key = (file_id, id);
        let cached = self.pages.borrow().get(&key).map(|(page, _)| Rc::clone(page));
        if let Some(page) = cached {
            self.hits.set(self.hits.get() + 1);
            self.touch(key);
            return Ok(page);
        }

        self.misses.set(self.misses.get() + 1);
        let mut buffer = vec![0u8; PAGE_SIZE];
        let mut reader = file;
        reader
            .seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))
            .map_err(|e| io_err("<heap>", e))?;
        let mut filled = 0usize;
        while filled < PAGE_SIZE {
            let read = reader
                .read(&mut buffer[filled..])
                .map_err(|e| io_err("<heap>", e))?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        buffer.truncate(filled);

        let page = Rc::new(buffer);
        let tick = self.next_tick();
        self.pages
            .borrow_mut()
            .insert(key, (Rc::clone(&page), tick));
        self.queue.borrow_mut().push_back((tick, key));
        self.evict();
        Ok(page)
    }

    /// 下一个访问序号.
    fn next_tick(&self) -> u64 {
        let tick = self.tick.get() + 1;
        self.tick.set(tick);
        tick
    }

    /// 丢掉某一页的缓存(文件在那一页上又写了新字节时用).
    ///
    /// "页不可变"指的是**已提交**的那些字节:追加式写只往后写,而"文件尾"通常就落在
    /// 缓存里那一页上--不丢它就会读到追加之前的旧内容(实测踩过:同一进程里改完一行,
    /// 下一次扫描读到的是旧页).
    fn invalidate(&self, file_id: u32, id: u32) {
        self.pages.borrow_mut().remove(&(file_id, id));
    }

    /// 把一页标成"最近用过".
    fn touch(&self, key: (u32, u32)) {
        let tick = self.next_tick();
        if let Some((_, current)) = self.pages.borrow_mut().get_mut(&key) {
            *current = tick;
        }
        self.queue.borrow_mut().push_back((tick, key));
    }

    /// 超预算就把最旧的丢出去(队首那些过期的条目直接跳过).
    fn evict(&self) {
        while self.pages.borrow().len() > self.capacity {
            let Some((tick, key)) = self.queue.borrow_mut().pop_front() else {
                return;
            };
            let stale = match self.pages.borrow().get(&key) {
                Some((_, current)) => *current != tick,
                None => true,
            };
            if stale {
                continue;
            }
            self.pages.borrow_mut().remove(&key);
        }
        // 队列会随访问次数变长(每次访问推一个),太长就按当前序号重建一遍
        if self.queue.borrow().len() > self.capacity * 8 + 1024 {
            let mut entries: Vec<(u64, (u32, u32))> = self
                .pages
                .borrow()
                .iter()
                .map(|(key, (_, tick))| (*tick, *key))
                .collect();
            entries.sort_unstable();
            *self.queue.borrow_mut() = entries.into();
        }
    }

}

/// 一份堆文件:文件句柄 + 已经写到哪儿 + 页缓存.
#[derive(Debug)]
pub struct HeapFile {
    path: PathBuf,
    /// 这份文件在页缓存里的编号(见 [`BufferPool`] 的键).
    id: u32,
    file: File,
    /// 现在文件里写到哪儿(字节).只增不减(整理是重写另一个文件再 rename).
    used: Cell<u64>,
    /// 打开时账本说的**已提交**长度(字节).
    committed: Cell<u64>,
    pool: Rc<BufferPool>,
}

impl HeapFile {
    /// 打开一份(可能还不存在的)堆文件.
    ///
    /// `committed` 是账本说的已提交长度:它之外的字节是崩在半路(或者回滚掉)的写,
    /// 打开时截掉--这就是"要么全在提交前,要么全在提交后"的落点.
    pub fn open(path: &Path, committed: u64, pool: Rc<BufferPool>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .map_err(|e| io_err(&path.display().to_string(), e))?;
        let size = file
            .metadata()
            .map_err(|e| io_err(&path.display().to_string(), e))?
            .len();

        // 账本之内的才算数:多出来的字节截掉(崩在半路的写)
        let keep = committed.min(size);
        if size > keep {
            file.set_len(keep)
                .map_err(|e| io_err(&path.display().to_string(), e))?;
        }
        // 账本说的比文件还长:那是坏的现场,宁可报错
        if committed > size {
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: None,
                message: format!("{}", tr!("pers.heap.009", arg0 = committed, arg1 = size)),
            });
        }

        Ok(HeapFile {
            path: path.to_path_buf(),
            id: NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed),
            file,
            used: Cell::new(keep),
            committed: Cell::new(keep),
            pool,
        })
    }

    /// 现在写到哪儿(字节).
    pub fn used(&self) -> u64 {
        self.used.get()
    }

    /// 已提交的长度.
    pub fn committed(&self) -> u64 {
        self.committed.get()
    }

    /// 提交成功之后认账:从这以后写的东西才算下一轮.
    pub fn mark_committed(&self, used: u64) {
        self.committed.set(used);
    }

    /// 追加一段字节,返回它落在哪儿.
    fn append(&self, bytes: &[u8]) -> Result<Loc> {
        let at = self.used.get();
        let page = (at / PAGE_SIZE as u64) as u32;
        let offset = (at % PAGE_SIZE as u64) as u32;
        let mut writer = &self.file;
        writer
            .seek(SeekFrom::Start(at))
            .map_err(|e| io_err(&self.name(), e))?;
        writer
            .write_all(bytes)
            .map_err(|e| io_err(&self.name(), e))?;
        self.used.set(at + bytes.len() as u64);
        // 写到的页从缓存里丢掉:那些字节已经变了(见 `BufferPool::invalidate`)
        if !bytes.is_empty() {
            let first = (at / PAGE_SIZE as u64) as u32;
            let last = ((at + bytes.len() as u64 - 1) / PAGE_SIZE as u64) as u32;
            for id in first..=last {
                self.pool.invalidate(self.id, id);
            }
        }
        Ok(Loc {
            page,
            offset,
            len: bytes.len() as u32,
        })
    }

    /// 读一段字节(可能跨页,按页拼).
    fn read(&self, loc: Loc) -> Result<Vec<u8>> {
        let at = loc.page as u64 * PAGE_SIZE as u64 + loc.offset as u64;
        let mut out = Vec::with_capacity(loc.len as usize);
        let mut page = (at / PAGE_SIZE as u64) as u32;
        let mut offset = (at % PAGE_SIZE as u64) as usize;
        while out.len() < loc.len as usize {
            let bytes = self.pool.page(self.id, &self.file, page)?;
            if offset >= bytes.len() {
                return Err(DbError::IrRuntime {
                    node: None,
                    message: format!(
                        "{}",
                        tr!("pers.heap.001", arg0 = self.name(), arg1 = page, arg2 = offset)
                    ),
                });
            }
            let take = (loc.len as usize - out.len()).min(bytes.len() - offset);
            out.extend_from_slice(&bytes[offset..offset + take]);
            page += 1;
            offset = 0;
        }
        Ok(out)
    }

    fn name(&self) -> String {
        self.path.display().to_string()
    }
}

/// 一张**页式基表**:数据在磁盘上,按行号随机访问,写只追加.
///
/// [`Table`] 那一套(`nrows` / `row` / `insert` / `update_row` / `truncate_to`)与内存里
/// 的 [`PhysTab`](crate::db::phys::PhysTab) 语义完全一致,区别只在于行不在内存里:
/// 每次读按行目录去页缓存取那一段再解码.
#[derive(Debug)]
pub struct HeapTab {
    name: String,
    shape: ColumnsShape,
    file: Rc<HeapFile>,
    /// 行目录:`rid → 这一行在文件里的位置`.打开时扫出来,之后随写入更新.
    dir: Vec<Loc>,
    /// 行目录里已经**提交**的行数(打开时账本说的那个).
    ///
    /// `Cell`:提交点(`Paged::mark_committed`)手里只有 `&self`.
    committed_rows: Cell<usize>,
    /// 地址动过的,编号在已提交范围之内的那些行(提交时要写进 `<表>.log`).
    moved: RefCell<Vec<usize>>,
    /// **活着的**字节数(每条记录的 8 字节头 + 行体,按当前行目录算).
    ///
    /// 与文件长度一比就知道有多少死字节(旧版本)--整理与否看它.
    live: u64,
}

impl HeapTab {
    /// 打开一份堆文件:扫一遍记录头把行目录建出来.
    ///
    /// 只认**已提交**长度之内的记录,而且只认 `rid < committed_rows` 的那些:同一个
    /// 行号出现多次时**后出现的赢**(`UPDATE` 是再写一条新记录).目录里出现空洞
    /// (某个 `rid` 从头到尾没见过)就报错--那是坏的现场,宁可开不了库.
    pub fn open(
        name: &str,
        shape: ColumnsShape,
        file: Rc<HeapFile>,
        committed_rows: usize,
    ) -> Result<Self> {
        let placeholder = Loc {
            page: 0,
            offset: 0,
            len: 0,
        };
        let mut dir = vec![placeholder; committed_rows];
        let mut seen = vec![false; committed_rows];
        let committed = file.committed();
        let mut reader = BufReader::with_capacity(1 << 20, &file.file);
        let mut at = 0u64;
        let mut header = [0u8; RECORD_HEADER];

        // **顺序**读:一条接一条(记录头 + 跳过行体),不要每条都 seek--`BufReader`
        // 一 seek 就把缓冲区丢了,一百多万条记录就是一百多万次 pread(实测把打开库
        // 从几秒拖到几分钟).
        while at + RECORD_HEADER as u64 <= committed {
            reader
                .read_exact(&mut header)
                .map_err(|e| io_err(&file.name(), e))?;
            let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as u64;
            let rid = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
            let start = at + RECORD_HEADER as u64;
            let end = start + len;
            if end > committed {
                // 账本之内的记录必须是完整的:半截记录说明文件被人动过
                return Err(DbError::Format {
                    path: file.name(),
                    line: None,
                    message: format!("{}", tr!("pers.heap.002", arg0 = file.name(), arg1 = at)),
                });
            }
            if rid < committed_rows {
                dir[rid] = Loc {
                    page: (start / PAGE_SIZE as u64) as u32,
                    offset: (start % PAGE_SIZE as u64) as u32,
                    len: len as u32,
                };
                seen[rid] = true;
            }
            // 跳过行体(顺序读,走 `BufReader` 那块缓冲)
            std::io::copy(&mut (&mut reader).take(len), &mut std::io::sink())
                .map_err(|e| io_err(&file.name(), e))?;
            at = end;
        }

        if let Some(missing) = seen.iter().position(|ok| !ok) {
            return Err(DbError::Format {
                path: file.name(),
                line: None,
                message: format!("{}", tr!("pers.heap.003", arg0 = file.name(), arg1 = missing)),
            });
        }

        let live = dir
            .iter()
            .map(|at| RECORD_HEADER as u64 + at.len as u64)
            .sum();

        Ok(HeapTab {
            name: name.to_string(),
            shape,
            file,
            dir,
            committed_rows: Cell::new(committed_rows),
            moved: RefCell::new(Vec::new()),
            live,
        })
    }

    /// 把 `<表>.log` 里的**行位置**盖回目录(打开时用,`records` 是已提交的条数).
    pub fn apply_moved_log(&mut self, path: &Path, records: usize) -> Result<()> {
        if records == 0 {
            return Ok(());
        }
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DbError::Format {
                    path: path.display().to_string(),
                    line: None,
                    message: format!("{}", tr!("pers.heap.010", arg0 = records)),
                })
            }
            Err(e) => return Err(io_err(&path.display().to_string(), e)),
        };
        let mut reader = BufReader::new(file);
        let mut record = [0u8; MOVE_RECORD];
        for at in 0..records {
            reader
                .read_exact(&mut record)
                .map_err(|e| io_err(&path.display().to_string(), e))?;
            let rid =
                u32::from_le_bytes([record[0], record[1], record[2], record[3]]) as usize;
            let loc = Loc {
                page: u32::from_le_bytes([record[4], record[5], record[6], record[7]]),
                offset: u32::from_le_bytes([record[8], record[9], record[10], record[11]]),
                len: u32::from_le_bytes([record[12], record[13], record[14], record[15]]),
            };
            if rid >= self.dir.len() {
                return Err(DbError::Format {
                    path: path.display().to_string(),
                    line: Some(at + 1),
                    message: format!("{}", tr!("pers.heap.011", arg0 = rid, arg1 = self.dir.len())),
                });
            }
            self.dir[rid] = loc;
        }
        Ok(())
    }

    /// 提交点:把"地址动过的行"追加到 `<表>.log`,返回日志里现在有多少条.
    ///
    /// `committed` 是日志里**已经算数**的条数:先截到那儿(上一次崩在半路的尾巴,
    /// 已经作废的旧地址都在那之后),再追加.
    pub fn write_moved_log(&self, path: &Path, committed: usize) -> Result<usize> {
        truncate_moves(path, committed)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| io_err(&path.display().to_string(), e))?;
        for rid in self.moved.borrow().iter() {
            let loc = self.dir[*rid];
            let mut record = [0u8; MOVE_RECORD];
            record[0..4].copy_from_slice(&(*rid as u32).to_le_bytes());
            record[4..8].copy_from_slice(&loc.page.to_le_bytes());
            record[8..12].copy_from_slice(&loc.offset.to_le_bytes());
            record[12..16].copy_from_slice(&loc.len.to_le_bytes());
            file.write_all(&record)
                .map_err(|e| io_err(&path.display().to_string(), e))?;
        }
        Ok(committed + self.moved.borrow().len())
    }

    /// 提交成功之后认账:文件长度与行数都变成"已提交".
    ///
    /// 提交点(`usr::service`)在清单写成功之后调它,之后再写的东西才算下一轮.
    pub fn mark_committed(&self) {
        self.file.mark_committed(self.file.used());
        self.committed_rows.set(self.dir.len());
        self.moved.borrow_mut().clear();
    }

    /// 交出去的位置:文件写到多少字节,目录有多少行.
    pub fn footprint(&self) -> (u64, usize) {
        (self.file.used(), self.dir.len())
    }

    /// 活着的字节数(死字节 = 文件长度 - 它).
    pub fn live_bytes(&self) -> u64 {
        self.live
    }

    /// 死字节是不是攒得太多了(该按当前内容密集重写一遍).
    ///
    /// 判据:`文件 > 活着 x 2` 且文件本身够大(`COMPACT_MIN_BYTES`).重写一遍是
    /// O(表) 的,所以小表不值得;而大表里死字节多了,每一次读都要跨过它们.
    pub fn needs_compaction(&self) -> bool {
        let used = self.file.used();
        used >= COMPACT_MIN_BYTES && used > self.live.saturating_mul(2)
    }

    /// 把一张**内存里**的表写成一份密集的堆文件(迁移与整理用),写到 `<path>.new`.
    ///
    /// **不自己推广**:推广那一步由提交点(或者打开库时的整理)说了算,清单里
    /// `promoted` 记着谁要搬(见 `usr::service`)--崩在中途只会留下一份 `.new` 残渣,
    /// 原文件照旧.返回新文件有多少字节.
    pub fn write_dense(path: &Path, table: &dyn Table) -> Result<u64> {
        let staged = path.with_extension("heap.new");
        let file = File::create(&staged).map_err(|e| io_err(&staged.display().to_string(), e))?;
        let mut writer = std::io::BufWriter::new(file);
        let mut written = 0u64;

        for r in 0..table.nrows() {
            let row = table.row(r).ok_or_else(|| DbError::IrRuntime {
                node: None,
                message: format!("{}", tr!("pers.heap.004", arg0 = r, arg1 = table.name())),
            })?;
            let payload = encode_row(&row, table.columns())?;
            writer
                .write_all(&(payload.len() as u32).to_le_bytes())
                .and_then(|()| writer.write_all(&(r as u32).to_le_bytes()))
                .and_then(|()| writer.write_all(&payload))
                .map_err(|e| io_err(&staged.display().to_string(), e))?;
            written += RECORD_HEADER as u64 + payload.len() as u64;
        }
        writer
            .flush()
            .map_err(|e| io_err(&staged.display().to_string(), e))?;
        Ok(written)
    }

    /// 第 `r` 行的位置.
    fn place(&self, r: usize) -> Option<Loc> {
        self.dir.get(r).copied()
    }
}

/// 暂存的堆文件名(`<name>.heap.new`).
pub fn staged_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".new");
    path.with_file_name(name)
}

/// 把暂存的堆文件搬进正式位置(幂等:已经搬过就没有 `.new` 了).
pub fn promote(path: &Path) -> Result<()> {
    let staged = staged_path(path);
    if !staged.exists() {
        return Ok(());
    }
    std::fs::rename(&staged, path).map_err(|e| io_err(&path.display().to_string(), e))
}

/// 把行位置日志截到前 `records` 条(不足这么多**完整**记录就报错).
pub fn truncate_moves(path: &Path, records: usize) -> Result<()> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if records == 0 {
                return Ok(());
            }
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: None,
                message: format!("{}", tr!("pers.heap.010", arg0 = records)),
            });
        }
        Err(e) => return Err(io_err(&path.display().to_string(), e)),
    };
    let size = file
        .metadata()
        .map_err(|e| io_err(&path.display().to_string(), e))?
        .len();
    let keep = records as u64 * MOVE_RECORD as u64;
    if size < keep {
        return Err(DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("pers.heap.012", arg0 = records)),
        });
    }
    if size > keep {
        file.set_len(keep)
            .map_err(|e| io_err(&path.display().to_string(), e))?;
    }
    Ok(())
}

impl TabShape for HeapTab {
    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }
}

impl Paged for HeapTab {
    fn footprint(&self) -> (u64, usize) {
        HeapTab::footprint(self)
    }

    fn needs_compaction(&self) -> bool {
        HeapTab::needs_compaction(self)
    }

    fn write_move_log(&self, log: &Path, committed: usize) -> Result<usize> {
        HeapTab::write_moved_log(self, log, committed)
    }

    fn mark_committed(&self) {
        HeapTab::mark_committed(self)
    }
}

impl Table for HeapTab {
    fn name(&self) -> &str {
        &self.name
    }

    fn nrows(&self) -> usize {
        self.dir.len()
    }

    /// 按行目录取一行,解码出来.
    ///
    /// **没有 `row_ref`**:值是从页里解码出来的,注定要拥有(`Value::Text(String)`),
    /// 借不出去.于是"借用行"那条快路径在页式基表上退回整行克隆(见
    /// `src/db/AGENTS.md` 的"借用行").
    fn row(&self, r: usize) -> Option<Row> {
        let at = self.place(r)?;
        let bytes = self.file.read(at).ok()?;
        decode_row(&bytes, self.columns()).ok()
    }

    /// **我自己落盘**(页式堆文件):提交点问这一句就知道不必整表重写.
    fn paged(&self) -> Option<&dyn Paged> {
        Some(self)
    }

    fn insert(&mut self, row: Row) -> Result<usize> {
        self.validate(&row)?;
        let payload = encode_row(&row, self.columns())?;
        let at = self.append_record(self.dir.len() as u32, &payload)?;
        self.dir.push(at);
        self.live += RECORD_HEADER as u64 + at.len as u64;
        Ok(self.dir.len() - 1)
    }

    /// 用一整行替换第 `r` 行:**再追加一条新记录**,目录改指它.
    ///
    /// 不原地改--已提交的那些字节一个都不许动(没有 WAL,原地改半截就成了"改了一半"
    /// 的库).旧版本留到下次整理,与 7.7 的日志同一个取舍.
    fn update_row(&mut self, r: usize, row: Row) -> Result<()> {
        let nrows = self.dir.len();
        if r >= nrows {
            return Err(DbError::RowOutOfBounds { index: r, nrows });
        }
        self.validate(&row)?;
        let payload = encode_row(&row, self.columns())?;
        let old = self.dir[r];
        let at = self.append_record(r as u32, &payload)?;
        // 活着的字节:这一行换了个记录,旧那条变成死字节
        self.live = self.live - RECORD_HEADER as u64 - old.len as u64
            + RECORD_HEADER as u64
            + at.len as u64;
        self.dir[r] = at;
        if r < self.committed_rows.get() && !self.moved.borrow().contains(&r) {
            // 已提交范围内的行挪了窝:新地址只能记进日志(目录文件里那一份不能改)
            self.moved.borrow_mut().push(r);
        }
        Ok(())
    }

    /// 截掉表尾的那些行(`n` 不小于现在的行数就什么都不做).
    ///
    /// 追加式的撤销记录用它:文件里那些字节留着(下次整理就没了),目录一截就看不见
    /// 了--而"目录有多长"记在清单里,所以重启之后也不会冒出来.
    fn truncate_to(&mut self, n: usize) -> Result<()> {
        while self.dir.len() > n {
            let last = self.dir.pop().expect("length checked above");
            self.live = self.live.saturating_sub(RECORD_HEADER as u64 + last.len as u64);
        }
        self.moved.borrow_mut().retain(|r| *r < n);
        Ok(())
    }
}

impl HeapTab {
    /// 把一条记录追加到文件尾,返回的是**行体**(不含 8 字节记录头)的位置.
    ///
    /// 目录里存的一律是行体的位置:打开时扫出来的那一份就是这么算的,写下去的这一份
    /// 必须与它一致--差一个记录头就会把行体读成编码头(实测踩过).
    fn append_record(&mut self, rid: u32, payload: &[u8]) -> Result<Loc> {
        let mut record = Vec::with_capacity(RECORD_HEADER + payload.len());
        record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        record.extend_from_slice(&rid.to_le_bytes());
        record.extend_from_slice(payload);
        let whole = self.file.append(&record)?;
        Ok(Loc {
            page: whole.page,
            offset: whole.offset + RECORD_HEADER as u32,
            len: payload.len() as u32,
        })
    }
}

/// 把一行编码成字节(逐列:`tag` + 长度 + CSV 字段文本).
pub fn encode_row(row: &Row, columns: &[Column]) -> Result<Vec<u8>> {
    if row.len() != columns.len() {
        return Err(DbError::ArityMismatch {
            expected: columns.len(),
            got: row.len(),
        });
    }
    let mut out = Vec::new();
    for value in row {
        match value {
            Value::Null => out.push(0u8),
            other => {
                let text = other.to_csv_field();
                out.push(1u8);
                out.extend_from_slice(&(text.len() as u32).to_le_bytes());
                out.extend_from_slice(text.as_bytes());
            }
        }
    }
    Ok(out)
}

/// 把字节解回一行.认不出来的编码一律报错(不猜).
pub fn decode_row(bytes: &[u8], columns: &[Column]) -> Result<Row> {
    let mut row = Row::with_capacity(columns.len());
    let mut at = 0usize;
    for column in columns {
        let tag = *bytes.get(at).ok_or_else(|| truncated(&column.name))?;
        at += 1;
        match tag {
            0 => row.push(Value::Null),
            1 => {
                let len_bytes = bytes
                    .get(at..at + 4)
                    .ok_or_else(|| truncated(&column.name))?;
                let len = u32::from_le_bytes([
                    len_bytes[0],
                    len_bytes[1],
                    len_bytes[2],
                    len_bytes[3],
                ]) as usize;
                at += 4;
                let text = bytes
                    .get(at..at + len)
                    .ok_or_else(|| truncated(&column.name))?;
                let text = std::str::from_utf8(text).map_err(|e| DbError::Format {
                    path: "<heap>".to_string(),
                    line: None,
                    message: format!("{}", tr!("pers.heap.005", arg0 = column.name, arg1 = e)),
                })?;
                at += len;
                row.push(column.ty.from_csv_field(text).map_err(|e| DbError::Format {
                    path: "<heap>".to_string(),
                    line: None,
                    message: format!("{}", tr!("pers.heap.006", arg0 = column.name, arg1 = e)),
                })?);
            }
            other => {
                return Err(DbError::Format {
                    path: "<heap>".to_string(),
                    line: None,
                    message: format!("{}", tr!("pers.heap.007", arg0 = column.name, arg1 = other)),
                })
            }
        }
    }
    Ok(row)
}

fn truncated(column: &str) -> DbError {
    DbError::Format {
        path: "<heap>".to_string(),
        line: None,
        message: format!("{}", tr!("pers.heap.008", arg0 = column)),
    }
}

fn io_err(path: &str, e: std::io::Error) -> DbError {
    DbError::Io {
        path: path.to_string(),
        kind: e.kind(),
        message: e.to_string(),
    }
}
