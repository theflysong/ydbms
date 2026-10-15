//! 语言(i18n)与本地化(l10n).
//!
//! # 源语言是简体中文
//!
//! `locales/zh-CN.yml` 是**源文件**:所有给用户看的文本都在那儿(终端横幅,帮助,
//! 元命令的输出,每一条错误与状态行).`locales/en.yml` 是它的英文译文.两份文件的
//! **键一一对应**,`tests/i18n.rs` 会强制这一点--缺一条,多一条,占位符对不上,
//! 都在测试里红.
//!
//! # 键长什么样
//!
//! `模块.文件.序号`,例如 `cmd.engine.012`:序号是**该文件里第几条**含中文的字面量
//! (按出现顺序,从 `001` 起).这样加一条文案时不用先想名字,而 YAML 里按文件分节,
//! 找起来直白.**别手工改序号**:真要重排就整节重排,再照着 `tests/i18n.rs` 的失败
//! 信息修(它会同时指出"代码里用了,YAML 里没有"和"YAML 里有,代码里没人用"两种).
//!
//! # 调用点一律用 [`tr!`](crate::tr)
//!
//! ```ignore
//! self.msg(&tr!("cmd.engine.012", rows = table.nrows()));
//! ```
//!
//! * **不要**直接用 `rust_i18n::t!`:`tr!` 会先把默认语言定下来(见下);
//! * 占位符在两边的写法不一样:**YAML 里写 `%{名字}`,调用点写 `名字 = 表达式`**;
//!   需要格式说明符时写在调用点(`n = x : {:>4}`);
//! * 纯文本(模板里没有占位符)直接 `tr!("key")`,返回的是 `Cow<'static, str>`:
//!   要 `String` 就 `.into_owned()`,要 `&str` 就 `.as_ref()`.
//!
//! # 默认语言,以及怎么换
//!
//! **库自己不去猜环境**:第一次真的要产出文本时([`tr!`](crate::tr) 里那次
//! [`ensure_default`](crate::i18n::ensure_default))默认定成 [`DEFAULT_LOCALE`](crate::i18n::DEFAULT_LOCALE)--于是测试与嵌入方拿到的都是
//! 中文,不受开发机 `LANG` 的影响.两个二进制在入口**先**调 [`init_from_env`](crate::i18n::init_from_env):
//! `YDBMS_LANG` → `LC_ALL` → `LC_MESSAGES` → `LANG`,取第一个认得的;都认不出来就用
//! 默认值.**认不出来不报错**--语言不认识不该让程序跑不起来.
//!
//! 嵌入方想自己定就先 [`set_locale`](crate::i18n::set_locale) (它认了才算数),再建会话--`set_locale` 之后
//! [`ensure_default`](crate::i18n::ensure_default) 就不再插手了.
//!
//! # 加一种语言
//!
//! 1. 在 `locales/` 下加一份 `<locale>.yml`(键照抄 `zh-CN.yml`,值写译文);
//! 2. 把语言标签加进 [`KNOWN_LOCALES`](crate::i18n::KNOWN_LOCALES) (`resolve` 还要能认出它的前缀);
//! 3. 跑 `cargo test --test i18n`--它会核对该语言与源语言的键,占位符是否一一对应.

use std::sync::atomic::{AtomicBool, Ordering};

/// 源语言,也是默认语言.
///
/// 说"源语言"是因为 `locales/zh-CN.yml` 就是文案的原稿:**改文案先改它**,别的语言
/// 跟着译.
pub const DEFAULT_LOCALE: &str = "zh-CN";

/// 认得的所有语言(`locales/` 下一份文件一个,与 [`resolve`] 必须一致).
pub const KNOWN_LOCALES: &[&str] = &["zh-CN", "en"];

/// 有没有人**显式**定过语言(定过之后 [`ensure_default`](crate::i18n::ensure_default) 就不再动手).
static LOCALE_SET: AtomicBool = AtomicBool::new(false);

