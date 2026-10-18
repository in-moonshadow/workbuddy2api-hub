//! 闸门 0（关卡 4.2）契约：账号文件的原子写回 + 幂等日期字段。
//!
//! 对照 Python `wb_accounts.py`：
//! - `Account.save()` :764-794 —— 同目录临时文件 + `os.replace` + 落盘前剥离
//!   `RUNTIME_ONLY_FIELDS` + `indent(2)`
//! - `lastCheckin` / `lastActivityReport` / `lastDailyChat` 的值是
//!   `"%Y-%m-%d %H:%M:%S"` **本地时间**字符串（`time.strftime`），「今天已做」
//!   由前 10 字符 `%Y-%m-%d` 前缀比较决定
//! - `AccountPool.load()` :2551-2571 —— 枚举用 `sorted(os.listdir(self.dir))`
//!
//! 本文件钉住六条不变式：写入后重读一致、未涉及的键逐字不变、运行时字段不
//! 落盘、临时文件无残留（成功与失败两侧）、枚举排序使 `representative()` 可预
//! 测、写回入口拒绝目录外的文件名。

use std::fs;
use std::path::Path;

use wb_storage::{
    read_idempotent_fields, representative, write_idempotent_fields, IdempotentFields,
};

/// 合成账号文档：包含三个幂等字段、两个运行时字段与一个**未知键**。
fn account_doc(uid: &str, realm: &str, token: &str) -> serde_json::Value {
    serde_json::json!({
        "uid": uid,
        "realm": realm,
        "product": "codebuddy",
        "enabled": true,
        "accessToken": token,
        "expiresAt": 1_800_000_000,
        "lastCheckin": "2026-10-15 09:00:00",
        "lastActivityReport": "2026-10-15 22:00:00",
        "lastDailyChat": "2026-10-15 22:05:00",
        "cooldownUntil": 0.0,
        "lastError": "",
        // 未知键：写入端不得因为不认识它而丢弃
        "futureField": {"nested": [1, 2, 3]},
    })
}

fn write_account(dir: &Path, file: &str, doc: &serde_json::Value) {
    fs::write(dir.join(file), serde_json::to_string_pretty(doc).unwrap()).unwrap();
}

/// 目录里剩余的 `*.tmp` 文件（任何临时文件残留都会让 `save()` 语义失真）。
fn tmp_residue(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    out.sort();
    out
}

#[test]
fn runtime_only_fields_mirror_matches_identity() {
    // storage 是零依赖叶子 crate，RUNTIME_ONLY_FIELDS 是 identity 的镜像；
    // 这里逐字对拍，任一侧改动而另一侧没跟上就失败（防止闸门 0 的字段集漂移）。
    // 生产代码不依赖 wb-identity，本断言只用 dev-dependency。
    assert_eq!(
        wb_identity::export::RUNTIME_ONLY_FIELDS.to_vec(),
        vec!["lastError", "cooldownUntil"],
        "storage 的 RUNTIME_ONLY_FIELDS 镜像与 identity 不一致"
    );
}

#[test]
fn write_then_read_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    write_account(dir.path(), "a.json", &account_doc("a", "intl", "tok-a"));

    let fields = IdempotentFields {
        last_checkin: Some("2026-10-17 09:12:34".to_string()),
        last_activity_report: None,
        last_daily_chat: Some("2026-10-17 22:00:01".to_string()),
    };
    write_idempotent_fields(dir.path(), "a.json", &fields).unwrap();

    // `read` 返回磁盘上的**全量**值：`None` 表示「不改写」，不是「读出来是 None」。
    let after = read_idempotent_fields(dir.path(), "a.json").unwrap();
    assert_eq!(after.last_checkin.as_deref(), Some("2026-10-17 09:12:34"));
    assert_eq!(
        after.last_activity_report.as_deref(),
        Some("2026-10-15 22:00:00"),
        "`None` 字段应保留磁盘原值"
    );
    assert_eq!(
        after.last_daily_chat.as_deref(),
        Some("2026-10-17 22:00:01")
    );
}

