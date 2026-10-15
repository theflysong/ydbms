//! i18n / l10n 的**核对**:文案表与代码是否对得上,中文有没有漏在外面,标点是不是半角.
//!
//! 这份测试是"收集全部中文文本"与"标点尽可能用半角"两条规矩的执行者--规矩写在根
//! `AGENTS.md` 的 "六,文案与标点" 里;这里一旦红,就说明有人往代码里塞了硬编码中文,
//! 全角标点,或者改了文案表却忘了改另一边.
//!
//! 检查四件事:
//!
//! 1. **键一一对应**:`src/**/*.rs` 里 `tr!("键")` 用到的键,与 `locales/zh-CN.yml`
//!    里的键**完全相同**(不多不少),而且调用点给的占位符与文案表里的 `%{…}` 一致;
//! 2. **两种语言同构**:`en.yml` 与 `zh-CN.yml` 的键与占位符一一对应;
//! 3. **中文只在文案表里**:`src/**/*.rs` 的**字符串字面量**里不许再出现中文
//!    (注释里可以--注释是给读代码的人看的,不翻译);
//! 4. **标点尽可能半角**:`src` 与 `tests` 下源码的注释与字符串,两份文案表,以及我们
//!    自己的 `*.md`,都不许出现"有半角对应体"的全角标点.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// 全角 → 半角:只列**有半角对应体**的那些(与根 `AGENTS.md` 的表一致)。
const FULLWIDTH: &[&str] = &[
    "——", "—", "。", "、", "“", "”", "‘", "’", "「", "」", "【", "】", "…", "，", "：", "；", "！",
    "？", "（", "）", "／", "～", "％", "＃", "＠", "＆", "＊", "＋", "－", "＝", "＜", "＞", "［",
    "］", "｛", "｝", "｜", "＼", "＾", "＿", "｀", "＄",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn walk_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("读目录") {
        let path = entry.expect("目录项").path();
        if path.is_dir() {
            walk_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// 只有 `src`:**测试里的中文是断言**(比对的就是渲染出来的中文),不搬进文案表。
fn src_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_rs(&root().join("src"), &mut out);
    out.sort();
    out
}

/// `src` 与 `tests` 下的源码(标点检查用)。跳过 `tests/i18n.rs`:它自己就装着那张
/// 全角对照表。
fn all_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_rs(&root().join("src"), &mut out);
    walk_rs(&root().join("tests"), &mut out);
    out.retain(|p| !p.ends_with("tests/i18n.rs"));
    out.sort();
    out
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Kind {
    Code,
    Comment,
    Str,
}

struct Seg {
    kind: Kind,
    start: usize,
    end: usize,
}

/// 够用的 Rust 词法:注释,字符串(含 `r#"…"#`),字符字面量与生命周期。
fn lex(src: &str) -> Vec<Seg> {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut out: Vec<Seg> = Vec::new();
    let mut i = 0;
    let mut code_start = 0;

    while i < n {
        let c = bytes[i];
        let two = bytes.get(i + 1).copied();
        if c == b'/' && two == Some(b'/') {
            out.push(Seg { kind: Kind::Code, start: code_start, end: i });
            let end = src[i..].find('\n').map(|k| i + k).unwrap_or(n);
            out.push(Seg { kind: Kind::Comment, start: i, end });
            i = end;
            code_start = i;
            continue;
        }
        if c == b'/' && two == Some(b'*') {
            out.push(Seg { kind: Kind::Code, start: code_start, end: i });
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if bytes[j] == b'/' && bytes.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            out.push(Seg { kind: Kind::Comment, start: i, end: j });
            i = j;
            code_start = i;
            continue;
        }
        if c == b'r' && matches!(two, Some(b'#') | Some(b'"')) {
            let mut j = i + 1;
            let mut hashes = 0;
            while bytes.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if bytes.get(j) == Some(&b'"') {
                let end_tag = format!("\"{}", "#".repeat(hashes));
                if let Some(k) = src[j + 1..].find(&end_tag) {
                    out.push(Seg { kind: Kind::Code, start: code_start, end: i });
                    out.push(Seg { kind: Kind::Str, start: j + 1, end: j + 1 + k });
                    i = j + 1 + k + end_tag.len();
                    code_start = i;
                    continue;
                }
            }
        }
        if c == b'"' {
            out.push(Seg { kind: Kind::Code, start: code_start, end: i });
            let mut j = i + 1;
            while j < n {
                if bytes[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if bytes[j] == b'"' {
                    break;
                }
                j += 1;
            }
            out.push(Seg { kind: Kind::Str, start: i + 1, end: j.min(n) });
            i = j + 1;
            code_start = i;
            continue;
        }
        if c == b'\'' {
            // `'a` 是生命周期,`'a'` 才是字符字面量
            let next = bytes.get(i + 1).copied();
            let after = bytes.get(i + 2).copied();
            let lifetime = next
                .map(|b| b.is_ascii_alphanumeric() || b == b'_')
                .unwrap_or(false)
                && after != Some(b'\'');
            if !lifetime {
                let mut j = i + 1;
                while j < n {
                    if bytes[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if bytes[j] == b'\'' {
                        break;
                    }
                    j += 1;
                }
                i = j + 1;
                code_start = i;
                continue;
            }
        }
        i += 1;
    }
    out.push(Seg { kind: Kind::Code, start: code_start, end: n });
    out.retain(|s| s.end > s.start);
    out
}

fn text<'a>(src: &'a str, seg: &Seg) -> &'a str {
    &src[seg.start..seg.end]
}

fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

fn line_of(src: &str, at: usize) -> usize {
    src[..at.min(src.len())].lines().count().max(1)
}

/// 文案表:一行一条 `键: "值"`。
fn yaml_entries(path: &Path) -> Vec<(String, String)> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读不了 {}:{e}", path.display()));
    let mut out = Vec::new();
    for (ln, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') || line.starts_with("_version") {
            continue;
        }
        let (key, value) = line
            .split_once(": ")
            .unwrap_or_else(|| panic!("{} 第 {} 行不像 `键: 值`:{line}", path.display(), ln + 1));
        let value = value.trim();
        assert!(
            value.starts_with('"') && value.ends_with('"') && value.len() >= 2,
            "{} 第 {} 行的值要写成双引号字符串:{line}",
            path.display(),
            ln + 1
        );
        out.push((key.to_string(), value[1..value.len() - 1].to_string()));
    }
    out
}

