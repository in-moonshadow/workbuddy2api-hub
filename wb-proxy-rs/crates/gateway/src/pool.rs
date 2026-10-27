//! 账号池与调度（M3 Task 3），移植自 wb_accounts.py AccountPool.pick
//! （:2862-3152）、SessionAffinity（:2172-2256）与 wb_proxy.py 并发闸
//! （:273-302, 18349-18368）。范围：账号调度状态位、ready(model) 近似判定、
//! 五级选择管道（affinity→权重→临期积分[默认关]→剩余用量[默认关]→轮询）、
//! demote（只改会话亲和，不冷却）、并发闸（只增不缩）。
//!
//! **锁纪律（契约最易错点）**：Python 的 ready() 可能联网 refresh token，
//! 严禁在池锁内调用。本设计把账号数据与可变调度状态分离——选号时先锁内
//! 读候选/游标，锁外逐个 ready() 过滤，命中后再锁内推进游标/绑定。M5 接入
//! refresh 时无需改结构。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 与 Python `round()` 一致的银行家舍入（half-to-even）；f64::round 是
/// half-away-from-zero，行为不同（wb_accounts.py:2979-2993 `_expiry_weight`）。
pub fn py_round(x: f64) -> f64 {
    let floor = x.floor();
    let diff = x - floor;
    if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else if floor % 2.0 == 0.0 {
        floor
    } else {
        floor + 1.0
    }
}

/// code 6004「额度用满」的免费模型窗口禁用：`{模型名: {at: 首次撞时刻, reset: 重置时刻}}`。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCap {
    pub at: f64,
    pub reset: f64,
}

/// 账号调度状态位（wb_accounts.py:464-638 的调度相关子集）。
/// 所有 `*_until` 都是**绝对 epoch**（f64 秒），判定用 `until > now`。
#[derive(Debug, Clone)]
pub struct AccountState {
    pub uid: String,
    pub realm: String,
    /// 凭据档文件名（如 `abc123.json`）。Python `Account.credential_file`；
    /// 幂等字段回写（wb-storage `write_idempotent_fields`）需要它定位文件。
    /// 手工构造（测试注入）时可为空串，此时不可回写。
    pub credential_file: String,
    /// 企业空间 id（Python `Account.enterprise_id`，wb_accounts.py:500-508）：
    /// 取 `data.enterpriseId`，为空回退 JWT 的 `enterprise_id` claim。
    /// 非空即「企业账号」：签到/活跃上报被短路（契约 11/12/14）。
    pub enterprise_id: String,
    /// 面板开关（Python 原生假值语义：M3 强制 bool，M5 面板接线再对齐）。
    pub enabled: bool,
    /// 是否存在 access_token（空串视为无）。
    pub has_token: bool,
    /// 出站 Bearer token（M3 测试注入脱敏值；重试循环构造 Authorization 头用）。
    pub access_token: String,
    /// 手动权重，夹到 [1,100]，0/None → 1。
    pub weight: i64,
    /// 出站身分（endpoints::normalize_product 规范值；auto_switch 旋转）。
    pub product: String,
    /// 账号级软限流直到时刻。
    pub cooldown_until: f64,
    /// 402 余额停用直到下一个本地 04:00（**本地时区，非 UTC**）。
    pub balance_until: f64,
    /// 熔断直到时刻（5xx 连击）。
    pub breaker_until: f64,
    /// 降权直到时刻（瞬时异常连击）。
    pub degrade_until: f64,
    /// 凭证 JWT exp（epoch 秒）；None = 未知（不挡，对齐 Python `not exp` → True）。
    pub expires_at: Option<f64>,
    /// 429 连击计数（soft_streak）——note_* 用，M3 仅承载。
    pub soft_streak: i64,
    /// 硬失败计数——note_* 用，M3 仅承载。
    pub fails: i64,
    /// 瞬时异常计数——note_* 用，M3 仅承载。
    pub degrade_count: i64,
    /// 每模型冷却 `{模型名: 绝对 epoch}`，纯内存不持久化。
    pub model_cooldowns: HashMap<String, f64>,
    /// 临期积分窗口内最近到期天数。M3 由调用方注入（credits 包解析属 M5）；
    /// None = 不在窗口。
    pub expiring_days: Option<f64>,
    /// 无作用域 429 连击计数（note_unscoped_rate 用，M3 仅承载）。
    pub unscoped_streak: i64,
    /// 免费模型额度窗口 `{模型名: ModelCap}`（note_model_cap 写，M3 仅承载）。
    pub model_caps: HashMap<String, ModelCap>,
    /// 运行时「免费模型」目录（Python `Account.free_models`）。
    ///
    /// ⚠️ **不是** `note_model_cap` 的门控：那条用静态表
    /// `crate::cap::is_free_cap_model(realm, model)`（Python :953）。
    /// 本字段对应 Python 的 `model_is_free()`（每日额度守卫），由
    /// `apply_daily_credit_limit()` 从**模型目录**按 realm 填充 —— 与静态表
    /// 是两套数据源。M5 的每日额度守卫尚未落地 ⇒ 目前**无消费者**、恒空。
    pub free_models: HashSet<String>,
    /// 最近一次错误消息（note_* 写，日志/诊断用）。
    pub last_error: String,
    /// 最近一次错误详情（note_* 写，日志/诊断用）。
    pub last_error_detail: String,
    /// 账号的活动触发来源（scheduler/manual/account_add/account_import）。
    ///
    /// 对齐 Python `trigger`：活动历史写入时使用该值；拿不到时默认 `unknown`。
    pub trigger: String,
}

impl AccountState {
    pub fn new(uid: &str, realm: &str) -> Self {
        Self {
            uid: uid.into(),
            realm: realm.into(),
            credential_file: String::new(),
            enterprise_id: String::new(),
            enabled: true,
            has_token: true,
            access_token: String::new(),
            weight: 1,
            product: crate::endpoints::PRODUCT_DESKTOP.into(),
            cooldown_until: 0.0,
            balance_until: 0.0,
            breaker_until: 0.0,
            degrade_until: 0.0,
            expires_at: None,
            soft_streak: 0,
            fails: 0,
            degrade_count: 0,
            model_cooldowns: HashMap::new(),
            expiring_days: None,
            unscoped_streak: 0,
            model_caps: HashMap::new(),
            free_models: HashSet::new(),
            last_error: String::new(),
            last_error_detail: String::new(),
            trigger: "unknown".to_string(),
        }
    }

    /// throttle_wait（wb_accounts.py:2125-2135）：各 until 距 now 的剩余等待
    /// 取最大；有 model 再叠模型冷却。全未冷却 → 0。
    pub fn throttle_wait(&self, model: Option<&str>, now: f64) -> f64 {
        let mut wait = 0.0_f64.max(self.cooldown_until - now);
        wait = wait
            .max(self.balance_until - now)
            .max(self.breaker_until - now)
            .max(self.degrade_until - now);
        if let Some(m) = model {
            if let Some(&until) = self.model_cooldowns.get(m) {
                wait = wait.max(until - now);
            }
        }
        wait.max(0.0)
    }

    /// set_product（wb_accounts.py:1200-1214）：切换出站身分。normalize 后与
    /// 现值相同 → False；不同 → 改字段 + clear_error(keep_caps=True)（保留仍
    /// 在恢复期的 6004 cap，跨身分成立，真机实测不能顺手清掉）。M3 纯内存，
    /// 凭证档持久化属 M5。
    pub fn set_product(&mut self, value: &str, now: f64) -> bool {
        let new = crate::endpoints::normalize_product(value);
        if new == self.product {
            return false;
        }
        self.product = new.into();
        crate::notes::clear_error(self, None, true, now);
        true
    }