#[test]
fn none_fields_leave_disk_value_untouched() {
    let dir = tempfile::tempdir().unwrap();
    write_account(dir.path(), "a.json", &account_doc("a", "intl", "tok-a"));

    // 只写签到；另外两个字段在文件里已有值，必须保持磁盘原值。
    write_idempotent_fields(
        dir.path(),
        "a.json",
        &IdempotentFields {
            last_checkin: Some("2026-10-17 09:12:34".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let after = read_idempotent_fields(dir.path(), "a.json").unwrap();
    assert_eq!(after.last_checkin.as_deref(), Some("2026-10-17 09:12:34"));
    assert_eq!(
        after.last_activity_report.as_deref(),
        Some("2026-10-15 22:00:00")
    );
    assert_eq!(
        after.last_daily_chat.as_deref(),
        Some("2026-10-15 22:05:00")
    );
}

#[test]
fn untouched_keys_survive_byte_for_byte_in_value() {
    let dir = tempfile::tempdir().unwrap();
    let before = account_doc("a", "intl", "tok-a");
    write_account(dir.path(), "a.json", &before);

    write_idempotent_fields(
        dir.path(),
        "a.json",
        &IdempotentFields {
            last_checkin: Some("2026-10-17 09:12:34".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let text = fs::read_to_string(dir.path().join("a.json")).unwrap();
    let after: serde_json::Value = serde_json::from_str(&text).unwrap();
    let before_obj = before.as_object().unwrap();
    let after_obj = after.as_object().unwrap();

    for (key, value) in before_obj {
        if key == "lastCheckin" || key == "lastError" || key == "cooldownUntil" {
            continue;
        }
        assert_eq!(
            after_obj.get(key),
            Some(value),
            "未涉及的键 `{key}` 被写回流程改动了"
        );
    }
    // 未知键必须还在（不能因为反序列化到一个不认识的 struct 就丢掉）
    assert_eq!(after_obj["futureField"]["nested"][2], 3);
    // 缩进沿用 Python `indent=2`
    assert!(text.contains("\n  \"uid\""), "落盘应为 indent(2) 的 JSON");
}

#[test]
fn runtime_only_fields_are_not_persisted() {
    let dir = tempfile::tempdir().unwrap();
    write_account(dir.path(), "a.json", &account_doc("a", "intl", "tok-a"));

    write_idempotent_fields(
        dir.path(),
        "a.json",
        &IdempotentFields {
            last_checkin: Some("2026-10-17 09:12:34".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("a.json")).unwrap()).unwrap();
    for field in wb_identity::export::RUNTIME_ONLY_FIELDS {
        assert!(
            after.get(field).is_none(),
            "`{field}` 属 RUNTIME_ONLY_FIELDS，不应落盘"
        );
    }
    // 同时确认不是把文件写空了：token 仍在
    assert_eq!(after["accessToken"], "tok-a");
}

#[test]
fn no_temp_file_left_after_success() {
    let dir = tempfile::tempdir().unwrap();
    write_account(dir.path(), "a.json", &account_doc("a", "intl", "tok-a"));

    for i in 0..5 {
        write_idempotent_fields(
            dir.path(),
            "a.json",
            &IdempotentFields {
                last_checkin: Some(format!("2026-10-17 09:12:{i:02}")),
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(tmp_residue(dir.path()), Vec::<String>::new());
}

#[test]
fn no_temp_file_left_after_failure() {
    let dir = tempfile::tempdir().unwrap();
    // 账号文件不存在 → 写回必须报错且不在目录里留下任何临时文件
    assert!(write_idempotent_fields(
        dir.path(),
        "missing.json",
        &IdempotentFields {
            last_checkin: Some("2026-10-17 09:12:34".to_string()),
            ..Default::default()
        },
    )
    .is_err());
    assert_eq!(tmp_residue(dir.path()), Vec::<String>::new());
}

#[test]
fn write_rejects_names_outside_accounts_dir() {
    let dir = tempfile::tempdir().unwrap();
    write_account(dir.path(), "a.json", &account_doc("a", "intl", "tok-a"));
    write_account(
        dir.path(),
        "settings.json",
        &serde_json::json!({"api_key": ""}),
    );

    let fields = IdempotentFields {
        last_checkin: Some("2026-10-17 09:12:34".to_string()),
        ..Default::default()
    };
    // 路径穿越 / 非 .json / 配置文件 / 空名，一律拒绝
    for bad in [
        "../a.json",
        "sub/a.json",
        "a.txt",
        "",
        ".",
        "..",
        "settings.json",
        "model.json",
        "active_realm.json",
    ] {
        assert!(
            write_idempotent_fields(dir.path(), bad, &fields).is_err(),
            "写回入口接受了非法文件名 `{bad}`"
        );
        assert!(read_idempotent_fields(dir.path(), bad).is_err());
    }
    assert_eq!(tmp_residue(dir.path()), Vec::<String>::new());
}

#[test]
fn representative_follows_sorted_file_order() {
    // Python `sorted(os.listdir)`：`a.json` 先于 `b.json`，与创建顺序无关。
    // 两个账号都没有 token → representative 取「第一个」，必须是 a.json。
    let dir = tempfile::tempdir().unwrap();
    write_account(dir.path(), "b.json", &account_doc("uid-b", "intl", ""));
    write_account(dir.path(), "a.json", &account_doc("uid-a", "intl", ""));

    let rep = representative(dir.path(), "intl").expect("应有代表账号");
    assert_eq!(rep.credential_file, "a.json");
    assert_eq!(rep.uid, "uid-a");
}

#[test]
fn representative_prefers_first_sorted_account_with_token() {
    // a.json 有 token 但 realm 不匹配 → 跳过；b.json 匹配且有 token → 选中 b。
    // 再确认 z.json 即使有 token 也不会抢在 b.json 之前（排序优先）。
    let dir = tempfile::tempdir().unwrap();
    write_account(dir.path(), "z.json", &account_doc("uid-z", "intl", "tok-z"));
    write_account(dir.path(), "a.json", &account_doc("uid-a", "cn", "tok-a"));
    write_account(dir.path(), "b.json", &account_doc("uid-b", "intl", "tok-b"));

    let rep = representative(dir.path(), "intl").expect("应有代表账号");
    assert_eq!(rep.credential_file, "b.json");
    assert_eq!(rep.uid, "uid-b");
}

#[test]
fn representative_is_stable_across_repeated_calls() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["c", "a", "b", "d"] {
        write_account(
            dir.path(),
            &format!("{name}.json"),
            &account_doc(&format!("uid-{name}"), "intl", ""),
        );
    }
    let first = representative(dir.path(), "intl").unwrap();
    for _ in 0..5 {
        assert_eq!(representative(dir.path(), "intl").unwrap(), first);
    }
    assert_eq!(first.credential_file, "a.json");
}
