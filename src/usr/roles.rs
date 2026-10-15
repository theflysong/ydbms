//! 角色之间的成员关系:**谁借得到谁的权限**.
//!
//! `users.json` 里每个角色只记**直接**成员关系([`User::member_of`]);这里把
//! "直接"算成"间接可达到",并且**到处都防环**(图是用户自己改出来的,不能假设它无环).
//!
//! # 三条口径(与 PostgreSQL 对齐的那几条)
//!
//! * [`members_of`] **不看** `INHERIT`:它就是"图上的可达集合",`SET ROLE` 用它
//!   (借身份是显式动作,不看自动继承的开关);
//! * [`effective_roles`] **看** `INHERIT`,而且**逐边**看:一条从 `child` 到
//!   `parent` 的边,只有 `child` 自己 `INHERIT` 时才走下去--所以
//!   `NOINHERIT` 的角色不会因为"我是某个角色的成员"而自动拿到权限;
//! * **超级用户不在这个闭包里**:`is_admin` 只看当前角色自己(与 PostgreSQL 一致),
//!   调用方不要用 [`effective_roles`] 去判超级用户.
//!
//! 这几个都是**纯函数**(只读 [`UserStore`]),好单测,也让"谁能做什么"这句口径
//! 只有一处落点.

use std::collections::BTreeSet;

use super::UserStore;

/// `role` 直接或间接属于哪些角色(`role` 自己不算;**不看** `INHERIT`).
///
/// 找不到 `role` 就是空集;环由 `seen` 兜住(图是用户改出来的,别假设它无环).
pub fn members_of(store: &UserStore, role: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut pending: Vec<String> = match store.find(role) {
        Some(user) => user.member_of.clone(),
        None => return out,
    };

    while let Some(name) = pending.pop() {
        if name == role || !out.insert(name.clone()) {
            continue;
        }
        if let Some(user) = store.find(&name) {
            pending.extend(user.member_of.iter().cloned());
        }
    }
    out
}

/// 当前角色实际带着哪些身份:自己,加上(按 `INHERIT` **逐边**判断)借来的那些.
///
/// 权限检查用它:库 owner,表权限都在这个集合里找.
pub fn effective_roles(store: &UserStore, role: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.insert(role.to_string());

    let Some(user) = store.find(role) else {
        return out;
    };
    if !user.inherit {
        return out;
    }

    // 与 `members_of` 同一趟走法,只是每一跳都先问"走过去的那个角色允许自动继承吗"
    let mut pending: Vec<String> = user.member_of.clone();
    while let Some(name) = pending.pop() {
        if name == role || !out.insert(name.clone()) {
            continue;
        }
        if let Some(next) = store.find(&name) {
            if next.inherit {
                pending.extend(next.member_of.iter().cloned());
            }
        }
    }
    out
}

/// `SET ROLE` 能不能把身份借给 `target`(当前角色是 `current`).
///
/// 借身份不看 `INHERIT`:只要是成员就行(`SET ROLE` 是显式的).超级用户由调用方
/// 另外放行.
pub fn can_set_role(store: &UserStore, current: &str, target: &str) -> bool {
    current == target || members_of(store, current).contains(target)
}

/// 把 `member` 加进 `role` 的成员里(一条 `member → role` 的边),会不会成环?
///
/// 成环的意思:图上**已经有**一条 `role → ... → member` 的路,再加这条边就绕回来
/// 了(也就是 `member` 已经在 `role` 的可达集合里).`role == member` 也算.
pub fn would_cycle(store: &UserStore, role: &str, member: &str) -> bool {
    role == member || members_of(store, role).contains(member)
}

/// 直接属于 `role` 的那些角色(`\du` 用).
pub fn direct_members(store: &UserStore, role: &str) -> Vec<String> {
    let mut out: Vec<String> = store
        .users
        .iter()
        .filter(|user| user.member_of.iter().any(|held| held == role))
        .map(|user| user.name.clone())
        .collect();
    out.sort();
    out
}