    /// ready(model) 近似判定（wb_accounts.py:1079-1112，第一步子集）：
    /// 短路链 = enabled+token → throttle_wait → 凭证时效。reserve/daily/credit
    /// 闸门与 refresh 依赖 settings/credits 解析（M5），此处略。
    /// **纯内存无副作用**——M5 接 refresh 后调用方需保证锁外调用（见模块头）。
    pub fn ready(&self, model: Option<&str>, now: f64) -> bool {
        if !self.enabled || !self.has_token {
            return false;
        }
        if self.throttle_wait(model, now) > 0.0 {
            return false;
        }
        // exp > now+120 才算可用；不足/已过期视为不可用（M5 改调 refresh 定论）
        if let Some(exp) = self.expires_at {
            if exp - now <= 120.0 {
                return false;
            }
        }
        true
    }

    /// 临期窗口判定（wb_accounts.py:854-868 的第一步子集）：
    /// window<=0 关；soonest 有值且 <= window。
    pub fn in_expiring_window(&self, window_days: f64) -> bool {
        if window_days <= 0.0 {
            return false;
        }
        match self.expiring_days {
            Some(days) => days <= window_days,
            None => false,
        }
    }

    /// _expiry_weight（wb_accounts.py:2979-2993）：max(1, round(window-days)+1)；
    /// days 缺失 → 1。round 用银行家舍入。
    pub fn expiry_weight(&self, window_days: f64) -> f64 {
        match self.expiring_days {
            Some(days) => (py_round(window_days - days) + 1.0).max(1.0),
            None => 1.0,
        }
    }

    /// unavailable_reason（wb_accounts.py:1042-1077）：顺序与 ready 一致但不发
    /// refresh。惩罚取最大值，平手取列表顺序（熔断>降权>余额保护>软限流>模型）。
    pub fn unavailable_reason(&self, model: Option<&str>, now: f64) -> Option<String> {
        if !self.enabled {
            return Some("已停用".into());
        }
        if !self.has_token {
            return Some("无凭证".into());
        }
        let mut penalties: Vec<(&str, f64)> = vec![
            ("熔断", self.breaker_until),
            ("降权", self.degrade_until),
            ("余额保护", self.balance_until),
            ("软限流冷却", self.cooldown_until),
        ];
        if let Some(m) = model {
            if let Some(&until) = self.model_cooldowns.get(m) {
                penalties.push(("模型 冷却", until));
            }
        }
        // Python max 平手取第一个：遍历取严格更大者，保留同值前者
        let mut best: Option<(&str, f64)> = None;
        for p in penalties {
            if best.map(|(_, b)| p.1 > b).unwrap_or(true) {
                best = Some(p);
            }
        }
        if let Some((label, until)) = best {
            if until > now {
                let secs = (until - now).round() as i64;
                return Some(format!("{label} 剩余 {secs}s"));
            }
        }
        if let Some(exp) = self.expires_at {
            if exp - now <= 120.0 {
                return Some("凭证已过期".into());
            }
        }
        None
    }
}

/// 账号目录里被当配置文件跳过的名字（与 wb-storage 的 NON_ACCOUNT_FILES
/// 保持同步；storage 是 api 层的兄弟 crate，gateway 不能反向依赖它）。
const NON_ACCOUNT_FILES: [&str; 3] = ["settings.json", "model.json", "active_realm.json"];

/// 从账号目录装载池（对齐 Python `AccountPool.load()` :2417-2466 的逐字段
/// 映射）。只读、不落盘：跳过非账号 JSON、读不到或 uid 为空的行跳过，
/// 其余按文件名排序进入 Vec（对齐 Python `sorted(os.listdir)`）。
///
/// 字段映射（wb_accounts.py:465-538 `Account.__init__` 的调度相关子集）：
/// - uid：`data.uid`，为空回退 `jwt_uid(token)`，仍为空则整行丢弃
/// - realm：`data.realm`，为空回退 `detect_realm_from_token(token, domain)`
/// - domain：`data.domain`，为空回退区域规范域名
/// - product：`normalize_product(data.product)`（未知回退 workbuddy）
/// - expires_at：`normalize_epoch(data.expiresAt)`，为空回退 `jwt_exp(token)`
/// - enabled / weight：面板持久化字段原样读入（夹到 [1,100]）
/// - credential_file：凭据档文件名（幂等字段回写定位用）
/// - enterprise_id：`data.enterpriseId`，为空回退 JWT `enterprise_id` claim
///
/// 运行期状态（cooldown/breaker/soft_streak 等）一律按 `AccountState::new()`
/// 的默认值起算——Python 的 last_error/model_cooldowns 也是 RUNTIME_ONLY，
/// 不随凭据档恢复（wb_accounts.py:518-536, 2417-2466）。
pub fn load_account_pool(accounts_dir: &std::path::Path) -> Vec<AccountState> {
    let mut accounts = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(accounts_dir) else {
        return accounts;
    };
    let mut files: Vec<std::path::PathBuf> = read_dir
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();
    for path in files {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if NON_ACCOUNT_FILES.contains(&name) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if !doc.is_object() {
            continue;
        }
        let token = doc
            .get("accessToken")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let uid_raw = doc.get("uid").and_then(|v| v.as_str()).unwrap_or("");
        let uid = if uid_raw.is_empty() {
            wb_identity::jwt::jwt_uid(token)
        } else {
            uid_raw.to_string()
        };
        if uid.is_empty() {
            continue;
        }
        let domain_raw = doc.get("domain").and_then(|v| v.as_str()).unwrap_or("");
        let realm = doc
            .get("realm")
            .and_then(|v| v.as_str())
            .filter(|r| !r.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                wb_identity::realm::detect_realm_from_token(token, Some(domain_raw)).to_string()
            });
        // 注：Account 的 domain 字段在 Rust 侧不驻留池——出站头由
        // headers::chat_request_headers(realm, product) 经 domain_for_realm 现算。
        let product = crate::endpoints::normalize_product(
            doc.get("product").and_then(|v| v.as_str()).unwrap_or(""),
        )
        .to_string();
        let expires_at = doc
            .get("expiresAt")
            .and_then(|v| v.as_f64())
            .map(wb_identity::jwt::normalize_epoch)
            .filter(|e| *e > 0)
            .or_else(|| {
                let exp = wb_identity::jwt::jwt_exp(token);
                (exp > 0).then_some(exp)
            })
            .map(|e| e as f64);
        let enabled = doc.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        let weight = doc
            .get("weight")
            .and_then(|v| v.as_i64())
            .unwrap_or(1)
            .clamp(1, 100);
        // 企业 id（wb_accounts.py:500-508）：凭据档里的 enterpriseId 常常为空——
        // 企业账号是登录时切空间换来的 token，id 只写在 token 里。不回填则
        // is_enterprise() 判假，企业账号会被误送去签到/活跃上报并静默失败。
        let enterprise_id = match doc
            .get("enterpriseId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
        {
            "" => wb_identity::jwt::jwt_claims(token)
                .get("enterprise_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            value => value.to_string(),
        };
        accounts.push(AccountState {
            uid: uid.clone(),
            realm: realm.clone(),
            credential_file: name.to_string(),
            enterprise_id,
            enabled,
            has_token: !token.is_empty(),
            access_token: token.to_string(),
            weight,
            product,
            expires_at,
            ..AccountState::new(&uid, &realm)
        });
    }
    accounts
}

/// 会话亲和（wb_accounts.py:2172-2256）：`bindings[session_key]` 是
/// `[(uid, 绝对epoch过期), ...]`，队首最新。ttl=7200s、history=3、max=5000。
///
/// ⚠️ 必须用 `new()`/`Default` 委托——`derive(Default)` 会让 ttl/history/
/// max_entries 全 0，导致每 bind 即 purge + truncate(0) 清空。
#[derive(Debug)]
pub struct SessionAffinity {
    bindings: HashMap<String, Vec<(String, f64)>>,
    ttl: f64,
    history: usize,
    max_entries: usize,
}

impl Default for SessionAffinity {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionAffinity {
    pub fn new() -> Self {
        Self {
            bindings: HashMap::new(),
            ttl: 7200.0,
            history: 3,
            max_entries: 5000,
        }
    }

    fn purge(&mut self, now: f64) {
        self.bindings.retain(|_, v| {
            v.retain(|(_, until)| *until > now);
            !v.is_empty()
        });
    }

    /// bind：去重插头、截断 history；达到 max_entries 先全表清理再插。
    pub fn bind(&mut self, key: &str, uid: &str, now: f64) {
        if self.bindings.len() >= self.max_entries {
            self.purge(now);
        }
        let until = now + self.ttl;
        let entry = self.bindings.entry(key.into()).or_default();
        entry.retain(|(u, _)| u != uid);
        entry.insert(0, (uid.into(), until));
        entry.truncate(self.history);
    }

    /// recent：返回未过期 uid（队首最新）；就地淘汰过期项。
    pub fn recent(&mut self, key: &str, now: f64) -> Vec<String> {
        let Some(entry) = self.bindings.get_mut(key) else {
            return Vec::new();
        };
        entry.retain(|(_, until)| *until > now);
        if entry.is_empty() {
            self.bindings.remove(key);
            return Vec::new();
        }
        entry.iter().map(|(u, _)| u.clone()).collect()
    }

    /// demote（wb_accounts.py:2232-2252）：uid 为 None 删队首；否则删该 uid
    /// 全部条目。**只改绑定列表，不给账号加冷却**。列表清空则整键移除。
    pub fn demote(&mut self, key: &str, uid: Option<&str>) {
        let Some(entry) = self.bindings.get_mut(key) else {
            return;
        };
        match uid {
            None => {
                if !entry.is_empty() {
                    entry.remove(0);
                }
            }
            Some(u) => {
                entry.retain(|(existing, _)| existing != u);
            }
        }
        if entry.is_empty() {
            self.bindings.remove(key);
        }
    }
}

/// 池选择配置（对应 settings：remaining_priority_enabled 默认关、
/// expiring_window_days 默认 7；wb_settings.py:102, 1152-1163, 575-585）。
#[derive(Debug, Clone)]
pub struct PoolOptions {
    pub remaining_priority_enabled: bool,
    pub expiring_window_days: f64,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            remaining_priority_enabled: false,
            expiring_window_days: 7.0,
        }
    }
}

