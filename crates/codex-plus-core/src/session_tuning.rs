//! Codex 页面「上下文与压缩」（96-context-tuning.js）的后端：把 Codex 原生的会话调优键同步进
//! Codex++ 的通用配置。
//!
//! 为什么要同步：Codex++ 切换供应商 / 重启时会按「供应商快照 + 通用配置」重写 config.toml，
//! 页面只用 `config/batchWrite` 写进 live 的话，切走时它会被回存进当前供应商的快照、换到别的
//! 供应商就没了。写进通用配置后：合并时通用配置覆盖快照、回存时同值会被剥掉，于是对所有
//! 开了「使用通用配置」的供应商一致生效。
//!
//! 页面先经 Codex 的 `config/batchWrite` 写 live（Codex 自己校验、新会话立即生效），回读通过后
//! 再调这里；所以这里只认白名单里的键，并按类型和范围再校验一遍，避免写出 Codex 拒载的值
//! （例如阈值 150 会让每次 `thread/start` 都失败）。

use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item};

use crate::gateway::restore::parse_toml;
use crate::settings::{BackendSettings, SettingsStore};

/// 一个可同步键的取值约束。
#[derive(Clone, Copy)]
enum KeyKind {
    Text { max_chars: usize },
    Int { min: i64, max: i64 },
    Bool,
}

/// 允许经页面写进通用配置的键。只放实测过「写进 config.toml 新会话就生效」的。
const SESSION_TUNING_KEYS: &[(&str, KeyKind)] = &[
    ("compact_prompt", KeyKind::Text { max_chars: 20_000 }),
    // Codex 自己的校验是 0~100，0 等于关闭；页面用「不设置」表示关闭，所以这里从 1 起。
    ("model_post_turn_compact_threshold_percent", KeyKind::Int { min: 1, max: 100 }),
    ("tool_output_token_limit", KeyKind::Int { min: 1, max: 10_000_000 }),
    ("include_environment_context", KeyKind::Bool),
    ("include_environment_context_time", KeyKind::Bool),
    ("include_permissions_instructions", KeyKind::Bool),
    ("include_apps_instructions", KeyKind::Bool),
    ("include_collaboration_mode_instructions", KeyKind::Bool),
];

fn key_kind(key: &str) -> Option<KeyKind> {
    SESSION_TUNING_KEYS.iter().find(|(name, _)| *name == key).map(|(_, kind)| *kind)
}

/// 这些根键由通用模板按旧值清理，不能在管理器中仅按键名删除供应商的覆盖值。
pub fn is_tuning_key(key: &str) -> bool {
    key_kind(key).is_some()
}

#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PendingBackfill {
    profile: String,
    values: BTreeMap<String, ExpectedLive>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
enum ExpectedLive {
    Unapplied,
    Value(String),
    Deleted,
}

const PENDING_KEY: &str = "_relayTuningBackfill";