/// `%{名字}` 占位符的集合。
fn placeholders(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'%' && bytes[i + 1] == b'{' {
            if let Some(k) = text[i + 2..].find('}') {
                out.insert(text[i + 2..i + 2 + k].to_string());
                i += 2 + k;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// 代码里 `tr!(...)` 用到的键 → 调用点给的占位符名字。
///
/// 注意不能"只在 code 段里找 `tr!(\"`":**键本身就是字符串字面量**,词法器在引号处
/// 就把 code 段截断了。所以这里在整段源码里找 `tr!`,再按段的位置判断它是不是**代码**
/// (文档注释里的示例不算),然后往后读那个字符串。
fn calls_in(src: &str) -> BTreeMap<String, BTreeSet<String>> {
    let segs = lex(src);
    let at_kind = |at: usize, kind: Kind| {
        segs.iter()
            .any(|s| s.kind == kind && s.start <= at && at < s.end)
    };

    let mut out = BTreeMap::new();
    let mut from = 0;
    while let Some(rel) = src[from..].find("tr!(") {
        let at = from + rel;
        from = at + 5;
        // 词边界:`include_str!(` 里也有 `tr!(`(是 `..._s` + `tr!(`),那不算调用
        if at > 0 {
            let prev = src.as_bytes()[at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        if at_kind(at, Kind::Comment) || at_kind(at, Kind::Str) {
            continue;
        }
        let key_start = at + 4;
        if src.as_bytes().get(key_start) != Some(&b'"') {
            continue;
        }
        let Some(close_rel) = src[key_start + 1..].find('"') else {
            continue;
        };
        let key = src[key_start + 1..key_start + 1 + close_rel].to_string();

        // 从 `(` 起做括号配平(字符串里的括号不算)
        let mut depth = 0;
        let mut j = at + 3;
        let bytes = src.as_bytes();
        while j < src.len() {
            if at_kind(j, Kind::Str) {
                j += 1;
                continue;
            }
            match bytes[j] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let body = &src[key_start + 1 + close_rel + 1..j];
        let names = split_top_level(body)
            .into_iter()
            .filter_map(|arg| {
                let (name, _) = arg.split_once('=')?;
                let name = name.trim();
                let plain = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_');
                plain.then(|| name.to_string())
            })
            .collect();
        out.insert(key, names);
    }
    out
}

/// 按顶层逗号切参数(括号配平,字符串里的逗号不算)。
fn split_top_level(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let (mut depth, mut last, mut i) = (0i32, 0usize, 0usize);
    while i < body.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < body.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&body[last..i]);
                last = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&body[last..]);
    out
}

#[test]
fn every_key_used_in_code_exists_in_the_locale_files() {
    let root = root();
    let zh = yaml_entries(&root.join("locales/zh-CN.yml"));
    let en = yaml_entries(&root.join("locales/en.yml"));

    let zh_keys: Vec<String> = zh.iter().map(|(k, _)| k.clone()).collect();
    let en_keys: Vec<String> = en.iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(zh_keys, en_keys, "en.yml 与 zh-CN.yml 的键与顺序必须完全一致");

    let mut used: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in src_sources() {
        if path.ends_with("src/i18n.rs") {
            continue; // 它的 `tr!` 只出现在文档注释的示例里
        }
        let src = std::fs::read_to_string(&path).expect("读源文件");
        for (key, names) in calls_in(&src) {
            used.insert(key, names);
        }
    }
    assert!(
        used.len() > 400,
        "只找到 {} 个 tr! 调用,词法是不是坏了?",
        used.len()
    );

    let missing: Vec<&String> = used.keys().filter(|k| !zh_keys.contains(k)).collect();
    assert!(missing.is_empty(), "代码里用了,文案表里没有:{missing:#?}");

    let unused: Vec<&String> = zh_keys.iter().filter(|k| !used.contains_key(*k)).collect();
    assert!(unused.is_empty(), "文案表里有,代码里没人用:{unused:#?}");

    for (key, names) in &used {
        let value = &zh.iter().find(|(k, _)| k == key).expect("上面查过").1;
        assert_eq!(
            &placeholders(value),
            names,
            "{key} 的占位符对不上:文案表 {value:?},调用点 {names:?}"
        );
    }
    for (key, value) in &zh {
        let en_value = &en.iter().find(|(k, _)| k == key).expect("键一致").1;
        assert_eq!(
            placeholders(value),
            placeholders(en_value),
            "{key} 的占位符在中英两边对不上"
        );
    }
}

#[test]
fn chinese_lives_in_the_locale_files_only() {
    // `src` 的字符串字面量里不许再有中文--文案一律走 `tr!("…")`。
    for path in src_sources() {
        let src = std::fs::read_to_string(&path).expect("读源文件");
        for seg in lex(&src) {
            if seg.kind == Kind::Str && has_cjk(text(&src, &seg)) {
                panic!(
                    "{}:{} 的字符串字面量里还有中文(要搬进 locales/):{:?}",
                    path.display(),
                    line_of(&src, seg.start),
                    text(&src, &seg).chars().take(40).collect::<String>()
                );
            }
        }
    }
}

#[test]
fn punctuation_is_halfwidth() {
    let mut offenders = Vec::new();

    let mut check = |path: &Path, line: usize, body: &str| {
        for ch in FULLWIDTH {
            if body.contains(ch) {
                offenders.push(format!("{}:{line} 有 `{ch}`", path.display()));
            }
        }
    };

    for path in all_sources() {
        let src = std::fs::read_to_string(&path).expect("读源文件");
        for seg in lex(&src) {
            if seg.kind == Kind::Code {
                continue;
            }
            check(&path, line_of(&src, seg.start), text(&src, &seg));
        }
    }
    for path in [root().join("locales/zh-CN.yml"), root().join("locales/en.yml")] {
        let src = std::fs::read_to_string(&path).expect("读文案表");
        check(&path, 1, &src);
    }
    let mut md = vec![root().join("AGENTS.md"), root().join("TODO.md")];
    for dir in ["src/cmd", "src/db", "src/ir", "src/pers", "src/usr"] {
        md.push(root().join(dir).join("AGENTS.md"));
    }
    for path in md {
        let src = std::fs::read_to_string(&path).expect("读 markdown");
        check(&path, 1, &src);
    }

    assert!(
        offenders.is_empty(),
        "这些地方该用半角标点:\n{}",
        offenders.join("\n")
    );
}