/// 可变调度状态（游标/权重表/亲和），Mutex 短临界区保护。
#[derive(Debug, Default)]
struct PickerState {
    /// 全局轮询游标（只前进）。
    cursor: usize,
    /// 页级轮询游标 `{(页号,页大小): 游标}`。
    page_cursors: HashMap<(usize, usize), usize>,
    /// L2 手动权重 WRR 状态 `{uid: 累计}`。
    manual_weights: HashMap<String, f64>,
    /// L3 临期 WRR 状态。
    expiry_weights: HashMap<String, f64>,
    /// L4 剩余用量 WRR 状态 `{(uid, model): 累计}`。
    remaining_pick_state: HashMap<(String, String), f64>,
    affinity: SessionAffinity,
}

/// 账号池：账号数据与调度状态分离（锁纪律见模块头）。
#[derive(Debug)]
pub struct AccountPool {
    accounts: Vec<AccountState>,
    state: Mutex<PickerState>,
    opts: PoolOptions,
}

impl AccountPool {
    pub fn new(accounts: Vec<AccountState>, opts: PoolOptions) -> Self {
        Self {
            accounts,
            state: Mutex::new(PickerState::default()),
            opts,
        }
    }

    pub fn account(&self, index: usize) -> Option<&AccountState> {
        self.accounts.get(index)
    }

    /// 可变账号访问（retry 循环记账用）。锁纪律：accounts 不可变表，
    /// note_* 修改的是 AccountState 自身（内部无锁，M3 单线程路径 OK）。
    pub fn account_mut(&mut self, index: usize) -> Option<&mut AccountState> {
        self.accounts.get_mut(index)
    }

    /// account_by_uid（Python `AccountPool.get` :2659-2663）：跨区域首个 uid
    /// 命中。
    pub fn account_by_uid(&self, uid: &str) -> Option<&AccountState> {
        self.accounts.iter().find(|a| a.uid == uid)
    }

    /// account_by_uid_mut：同上，可变版本（路由要改 AccountState 时用）。
    pub fn account_by_uid_mut(&mut self, uid: &str) -> Option<&mut AccountState> {
        self.accounts.iter_mut().find(|a| a.uid == uid)
    }

    /// set_account_enabled（Python `set_enabled` :2774-2782）：置 enabled；
    /// 重新启用时顺带 clear_error（Python `account.clear_error()` 默认
    /// keep_caps=False）。返回账号是否找到并更新。**不落盘**。
    pub fn set_account_enabled(&mut self, uid: &str, enabled: bool) -> bool {
        let Some(account) = self.accounts.iter_mut().find(|a| a.uid == uid) else {
            return false;
        };
        account.enabled = enabled;
        if enabled {
            crate::notes::clear_error(account, None, false, 0.0);
        }
        true
    }

    /// set_account_weight（Python `set_weight` :2784-2793）：手动权重夹到
    /// [1,100]（0/负数 → 1）。返回账号是否找到并更新。**不落盘**。
    pub fn set_account_weight(&mut self, uid: &str, weight: i64) -> bool {
        let Some(account) = self.accounts.iter_mut().find(|a| a.uid == uid) else {
            return false;
        };
        account.weight = weight.clamp(1, 100);
        true
    }

    /// account_product（Python `_route_accounts_product` :17144-17190 的逐账号
    /// `account.set_product(target)`）：返回本次是否真的切换（False = 同值/无号）。
    /// **不落盘**——面板操作要立即存档由调用方决定。
    pub fn account_product(&mut self, uid: &str, product: &str, now: f64) -> bool {
        match self.accounts.iter_mut().find(|a| a.uid == uid) {
            Some(account) => account.set_product(product, now),
            None => false,
        }
    }

    /// remove_account（Python `AccountPool.remove` :2690-2696 的记忆体半）：从
    /// 池中移除账号并返回。**不删凭据文件**——真实删除由调用方（api/tasks 层）
    /// 在用户批准后执行，本层只动内存。
    pub fn remove_account(&mut self, uid: &str) -> Option<AccountState> {
        let index = self.accounts.iter().position(|a| a.uid == uid)?;
        Some(self.accounts.remove(index))
    }

    /// upsert_account（Python `AccountPool.add` :2665-2688 的记忆体半）：uid 已
    /// 存在 → 原地替换；否则追加。返回 `true` = 更新了既有账号，`false` = 新增。
    /// **不落盘**——写盘由调用方（import 路径）负责。
    pub fn upsert_account(&mut self, state: AccountState) -> bool {
        match self.accounts.iter().position(|a| a.uid == state.uid) {
            Some(index) => {
                self.accounts[index] = state;
                true
            }
            None => {
                self.accounts.push(state);
                false
            }
        }
    }