fn fingerprint(item: &Item) -> String {
    let value = item_to_json(item);
    let text = if value.is_null() { item.to_string() } else { value.to_string() };
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

impl PendingBackfill {
    pub(crate) fn store_in(&self, raw: &mut Map<String, Value>) -> anyhow::Result<()> {
        if self.values.is_empty() {
            raw.remove(PENDING_KEY);
        } else {
            raw.insert(PENDING_KEY.into(), serde_json::to_value(self)?);
        }
        Ok(())
    }

    pub(crate) fn reconcile(&self, id: &str, snapshot: &str, backfilled: &mut String) -> anyhow::Result<()> {
        if self.profile != id { return Ok(()); }
        let mut live = parse_toml("供应商回填配置", backfilled)?;
        let saved = parse_toml("供应商快照", snapshot)?;
        for (key, expected) in &self.values {
            if !is_tuning_key(key) { continue; }
            let matches = match (live.get(key), expected) {
                (Some(item), ExpectedLive::Value(hash)) => fingerprint(item) == *hash,
                (None, ExpectedLive::Deleted) => true,
                _ => false,
            };
            if matches {
                match saved.get(key) {
                    Some(item) => live[key] = item.clone(),
                    None => { live.remove(key); }
                }
            }
        }
        *backfilled = live.to_string();
        Ok(())
    }
}

/// 完整重放模板成功后旧值才失效；普通启动或多开实例不应消费主实例的记录。
pub struct TemplateReapplyRollback {
    store: SettingsStore,
    before: BackendSettings,
}

impl TemplateReapplyRollback {
    pub fn restore(self, restore_live: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.store.save_with_commit(&self.before, |_| restore_live())?;
        Ok(())
    }
}

pub fn with_template_reapply<T>(
    store: &SettingsStore,
    settings: &BackendSettings,
    apply: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<(T, Option<TemplateReapplyRollback>)> {
    let Some(pending) = settings.pending_tuning_backfill.as_ref().filter(|record| record.profile == settings.active_relay_id) else {
        return Ok((apply()?, None));
    };
    let mut saved = store.load()?;
    ensure!(saved.settings_revision == settings.settings_revision, "settings_revision_conflict：重新应用前设置已更新");
    let mut result = None;
    let committed = store.with_tuning_backfill(Default::default()).save_with_commit(&saved, |_| {
        result = Some(apply()?);
        Ok(())
    })?;
    saved.settings_revision = committed;
    Ok((result.expect("successful publish returned its result"), Some(TemplateReapplyRollback {
        store: store.with_tuning_backfill(pending.clone()), before: saved,
    })))
}

fn item_to_json(item: &Item) -> Value {
    let Some(value) = item.as_value() else {
        return Value::Null;
    };
    if let Some(text) = value.as_str() {
        return json!(text);
    }
    if let Some(number) = value.as_integer() {
        return json!(number);
    }
    if let Some(flag) = value.as_bool() {
        return json!(flag);
    }
    Value::Null
}

/// 把页面给的值按键的约束转成 TOML 值；`null` 表示删掉这个键。
fn toml_value(key: &str, kind: KeyKind, value: &Value) -> anyhow::Result<Option<toml_edit::Value>> {
    if value.is_null() {
        return Ok(None);
    }
    match kind {
        KeyKind::Text { max_chars } => {
            let text = value.as_str().with_context(|| format!("{key} 必须是文本"))?;
            if text.trim().is_empty() {
                return Ok(None);
            }
            if text.chars().count() > max_chars {
                anyhow::bail!("{key} 太长（最多 {max_chars} 字）");
            }
            Ok(Some(text.into()))
        }
        KeyKind::Int { min, max } => {
            let number = value.as_i64().with_context(|| format!("{key} 必须是整数"))?;
            if number < min || number > max {
                anyhow::bail!("{key} 必须在 {min} 到 {max} 之间");
            }
            Ok(Some(number.into()))
        }
        KeyKind::Bool => {
            let flag = value.as_bool().with_context(|| format!("{key} 必须是开或关"))?;
            Ok(Some(flag.into()))
        }
    }
}

fn values_of(doc: &DocumentMut) -> Value {
    let mut values = Map::new();
    for (key, _) in SESSION_TUNING_KEYS {
        values.insert((*key).to_string(), doc.get(key).map(item_to_json).unwrap_or(Value::Null));
    }
    Value::Object(values)
}

/// 当前供应商会不会套用通用配置。不会时页面要提示「设置只跟着这个供应商走」。
fn common_config_applies(settings: &crate::settings::BackendSettings) -> bool {
    !settings.relay_profiles_enabled || settings.active_relay_profile().use_common_config
}

fn state_value(settings: &crate::settings::BackendSettings, doc: &DocumentMut) -> Value {
    let active = settings.active_relay_profile();
    json!({
        "status": "ok",
        "values": values_of(doc),
        "commonConfigApplies": common_config_applies(settings),
        "activeProfileName": if active.name.trim().is_empty() { active.id.clone() } else { active.name.clone() },
    })
}

/// 标量按值比较；旧模板里已有的不支持类型只认原样保留。
fn matches_old(doc: &DocumentMut, key: &str, old: &DocumentMut) -> bool {
    old.get(key).is_some_and(|before| {
        doc.get(key).is_some_and(|item| {
            let value = item_to_json(before);
            if value.is_null() {
                before.to_string() == item.to_string()
            } else {
                item_to_json(item) == value
            }
        })
    })
}

pub(crate) fn validate_common_fields(old: &DocumentMut, draft: &DocumentMut) -> anyhow::Result<()> {
    for (key, item) in draft.iter() {
        if old
            .get(key)
            .is_some_and(|previous| previous.to_string() == item.to_string())
        {
            continue;
        }
        let valid = match key {
            "features" | "tui" | "permissions" => item.is_table_like(),
            "sandbox_workspace_write" => item.as_table_like().is_some_and(|sandbox| {
                sandbox.iter().all(|(field, value)| {
                    if old
                        .get(key)
                        .and_then(|previous| previous.get(field))
                        .is_some_and(|previous| previous.to_string() == value.to_string())
                    {
                        return true;
                    }
                    match field {
                        "network_access" | "exclude_tmpdir_env_var" | "exclude_slash_tmp" => {
                            value.is_bool()
                        }
                        "writable_roots" => value
                            .as_array()
                            .is_some_and(|roots| roots.iter().all(|root| root.is_str())),
                        _ => true,
                    }
                })
            }),
            "notify" => item
                .as_array()
                .is_some_and(|args| args.iter().all(|arg| arg.is_str())),
            "web_search" => matches!(item.as_str(), Some("disabled" | "cached" | "live")),
            "sandbox_mode" => matches!(
                item.as_str(),
                Some("read-only" | "workspace-write" | "danger-full-access")
            ),
            "approval_policy" => {
                matches!(
                    item.as_str(),
                    Some("untrusted" | "on-failure" | "on-request" | "never")
                ) || item.as_table_like().is_some_and(|policy| {
                    policy.len() == 1
                        && policy.get("granular").is_some_and(|value| {
                            value
                                .as_table_like()
                                .is_some_and(|flags| flags.iter().all(|(_, flag)| flag.is_bool()))
                        })
                })
            }
            "personality" => matches!(item.as_str(), Some("none" | "friendly" | "pragmatic")),
            "model_verbosity" => matches!(item.as_str(), Some("low" | "medium" | "high")),
            "model_reasoning_effort" => matches!(
                item.as_str(),
                Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh")
            ),
            "model_reasoning_summary" => matches!(
                item.as_str(),
                Some("auto" | "concise" | "detailed" | "none")
            ),
            "developer_instructions" | "model_instructions_file" => item.is_str(),
            _ => true,
        };
        ensure!(valid, "{key} 的值无效");
    }
    Ok(())
}

/// Relay 内联编辑和会话页共用校验和快照清理；会话页已通过 batchWrite 更新 live，不再写第二遍。
pub(crate) fn save_common(
    store: &SettingsStore,
    mut settings: BackendSettings,
    old: &DocumentMut,
    mut next: DocumentMut,
    relay_previous: Option<&BackendSettings>,
    batch_keys: &[&str],
    save_settings: impl FnOnce(&SettingsStore, &BackendSettings) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    let mut changed = Vec::new();
    for &(key, kind) in SESSION_TUNING_KEYS {
        if matches_old(&next, key, old) {
            continue;
        }
        if let Some(item) = next.get(key) {
            let value = item_to_json(item);
            ensure!(!value.is_null(), "{key} 的值无效");
            if toml_value(key, kind, &value)?.is_none() {
                next.remove(key);
            }
        }
        if old.contains_key(key) || next.contains_key(key) {
            changed.push(key);
        }
    }
    let previous = relay_previous.unwrap_or(&settings);
    let active_id = previous.active_relay_id.clone();
    let tracks_active = previous.relay_profiles_enabled
        && active_id == settings.active_relay_id
        && previous.active_relay_profile().use_common_config
        && settings.active_relay_profile().use_common_config
        && active_id != crate::gateway::apply::CODEX_GATEWAY_RELAY_ID;
    let mut pending = if tracks_active && (!changed.is_empty() || !batch_keys.is_empty()) {
        previous.pending_tuning_backfill.clone().filter(|record| record.profile == active_id)
    } else {
        None
    };
    for profile in settings
        .relay_profiles
        .iter_mut()
        .filter(|p| !changed.is_empty() && p.use_common_config)
    {
        // 未能识别同值副本的历史快照原样保留，不能阻断对通用模板的编辑。
        let Ok(mut snapshot) = profile.config_contents.parse::<DocumentMut>() else {
            continue;
        };
        let mut removed = false;
        for &key in &changed {
            if profile.id == active_id && pending.as_ref().is_some_and(|record| record.values.contains_key(key)) {
                continue;
            }
            if matches_old(&snapshot, key, old) {
                snapshot.remove(key);
                removed = true;
            }
        }
        if removed {
            profile.config_contents =
                crate::relay_config::normalize_config_text(&snapshot.to_string());
        }
    }
    settings.relay_common_config_contents =
        crate::relay_config::normalize_config_text(&next.to_string());
    let pending_store;
    if tracks_active {
        let keys = if relay_previous.is_some() { &changed[..] } else { batch_keys };
        let snapshot = (!batch_keys.is_empty()).then(|| settings.active_relay_profile()
            .config_contents.parse::<DocumentMut>().ok()).flatten();
        for &key in keys {
            if !changed.contains(&key)
                && !pending.as_ref().is_some_and(|record| record.values.contains_key(key))
                && !snapshot.as_ref().is_some_and(|doc| doc.contains_key(key) && !matches_old(doc, key, &next))
            {
                continue;
            }
            let record = pending.get_or_insert_with(|| PendingBackfill {
                profile: active_id.clone(),
                ..Default::default()
            });
            if relay_previous.is_some() {
                // 连续保存并不应用，中间的模板值不能充当 live 的来源。
                record.values.entry(key.into()).or_insert_with(|| old.get(key)
                    .map_or(ExpectedLive::Unapplied, |item| ExpectedLive::Value(fingerprint(item))));
            } else {
                record.values.insert(key.into(), next.get(key)
                    .map_or(ExpectedLive::Deleted, |item| ExpectedLive::Value(fingerprint(item))));
            }
        }
    }
    let store = if let Some(pending) = pending {
        pending_store = store.with_tuning_backfill(pending);
        &pending_store
    } else {
        store
    };
    let revision = save_settings(store, &settings)?;
    crate::settings::notify_external_settings_change();
    Ok(revision)
}

/// `/session-tuning/read`：通用配置里这些键当前的值。
pub fn read(store: &SettingsStore) -> anyhow::Result<Value> {
    let settings = store.load().context("读取 Codex++ 设置失败")?;
    let doc = parse_toml("通用配置", &settings.relay_common_config_contents)?;
    Ok(state_value(&settings, &doc))
}

/// `/session-tuning/write`：`{ edits: [{ key, value }] }`，`value: null` 删除。
/// 白名单外的键整批拒绝，不做部分写入。
pub fn write(store: &SettingsStore, payload: &Value) -> anyhow::Result<Value> {
    let edits = payload
        .get("edits")
        .and_then(Value::as_array)
        .context("缺少 edits")?;
    let mut planned = Vec::with_capacity(edits.len());
    for edit in edits {
        let key = edit.get("key").and_then(Value::as_str).context("edits 里缺少 key")?;
        let kind = key_kind(key).with_context(|| format!("{key} 不能在这里修改"))?;
        let value = toml_value(key, kind, edit.get("value").unwrap_or(&Value::Null))?;
        planned.push((key.to_string(), value));
    }
    // 设置文件被别处同时改了（revision 冲突）就重读再来一次。
    let mut last_error = None;
    for _ in 0..2 {
        let settings = store.load().context("读取 Codex++ 设置失败")?;
        let old = parse_toml("通用配置", &settings.relay_common_config_contents)?;
        let mut doc = old.clone();
        for (key, value) in &planned {
            match value {
                Some(value) => doc[key.as_str()] = Item::Value(value.clone()),
                None => {
                    doc.remove(key);
                }
            }
        }
        let keys: Vec<_> = planned.iter().map(|(key, _)| key.as_str()).collect();
        match save_common(store, settings, &old, doc, None, &keys, SettingsStore::save_versioned) {
            Ok(_) => {
                let saved = store.load().context("保存后读取 Codex++ 设置失败")?;
                let saved_doc = parse_toml("通用配置", &saved.relay_common_config_contents)?;
                return Ok(state_value(&saved, &saved_doc));
            }
            Err(error) if format!("{error:#}").contains("settings_revision_conflict") => last_error = Some(error),
            Err(error) => return Err(error).context("保存通用配置失败"),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("保存通用配置失败")))
}