/// 把默认语言定下来--幂等,且只在没人显式定过时才动手.
///
/// `tr!` 每次查表前都会先调它(一个 `Relaxed` 的原子读,可以忽略不计).这么设计
/// 是因为 `rust-i18n` 的初始语言写死是 `en`:不钉一下,测试与嵌入方拿到的是英文,
/// 而它们(以及本版的日常使用)要的是中文.
pub fn ensure_default() {
    if LOCALE_SET.load(Ordering::Relaxed) {
        return;
    }
    LOCALE_SET.store(true, Ordering::Relaxed);
    rust_i18n::set_locale(DEFAULT_LOCALE);
}

/// 按环境定语言:`YDBMS_LANG` → `LC_ALL` → `LC_MESSAGES` → `LANG`(见 [`pick_from`]).
///
/// 两个二进制的 `main` 那一路在**做任何事之前**调它;库里不调([`ensure_default`]
/// 会兜住默认值).
pub fn init_from_env() {
    let picked = pick_from(&|name| std::env::var(name).ok());
    LOCALE_SET.store(true, Ordering::Relaxed);
    rust_i18n::set_locale(picked);
}

/// 从一组环境变量里挑第一个认得出来的语言;都不认就返回 [`DEFAULT_LOCALE`].
///
/// **纯函数**(环境由调用方给),于是"`zh_CN.UTF-8` 该不该算中文"这种事可以单测--
/// 与 [`crate::cmd::cli`] 里 `getenv` 的做法一致.
pub fn pick_from(getenv: &dyn Fn(&str) -> Option<String>) -> &'static str {
    for name in ["YDBMS_LANG", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(value) = getenv(name) {
            if let Some(locale) = resolve(&value) {
                return locale;
            }
        }
    }
    DEFAULT_LOCALE
}

/// 显式定语言.返回**认没认出来**.
///
/// 认了才算数(认不出来连"显式"都不算--否则一次写错的语言标签会把
/// [`ensure_default`] 挡在门外,于是界面变成一个键都找不到的 `zh-CN.cmd....`).
pub fn set_locale(locale: &str) -> bool {
    let Some(locale) = resolve(locale) else {
        return false;
    };
    LOCALE_SET.store(true, Ordering::Relaxed);
    rust_i18n::set_locale(locale);
    true
}

/// 当前语言.
pub fn locale() -> String {
    rust_i18n::locale().to_string()
}

/// 把一个语言标签规整成 [`KNOWN_LOCALES`] 里的一种;认不出来返回 `None`.
///
/// 规则:丢掉 `.` 之后的编码(`en_US.UTF-8` → `en_US`),`_` 换 `-`,`C`/`POSIX`
/// 这种不算语言,再按**语言前缀**挑--本版只有两种语言,所以 `zh*`(`zh-CN`,
/// `zh-TW`,`zh`)都当 `zh-CN`,`en*` 都当 `en`.
pub fn resolve(requested: &str) -> Option<&'static str> {
    let tag = requested.split('.').next().unwrap_or("").trim().replace('_', "-");
    if tag.is_empty() {
        return None;
    }
    if let Some(hit) = KNOWN_LOCALES
        .iter()
        .find(|known| known.eq_ignore_ascii_case(&tag))
    {
        return Some(hit);
    }

    match tag.split('-').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "zh" => Some("zh-CN"),
        "en" => Some("en"),
        _ => None,
    }
}

/// 查表并套用当前语言 -- **一律用它,不要直接用 `rust_i18n::t!`**.
///
/// 与 `t!` 的唯一差别是查表前先 [`ensure_default`](crate::i18n::ensure_default) (幂等,一次原子读).
#[macro_export]
macro_rules! tr {
    ($($tt:tt)*) => {{
        $crate::i18n::ensure_default();
        rust_i18n::t!($($tt)*)
    }};
}