    /// 池级 demote（retry 循环降级热号用）：转发到 affinity.demote。
    pub fn demote(&mut self, session_key: &str, uid: Option<&str>) {
        self.state.lock().unwrap().affinity.demote(session_key, uid);
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// 只读遍历（retry 兜底统计用）。
    pub fn iter_accounts(&self) -> std::slice::Iter<'_, AccountState> {
        self.accounts.iter()
    }

    fn find_ready(&self, uid: &str, realm: &str, model: Option<&str>, now: f64) -> Option<usize> {
        self.accounts
            .iter()
            .position(|a| a.uid == uid && a.realm == realm && a.ready(model, now))
    }

    /// pick_for_session（wb_accounts.py:2862-2899）：L1 亲和热号 → 普通五级；
    /// 命中后绑定 affinity。page 非 None 时不做亲和（页内批选场景）。
    pub fn pick_for_session(
        &self,
        realm: &str,
        session_key: Option<&str>,
        exclude: Option<&HashSet<String>>,
        model: Option<&str>,
        page: Option<(usize, usize)>,
        now: f64,
    ) -> Option<usize> {
        let exclude = exclude.cloned().unwrap_or_default();
        if let Some(sk) = session_key {
            if page.is_none() {
                let recent = self.state.lock().unwrap().affinity.recent(sk, now);
                for uid in recent {
                    if exclude.contains(&uid) {
                        continue;
                    }
                    if let Some(idx) = self.find_ready(&uid, realm, model, now) {
                        self.state.lock().unwrap().affinity.bind(sk, &uid, now);
                        return Some(idx);
                    }
                }
            }
        }
        let picked = self.pick(realm, &exclude, model, page, now);
        if let Some(idx) = picked {
            if let Some(sk) = session_key {
                if page.is_none() {
                    let uid = self.accounts[idx].uid.clone();
                    self.state.lock().unwrap().affinity.bind(sk, &uid, now);
                }
            }
        }
        picked
    }

    /// 五级选择（wb_accounts.py:2901-2934）：任一命中即返回。
    fn pick(
        &self,
        realm: &str,
        exclude: &HashSet<String>,
        model: Option<&str>,
        page: Option<(usize, usize)>,
        now: f64,
    ) -> Option<usize> {
        let group: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.realm == realm)
            .map(|(i, _)| i)
            .collect();
        if group.is_empty() {
            return None;
        }
        let group = match page {
            Some(p) => self.page_slice(&group, p),
            None => group,
        };
        if group.is_empty() {
            return None;
        }
        // L2 手动权重>1
        if let Some(i) = self.pick_weighted_first(&group, exclude, model, now) {
            return Some(i);
        }
        // L3 临期积分优先（默认关：window<=0 不进）
        if self.opts.expiring_window_days > 0.0 {
            if let Some(i) = self.pick_expiring_first(&group, exclude, model, now) {
                return Some(i);
            }
        }
        // L4 剩余用量优先（默认关）
        if self.opts.remaining_priority_enabled {
            if let Some(i) = self.pick_remaining_first(&group, exclude, model, now) {
                return Some(i);
            }
        }
        // L5 全局轮询
        self.pick_rotate(&group, exclude, model, page, now)
    }

    /// 平滑 WRR 公共内核（wb_accounts.py:3033-3080，nginx）：state 先按参与
    /// keys 剪枝；每项累加权重；赢家付**当轮总权重和**。等权 → 轮转。
    /// `entries` 与 `state` 由调用方绑定。
    fn smooth_pick(
        state: &mut HashMap<(String, String), f64>,
        keys: &HashSet<(String, String)>,
        entries: &[(usize, String, f64)],
        model: &str,
    ) -> Option<usize> {
        state.retain(|k, _| keys.contains(k));
        let mut total = 0.0_f64;
        let mut chosen: Option<(usize, String, f64)> = None;
        for (idx, uid, w) in entries {
            let key = (uid.clone(), model.to_string());
            let cur = state.get(&key).copied().unwrap_or(0.0) + w;
            state.insert(key, cur);
            total += w;
            if chosen.as_ref().map(|(_, _, cw)| cur > *cw).unwrap_or(true) {
                chosen = Some((*idx, uid.clone(), cur));
            }
        }
        if let Some((idx, uid, cur)) = chosen {
            state.insert((uid, model.to_string()), cur - total);
            Some(idx)
        } else {
            None
        }
    }

    fn pick_weighted_first(
        &self,
        group: &[usize],
        exclude: &HashSet<String>,
        model: Option<&str>,
        now: f64,
    ) -> Option<usize> {
        // entries: (account_index, uid, weight)
        let entries: Vec<(usize, String, f64)> = group
            .iter()
            .filter_map(|&i| {
                let a = &self.accounts[i];
                if a.weight > 1 && !exclude.contains(&a.uid) && a.ready(model, now) {
                    Some((i, a.uid.clone(), a.weight as f64))
                } else {
                    None
                }
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        let uid_keys: HashSet<String> = entries.iter().map(|(_, uid, _)| uid.clone()).collect();
        let mut st = self.state.lock().unwrap();
        // 手动权重按单键 (uid) 存（模型无关）
        st.manual_weights.retain(|k, _| uid_keys.contains(k));
        let mut total = 0.0_f64;
        let mut chosen: Option<(usize, String, f64)> = None;
        for (idx, uid, w) in &entries {
            let cur = st.manual_weights.get(uid).copied().unwrap_or(0.0) + w;
            st.manual_weights.insert(uid.clone(), cur);
            total += w;
            if chosen.as_ref().map(|(_, _, cw)| cur > *cw).unwrap_or(true) {
                chosen = Some((*idx, uid.clone(), cur));
            }
        }
        if let Some((idx, uid, cur)) = chosen {
            st.manual_weights.insert(uid, cur - total);
            Some(idx)
        } else {
            None
        }
    }

    fn pick_expiring_first(
        &self,
        group: &[usize],
        exclude: &HashSet<String>,
        model: Option<&str>,
        now: f64,
    ) -> Option<usize> {
        let window = self.opts.expiring_window_days;
        let entries: Vec<(usize, String, f64)> = group
            .iter()
            .filter_map(|&i| {
                let a = &self.accounts[i];
                if a.in_expiring_window(window) && !exclude.contains(&a.uid) && a.ready(model, now)
                {
                    Some((i, a.uid.clone(), a.expiry_weight(window)))
                } else {
                    None
                }
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        let uid_keys: HashSet<String> = entries.iter().map(|(_, uid, _)| uid.clone()).collect();
        let mut st = self.state.lock().unwrap();
        st.expiry_weights.retain(|k, _| uid_keys.contains(k));
        let mut total = 0.0_f64;
        let mut chosen: Option<(usize, String, f64)> = None;
        for (idx, uid, w) in &entries {
            let cur = st.expiry_weights.get(uid).copied().unwrap_or(0.0) + w;
            st.expiry_weights.insert(uid.clone(), cur);
            total += w;
            if chosen.as_ref().map(|(_, _, cw)| cur > *cw).unwrap_or(true) {
                chosen = Some((*idx, uid.clone(), cur));
            }
        }
        if let Some((idx, uid, cur)) = chosen {
            st.expiry_weights.insert(uid, cur - total);
            Some(idx)
        } else {
            None
        }
    }

    fn pick_remaining_first(
        &self,
        group: &[usize],
        exclude: &HashSet<String>,
        model: Option<&str>,
        now: f64,
    ) -> Option<usize> {
        let m = model?;
        {
            let st = self.state.lock().unwrap();
            if st.remaining_pick_state.is_empty() {
                return None;
            }
        }
        let entries: Vec<(usize, String, f64)> = group
            .iter()
            .filter_map(|&i| {
                let a = &self.accounts[i];
                if exclude.contains(&a.uid) || !a.ready(Some(m), now) {
                    return None;
                }
                let st = self.state.lock().unwrap();
                if let Some(&w) = st.remaining_pick_state.get(&(a.uid.clone(), m.to_string())) {
                    if w > 1.0 {
                        return Some((i, a.uid.clone(), w));
                    }
                }
                None
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        let keys: HashSet<(String, String)> = entries
            .iter()
            .map(|(_, uid, _)| (uid.clone(), m.to_string()))
            .collect();
        let mut st = self.state.lock().unwrap();
        Self::smooth_pick(&mut st.remaining_pick_state, &keys, &entries, m)
    }

    /// L5 轮询（wb_accounts.py:3127-3152）：start 锁内读，ready 锁外调；
    /// **游标只在选中时推进**（全跳过不动）；页游标与全局互不干扰。
    /// group 元素是 account 原始索引，pos 是 group 内位置（勿混用）。
    fn pick_rotate(
        &self,
        group: &[usize],
        exclude: &HashSet<String>,
        model: Option<&str>,
        page: Option<(usize, usize)>,
        now: f64,
    ) -> Option<usize> {
        let total = group.len();
        if total == 0 {
            return None;
        }
        let start = {
            let st = self.state.lock().unwrap();
            match page {
                Some(p) => st.page_cursors.get(&p).copied().unwrap_or(0),
                None => st.cursor,
            }
        };
        for offset in 0..total {
            let pos = (start + offset) % total;
            let account_index = group[pos];
            let a = &self.accounts[account_index];
            if exclude.contains(&a.uid) {
                continue;
            }
            if a.ready(model, now) {
                let mut st = self.state.lock().unwrap();
                let next = (pos + 1) % total;
                match page {
                    Some(p) => {
                        st.page_cursors.insert(p, next);
                    }
                    None => {
                        st.cursor = next;
                    }
                }
                return Some(account_index);
            }
        }
        None
    }

    /// _page_slice（wb_accounts.py:3082-3110）：size<=1 或 >=len → 原组；
    /// 否则按 **sort(uid) rank** 取 `rank % size == index % size`。
    fn page_slice(&self, group: &[usize], page: (usize, usize)) -> Vec<usize> {
        let (page_index, size) = page;
        if size <= 1 || size >= group.len() {
            return group.to_vec();
        }
        // 组内 uid 升序排名
        let mut by_uid: Vec<(usize, &String)> =
            group.iter().map(|&i| (i, &self.accounts[i].uid)).collect();
        by_uid.sort_by(|a, b| a.1.cmp(b.1));
        let rank: HashMap<usize, usize> = by_uid
            .iter()
            .enumerate()
            .map(|(r, &(i, _))| (i, r))
            .collect();
        group
            .iter()
            .filter(|&&i| rank[&i] % size == page_index % size)
            .copied()
            .collect()
    }

    /// apply_remaining_weights 入口（wb_proxy.py:2944-2966 的 Rust 侧）：
    /// 每请求注入 (uid, model) → weight>1 的剩余用量权重表（Task 4 计算）。
    pub fn set_remaining_weights(&self, weights: &[(&str, &str, f64)]) {
        let mut st = self.state.lock().unwrap();
        st.remaining_pick_state.clear();
        for (uid, model, w) in weights {
            st.remaining_pick_state
                .insert(((*uid).into(), (*model).into()), *w);
        }
    }

    /// count_ready（wb_accounts.py:2857-2860）：realm 过滤后 ready 计数。
    pub fn count_ready(&self, realm: Option<&str>, model: Option<&str>, now: f64) -> usize {
        self.accounts
            .iter()
            .filter(|a| realm.map(|r| a.realm == r).unwrap_or(true) && a.ready(model, now))
            .count()
    }

    /// pool_unavailable_detail（wb_proxy.py:12794-12820）：无账号 → 区域无号；
    /// 否则逐账号 reason 中文逗号拼接，前缀 `；各账号：`。
    pub fn unavailable_detail(&self, realm: &str, model: Option<&str>, now: f64) -> String {
        if self.accounts.iter().all(|a| a.realm != realm) {
            return "；该区域没有任何账号".into();
        }
        let mut parts: Vec<String> = Vec::new();
        for a in &self.accounts {
            if a.realm != realm {
                continue;
            }
            if let Some(reason) = a.unavailable_reason(model, now) {
                let short: String = a.uid.chars().take(8).collect();
                parts.push(format!("「{short}」 {reason}"));
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("；各账号：{}", parts.join("，"))
        }
    }
}

/// 并发闸（wb_proxy.py:273-302, 18349-18368）：`wanted = max(floor, ready)`，
/// **只增不缩**（缩窗会让 in-flight 超新上限）。超时 → 503 overloaded。
pub struct ConcurrencyGate {
    inner: Mutex<GateInner>,
    auto: bool,
    floor: usize,
    wait: Duration,
}

struct GateInner {
    cap: usize,
    sem: Arc<Semaphore>,
}

impl ConcurrencyGate {
    /// max 为初始容量；auto=true 时 resize 生效（默认 32、FLOOR=32）。
    pub fn new(max: usize, auto: bool, floor: usize, wait_seconds: f64) -> Self {
        Self {
            inner: Mutex::new(GateInner {
                cap: max,
                sem: Arc::new(Semaphore::new(max)),
            }),
            auto,
            floor,
            wait: Duration::from_secs_f64(wait_seconds),
        }
    }

    /// 仅 auto 模式且 wanted > 当前容量时重建（只增不缩）。
    pub fn resize(&self, ready_count: usize) {
        if !self.auto {
            return;
        }
        let wanted = self.floor.max(ready_count);
        let mut inner = self.inner.lock().unwrap();
        if wanted <= inner.cap {
            return;
        }
        inner.cap = wanted;
        inner.sem = Arc::new(Semaphore::new(wanted));
    }

    /// 获取槽位：wait 超时 → Err(Overloaded{in_flight})。释放由 Drop 保证。
    pub async fn acquire(&self) -> Result<PermitGuard, Overloaded> {
        let sem = self.inner.lock().unwrap().sem.clone();
        match tokio::time::timeout(self.wait, sem.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(PermitGuard { _permit: permit }),
            Ok(Err(_)) => Err(Overloaded {
                in_flight: self.in_flight(),
            }),
            Err(_) => Err(Overloaded {
                in_flight: self.in_flight(),
            }),
        }
    }

    fn in_flight(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.cap.saturating_sub(inner.sem.available_permits())
    }
}

#[derive(Debug)]
pub struct Overloaded {
    pub in_flight: usize,
}

pub struct PermitGuard {
    _permit: OwnedSemaphorePermit,
}

impl std::fmt::Debug for PermitGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermitGuard").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const NOW: f64 = 1_000.0;

    fn intl(n: u32) -> AccountState {
        AccountState::new(&format!("u{n}"), "intl")
    }

    fn exclude(ids: &[&str]) -> Option<HashSet<String>> {
        Some(ids.iter().map(|s| s.to_string()).collect())
    }

    fn pick_uid(pool: &AccountPool, model: Option<&str>) -> String {
        let idx = pool
            .pick_for_session("intl", None, None, model, None, NOW)
            .expect("expected a pick");
        pool.account(idx).unwrap().uid.clone()
    }

    #[test]
    fn py_round_is_bankers() {
        assert_eq!(py_round(0.5), 0.0);
        assert_eq!(py_round(1.5), 2.0);
        assert_eq!(py_round(2.5), 2.0);
        assert_eq!(py_round(0.4), 0.0);
        assert_eq!(py_round(3.5), 4.0);
    }

    #[test]
    fn rotate_picks_in_round_robin_order() {
        let pool = AccountPool::new(vec![intl(1), intl(2), intl(3)], PoolOptions::default());
        assert_eq!(pick_uid(&pool, None), "u1");
        assert_eq!(pick_uid(&pool, None), "u2");
        assert_eq!(pick_uid(&pool, None), "u3");
        assert_eq!(pick_uid(&pool, None), "u1");
    }

    #[test]
    fn exclude_skips_account() {
        let pool = AccountPool::new(vec![intl(1), intl(2), intl(3)], PoolOptions::default());
        let idx = pool
            .pick_for_session("intl", None, exclude(&["u2"]).as_ref(), None, None, NOW)
            .unwrap();
        assert_eq!(pool.account(idx).unwrap().uid, "u1");
        // 游标推进到 1 → u2 被排除 → u3
        let idx = pool
            .pick_for_session("intl", None, exclude(&["u2"]).as_ref(), None, None, NOW)
            .unwrap();
        assert_eq!(pool.account(idx).unwrap().uid, "u3");
    }

    #[test]
    fn cooling_account_skipped() {
        let mut accounts = vec![intl(1), intl(2), intl(3)];
        accounts[1].cooldown_until = NOW + 100.0;
        let pool = AccountPool::new(accounts, PoolOptions::default());
        assert_eq!(pick_uid(&pool, None), "u1");
        // 游标指向 u2（冷却）→ 跳过 → u3
        assert_eq!(pick_uid(&pool, None), "u3");
        assert_eq!(pick_uid(&pool, None), "u1");
    }

    #[test]
    fn model_cooldown_only_blocks_that_model() {
        let mut a1 = intl(1);
        a1.model_cooldowns.insert("m".into(), NOW + 500.0);
        let a2 = intl(2);
        let pool = AccountPool::new(vec![a1, a2], PoolOptions::default());
        // m 冷却 → a1 不可用 → 选中 a2
        assert_eq!(pick_uid(&pool, Some("m")), "u2");
        // 其他模型不受影响
        assert_eq!(pick_uid(&pool, Some("other")), "u1");
    }

    #[test]
    fn weighted_account_picked_more_often() {
        let mut accounts = vec![intl(1), intl(2)];
        accounts[1].weight = 5;
        let pool = AccountPool::new(accounts, PoolOptions::default());
        let mut u1 = 0;
        let mut u2 = 0;
        for _ in 0..6 {
            match pick_uid(&pool, None).as_str() {
                "u1" => u1 += 1,
                "u2" => u2 += 1,
                _ => panic!("unexpected uid"),
            }
        }
        assert!(u2 > u1, "weight=5 应被选更多：u1={u1} u2={u2}");
    }

    #[test]
    fn session_affinity_reuses_hot_account() {
        let pool = AccountPool::new(vec![intl(1), intl(2)], PoolOptions::default());
        // 第一次选 u1 并绑定
        let idx = pool
            .pick_for_session("intl", Some("sk"), None, None, None, NOW)
            .unwrap();
        assert_eq!(pool.account(idx).unwrap().uid, "u1");
        // 再次同会话：即使游标已推进也走亲和
        for _ in 0..3 {
            let idx = pool
                .pick_for_session("intl", Some("sk"), None, None, None, NOW)
                .unwrap();
            assert_eq!(pool.account(idx).unwrap().uid, "u1");
        }
    }

    #[test]
    fn demote_removes_only_from_affinity_not_cooldown() {
        let mut accounts = vec![intl(1), intl(2)];
        accounts[0].cooldown_until = 0.0; // 明确无冷却
        let pool = AccountPool::new(accounts, PoolOptions::default());
        let idx = pool
            .pick_for_session("intl", Some("sk"), None, None, None, NOW)
            .unwrap();
        assert_eq!(pool.account(idx).unwrap().uid, "u1");
        let uid = pool.account(idx).unwrap().uid.clone();
        pool.state.lock().unwrap().affinity.demote("sk", Some(&uid));
        // demote 后不再热号 → L5 轮询到 u2
        let idx = pool
            .pick_for_session("intl", Some("sk"), None, None, None, NOW)
            .unwrap();
        assert_eq!(pool.account(idx).unwrap().uid, "u2");
        // demote 未加冷却：u1 仍 ready
        assert!(pool.account(0).unwrap().ready(None, NOW));
    }

    #[test]
    fn expiring_account_wins_when_window_on() {
        let mut accounts = vec![intl(1), intl(2)];
        accounts[1].expiring_days = Some(1.0);
        let pool = AccountPool::new(accounts, PoolOptions::default());
        // 默认 window=7 开启 L3 → 临期 u2 先选
        assert_eq!(pick_uid(&pool, None), "u2");
        assert_eq!(pick_uid(&pool, None), "u2");
    }

    #[test]
    fn remaining_weights_prioritize_when_enabled() {
        let pool = AccountPool::new(
            vec![intl(1), intl(2)],
            PoolOptions {
                remaining_priority_enabled: true,
                ..PoolOptions::default()
            },
        );
        pool.set_remaining_weights(&[("u2", "m", 3.0)]);
        assert_eq!(pick_uid(&pool, Some("m")), "u2");
    }

    #[test]
    fn remaining_weights_ignored_when_disabled() {
        let pool = AccountPool::new(vec![intl(1), intl(2)], PoolOptions::default()); // L4 默认关
        pool.set_remaining_weights(&[("u2", "m", 3.0)]);
        assert_eq!(pick_uid(&pool, Some("m")), "u1");
    }

    #[test]
    fn realm_is_isolated() {
        let mut accounts = vec![intl(1), intl(2), AccountState::new("c1", "cn")];
        accounts[1].cooldown_until = NOW + 500.0;
        let pool = AccountPool::new(accounts, PoolOptions::default());
        let idx = pool
            .pick_for_session("cn", None, None, None, None, NOW)
            .unwrap();
        assert_eq!(pool.account(idx).unwrap().uid, "c1");
        assert_eq!(pool.account(idx).unwrap().realm, "cn");
    }

    #[test]
    fn ready_chain_and_reasons() {
        let mut a = intl(1);
        assert!(a.ready(None, NOW));
        a.enabled = false;
        assert!(!a.ready(None, NOW));
        assert_eq!(a.unavailable_reason(None, NOW), Some("已停用".into()));
        a.enabled = true;
        a.has_token = false;
        assert!(!a.ready(None, NOW));
        assert_eq!(a.unavailable_reason(None, NOW), Some("无凭证".into()));
        a.has_token = true;
        a.cooldown_until = NOW + 30.0;
        assert!(!a.ready(None, NOW));
        assert_eq!(
            a.unavailable_reason(None, NOW),
            Some("软限流冷却 剩余 30s".into())
        );
        a.cooldown_until = 0.0;
        a.expires_at = Some(NOW + 60.0);
        assert!(!a.ready(None, NOW)); // 不足 120s
        assert_eq!(a.unavailable_reason(None, NOW), Some("凭证已过期".into()));
        a.expires_at = Some(NOW + 500.0);
        assert!(a.ready(None, NOW));
        assert_eq!(a.unavailable_reason(None, NOW), None);
    }

    #[test]
    fn breaker_wins_penalty_tiebreak() {
        let mut a = intl(1);
        a.breaker_until = NOW + 100.0;
        a.cooldown_until = NOW + 100.0; // 平手 → 熔断（列表序优先）
        assert_eq!(
            a.unavailable_reason(None, NOW),
            Some("熔断 剩余 100s".into())
        );
    }

    #[test]
    fn no_accounts_503_detail() {
        let pool = AccountPool::new(vec![AccountState::new("c1", "cn")], PoolOptions::default());
        assert_eq!(
            pool.unavailable_detail("intl", None, NOW),
            "；该区域没有任何账号"
        );
    }

    #[test]
    fn unavailable_detail_lists_reasons_in_chinese() {
        let mut accounts = vec![intl(1), intl(2)];
        accounts[0].cooldown_until = NOW + 30.0;
        accounts[1].enabled = false;
        let pool = AccountPool::new(accounts, PoolOptions::default());
        assert_eq!(
            pool.unavailable_detail("intl", None, NOW),
            "；各账号：「u1」 软限流冷却 剩余 30s，「u2」 已停用"
        );
    }

    #[test]
    fn count_ready_respects_realm_and_cooldown() {
        let mut accounts = vec![intl(1), intl(2), AccountState::new("c1", "cn")];
        accounts[1].cooldown_until = NOW + 100.0;
        let pool = AccountPool::new(accounts, PoolOptions::default());
        assert_eq!(pool.count_ready(Some("intl"), None, NOW), 1);
        assert_eq!(pool.count_ready(Some("cn"), None, NOW), 1);
        assert_eq!(pool.count_ready(None, None, NOW), 2);
    }

    #[test]
    fn page_slice_round_robin() {
        let accounts: Vec<AccountState> = (1..=5).map(intl).collect();
        let pool = AccountPool::new(accounts, PoolOptions::default());
        // size=2：取 rank%2==0 的账号（rank 按 uid 升序 0..4）
        let idx = pool
            .pick_for_session("intl", None, None, None, Some((0, 2)), NOW)
            .unwrap();
        assert_eq!(pool.account(idx).unwrap().uid, "u1"); // rank0
        let idx = pool
            .pick_for_session("intl", None, None, None, Some((0, 2)), NOW)
            .unwrap();
        assert_eq!(pool.account(idx).unwrap().uid, "u3"); // rank2
    }

    #[tokio::test]
    async fn gate_acquire_then_overload_after_wait() {
        let gate = ConcurrencyGate::new(1, false, 1, 0.05);
        let _p1 = gate.acquire().await.expect("first permit");
        let err = gate
            .acquire()
            .await
            .expect_err("second acquire must time out");
        assert_eq!(err.in_flight, 1);
    }

    #[tokio::test]
    async fn gate_resize_only_grows() {
        let gate = ConcurrencyGate::new(2, true, 2, 0.05);
        gate.resize(10);
        let mut permits = Vec::new();
        for _ in 0..10 {
            permits.push(gate.acquire().await.expect("permit within grown capacity"));
        }
        // 已满 → 超时
        assert!(gate.acquire().await.is_err());
        drop(permits);
        // 收缩请求被忽略（只增不缩）
        gate.resize(3);
        let _p = gate
            .acquire()
            .await
            .expect("still usable after shrink attempt");
    }

    #[tokio::test]
    async fn gate_non_auto_ignores_resize() {
        let gate = ConcurrencyGate::new(2, false, 2, 0.05);
        gate.resize(100);
        let p1 = gate.acquire().await.unwrap();
        let p2 = gate.acquire().await.unwrap();
        let _keep = (p1, p2); // 保持占用，不释放
        let err = gate.acquire().await.expect_err("cap stays 2");
        assert_eq!(err.in_flight, 2);
    }

    // ---- T4d：set_product（wb_accounts.py:1200-1214）----

    #[test]
    fn set_product_normalizes_and_rejects_same() {
        let mut a = intl(1);
        assert_eq!(a.product, crate::endpoints::PRODUCT_DESKTOP);
        // 相同（desktop/workbuddy 同值）→ False
        assert!(!a.set_product("workbuddy", 1_000.0));
        assert_eq!(a.product, crate::endpoints::PRODUCT_DESKTOP);
        // 切换 vscode → True
        assert!(a.set_product("vscode", 1_000.0));
        assert_eq!(a.product, crate::endpoints::PRODUCT_VSCODE);
        // 再切回 cli
        assert!(a.set_product("CLI", 1_000.0));
        assert_eq!(a.product, crate::endpoints::PRODUCT_CLI);
    }

    #[test]
    fn set_product_keeps_active_caps() {
        let mut a = intl(1);
        let now = 1_000.0;
        a.model_cooldowns.insert("gpt-5".into(), now + 900.0);
        a.model_cooldowns.insert("gpt-4".into(), now + 600.0);
        a.model_caps.insert(
            "gpt-5".into(),
            ModelCap {
                at: now,
                reset: now + 900.0,
            },
        );
        a.last_error = "some old error".into();
        a.cooldown_until = now + 50.0;
        assert!(a.set_product("vscode", now));
        // 6004 恢复期内的 gpt-5 冷却保留；非 cap 的 gpt-4 冷却被清
        assert!(a.model_cooldowns.contains_key("gpt-5"));
        assert!(!a.model_cooldowns.contains_key("gpt-4"));
        // 账号级惩罚归零
        assert_eq!(a.last_error, "");
        assert_eq!(a.cooldown_until, 0.0);
    }

    // ---- load_account_pool（wb_accounts.py Account.__init__ 逐字段映射）----

    /// 最小可签发 JWT（payload 段 base64url；jwt.rs 只解码不验签）。
    fn fake_jwt(payload: &serde_json::Value) -> String {
        use base64::Engine as _;
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(payload).unwrap());
        format!("header.{enc}.sig")
    }

    fn write_account(dir: &tempfile::TempDir, name: &str, doc: serde_json::Value) {
        std::fs::write(dir.path().join(name), serde_json::to_vec(&doc).unwrap()).unwrap();
    }

    fn load(dir: &tempfile::TempDir) -> Vec<AccountState> {
        load_account_pool(dir.path())
    }

    #[test]
    fn load_maps_uid_realm_product_and_token_fields() {
        let dir = tempfile::tempdir().unwrap();
        write_account(
            &dir,
            "a.json",
            serde_json::json!({
                "uid": "u-1", "realm": "intl", "product": "vscode",
                "accessToken": "tok-1", "expiresAt": 1_800_000.0,
                "enabled": true, "weight": 7,
            }),
        );
        let accounts = load(&dir);
        assert_eq!(accounts.len(), 1);
        let a = &accounts[0];
        assert_eq!(a.uid, "u-1");
        assert_eq!(a.realm, "intl");
        assert_eq!(a.product, crate::endpoints::PRODUCT_VSCODE);
        assert_eq!(a.access_token, "tok-1");
        assert!(a.has_token);
        assert!(a.enabled);
        assert_eq!(a.weight, 7);
        assert_eq!(a.expires_at, Some(1_800_000.0));
        // 运行期状态一律从默认值起算（不随凭据档恢复）
        assert_eq!(a.cooldown_until, 0.0);
        assert_eq!(a.soft_streak, 0);
    }

    #[test]
    fn load_falls_back_to_jwt_uid_and_token_realm() {
        let dir = tempfile::tempdir().unwrap();
        let token = fake_jwt(&serde_json::json!({
            "sub": "jwt-sub", "iss": "https://open.workbuddy.ai",
            "exp": 2_000_000,
        }));
        // 无 uid 字段 → jwt_uid 兜底；无 realm 字段 → token issuer 判定 intl
        write_account(&dir, "a.json", serde_json::json!({"accessToken": token}));
        let a = &load(&dir)[0];
        assert_eq!(a.uid, "jwt-sub");
        assert_eq!(a.realm, "intl");
        assert_eq!(a.expires_at, Some(2_000_000.0));
    }

    #[test]
    fn load_skips_non_account_files_bad_json_and_empty_uid() {
        let dir = tempfile::tempdir().unwrap();
        write_account(
            &dir,
            "settings.json",
            serde_json::json!({"apiKeySet": true}),
        );
        write_account(&dir, "bad.json", serde_json::json!([])); // 非对象
        std::fs::write(dir.path().join("broken.json"), "{ not json").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not an account").unwrap();
        // 空 uid + 空 token → jwt_uid 也空 → 整行丢弃
        write_account(&dir, "empty.json", serde_json::json!({"accessToken": ""}));
        assert_eq!(load(&dir).len(), 0);
    }

    /// 幂等字段回写靠 `credential_file` 定位文件：它必须等于排序后的文件名，
    /// 且手工构造的 `AccountState` 默认为空串（不可回写）。
    #[test]
    fn load_records_credential_file_name() {
        let dir = tempfile::tempdir().unwrap();
        write_account(
            &dir,
            "z.json",
            serde_json::json!({"uid": "u-z", "realm": "cn"}),
        );
        write_account(
            &dir,
            "a.json",
            serde_json::json!({"uid": "u-a", "realm": "cn"}),
        );
        let accounts = load(&dir);
        assert_eq!(accounts[0].uid, "u-a");
        assert_eq!(accounts[0].credential_file, "a.json");
        assert_eq!(accounts[1].credential_file, "z.json");
        // 手工构造（测试注入）不指向任何文件
        assert_eq!(AccountState::new("u", "cn").credential_file, "");
    }

    /// 企业 id：凭据档优先；档里为空则回退 JWT `enterprise_id` claim
    /// （wb_accounts.py:500-508 —— 企业 token 换空间后 id 只在 token 里）。
    #[test]
    fn load_prefers_file_enterprise_id_then_jwt_claim() {
        let dir = tempfile::tempdir().unwrap();
        write_account(
            &dir,
            "file.json",
            serde_json::json!({"uid": "u-file", "realm": "cn", "enterpriseId": "ent-file"}),
        );
        let token = fake_jwt(&serde_json::json!({
            "sub": "u-jwt", "enterprise_id": "ent-jwt",
        }));
        write_account(
            &dir,
            "jwt.json",
            serde_json::json!({"uid": "u-jwt", "realm": "cn", "accessToken": token}),
        );
        write_account(
            &dir,
            "plain.json",
            serde_json::json!({"uid": "u-plain", "realm": "cn"}),
        );
        let accounts = load(&dir);
        // 排序：file.json < jwt.json < plain.json
        assert_eq!(accounts[0].enterprise_id, "ent-file");
        assert_eq!(accounts[1].enterprise_id, "ent-jwt", "档里为空应回退 token");
        assert_eq!(accounts[2].enterprise_id, "");
        assert_eq!(AccountState::new("u", "cn").enterprise_id, "");
    }

    #[test]
    fn load_sorts_by_filename_and_clamps_weight() {
        let dir = tempfile::tempdir().unwrap();
        write_account(
            &dir,
            "b.json",
            serde_json::json!({"uid": "u-b", "realm": "cn", "weight": 999, "enabled": false}),
        );
        write_account(
            &dir,
            "a.json",
            serde_json::json!({"uid": "u-a", "realm": "cn", "weight": 0}),
        );
        let accounts = load(&dir);
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].uid, "u-a"); // sorted(os.listdir)
        assert_eq!(accounts[0].weight, 1); // clamp 下限
        assert_eq!(accounts[1].uid, "u-b");
        assert_eq!(accounts[1].weight, 100); // clamp 上限
        assert!(!accounts[1].enabled);
    }

    // ---- 关卡 4.24（线 A）：登录注册表 + 账号状态操作 ----

    #[test]
    fn account_lookup_by_uid_across_realms() {
        let pool = AccountPool::new(
            vec![intl(1), AccountState::new("c1", "cn")],
            PoolOptions::default(),
        );
        assert_eq!(pool.account_by_uid("u1").unwrap().realm, "intl");
        assert_eq!(pool.account_by_uid("c1").unwrap().realm, "cn");
        assert!(pool.account_by_uid("missing").is_none());
    }

    #[test]
    fn set_enabled_clears_error_when_reenabling() {
        let mut pool = AccountPool::new(vec![intl(1)], PoolOptions::default());
        assert!(pool.set_account_enabled("u1", false));
        assert!(!pool.account_by_uid("u1").unwrap().enabled);
        // 重新启用顺带清限流（Python `clear_error()` 默认 keep_caps=False）
        {
            let account = pool.account_by_uid_mut("u1").unwrap();
            account.cooldown_until = NOW + 50.0;
            account.last_error = "old".into();
        }
        assert!(pool.set_account_enabled("u1", true));
        let account = pool.account_by_uid("u1").unwrap();
        assert!(account.enabled);
        assert_eq!(account.cooldown_until, 0.0);
        assert_eq!(account.last_error, "");
        // 未知 uid → false
        assert!(!pool.set_account_enabled("nope", true));
    }

    #[test]
    fn set_weight_clamps_to_1_100() {
        let mut pool = AccountPool::new(vec![intl(1)], PoolOptions::default());
        assert!(pool.set_account_weight("u1", 0));
        assert_eq!(pool.account_by_uid("u1").unwrap().weight, 1);
        assert!(pool.set_account_weight("u1", 999));
        assert_eq!(pool.account_by_uid("u1").unwrap().weight, 100);
        assert!(pool.set_account_weight("u1", 42));
        assert_eq!(pool.account_by_uid("u1").unwrap().weight, 42);
        assert!(!pool.set_account_weight("nope", 5));
    }

    #[test]
    fn account_product_switches_and_reports_change() {
        let mut pool = AccountPool::new(vec![intl(1)], PoolOptions::default());
        // 相同值 → false（desktop 与 workbuddy 同值）
        assert!(!pool.account_product("u1", "workbuddy", NOW));
        assert!(pool.account_product("u1", "cli", NOW));
        assert_eq!(
            pool.account_by_uid("u1").unwrap().product,
            crate::endpoints::PRODUCT_CLI
        );
        assert!(!pool.account_product("nope", "cli", NOW));
    }

    #[test]
    fn remove_account_only_drops_from_pool() {
        let mut pool = AccountPool::new(vec![intl(1), intl(2)], PoolOptions::default());
        let removed = pool.remove_account("u1").expect("u1 应在池中");
        assert_eq!(removed.uid, "u1");
        assert_eq!(pool.len(), 1);
        assert!(pool.remove_account("u1").is_none());
    }

    #[test]
    fn upsert_replaces_existing_and_appends_new() {
        let mut pool = AccountPool::new(vec![intl(1)], PoolOptions::default());
        let mut replacement = intl(1);
        replacement.weight = 77;
        replacement.realm = "cn".into();
        // 更新既有 → true
        assert!(pool.upsert_account(replacement));
        assert_eq!(pool.len(), 1);
        let account = pool.account_by_uid("u1").unwrap();
        assert_eq!(account.realm, "cn");
        assert_eq!(account.weight, 77);
        // 新增 → false
        assert!(!pool.upsert_account(intl(2)));
        assert_eq!(pool.len(), 2);
    }
}
