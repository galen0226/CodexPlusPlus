use codex_plus_core::aggregate_catalog::save_with_active_catalog_refresh;
use codex_plus_core::gateway::apply::ApplyContext;
use codex_plus_core::relay_config::apply_relay_profile_to_home_with_switch_rules;
use codex_plus_core::session_tuning::{read, write};
use codex_plus_core::settings::{
    BackendSettings, RelayMode, RelayProfile, RelayProtocol, SettingsStore,
};
use serde_json::{Value, json};

// 「上下文与压缩」后端：把压缩 / 精简系统提示等键同步进通用配置。只碰临时目录。

fn profile(id: &str, model: &str, extra: &str) -> RelayProfile {
    RelayProfile {
        id: id.to_string(),
        name: id.to_string(),
        relay_mode: RelayMode::PureApi,
        protocol: RelayProtocol::Responses,
        config_contents: format!(
            "model = \"{model}\"\nmodel_provider = \"custom\"\n{extra}\n[model_providers.custom]\nname = \"custom\"\nwire_api = \"responses\"\nrequires_openai_auth = true\nbase_url = \"https://responses.example.test/v1\"\n"
        ),
        auth_contents: r#"{"OPENAI_API_KEY":"sk-test-redacted"}"#.to_string(),
        ..RelayProfile::default()
    }
}

fn store_with(common: &str, profiles: Vec<RelayProfile>) -> (tempfile::TempDir, SettingsStore) {
    let temp = tempfile::tempdir().unwrap();
    let store = SettingsStore::new(temp.path().join("settings.json"));
    let mut settings = BackendSettings::default();
    settings.relay_profiles_enabled = true;
    settings.active_relay_id = profiles[0].id.clone();
    settings.relay_profiles = profiles;
    settings.relay_common_config_contents = common.to_string();
    store.save(&settings).unwrap();
    (temp, store)
}

fn edits(items: Value) -> Value {
    json!({ "edits": items })
}

#[test]
fn writes_and_removes_whitelisted_keys_in_common_config() {
    let (_temp, store) = store_with("model_reasoning_effort = \"high\"\n", vec![profile("a", "glm-5.2", "")]);
    let state = write(
        &store,
        &edits(json!([
            { "key": "compact_prompt", "value": "保留决定与待办" },
            { "key": "model_post_turn_compact_threshold_percent", "value": 80 },
            { "key": "include_environment_context", "value": false },
        ])),
    )
    .unwrap();
    assert_eq!(state["values"]["compact_prompt"], "保留决定与待办");
    assert_eq!(state["values"]["model_post_turn_compact_threshold_percent"], 80);
    assert_eq!(state["values"]["include_environment_context"], false);
    assert_eq!(state["values"]["tool_output_token_limit"], Value::Null);
    assert_eq!(state["commonConfigApplies"], true);
    let common = store.load().unwrap().relay_common_config_contents;
    assert!(common.contains("model_reasoning_effort = \"high\""), "原有通用配置不动：{common}");
    assert!(common.contains("model_post_turn_compact_threshold_percent = 80"), "{common}");

    // null 与空文本都是删除。
    let state = write(
        &store,
        &edits(json!([
            { "key": "compact_prompt", "value": "  " },
            { "key": "include_environment_context", "value": null },
        ])),
    )
    .unwrap();
    assert_eq!(state["values"]["compact_prompt"], Value::Null);
    assert_eq!(state["values"]["include_environment_context"], Value::Null);
    let common = store.load().unwrap().relay_common_config_contents;
    assert!(!common.contains("compact_prompt") && !common.contains("include_environment_context"), "{common}");
    assert_eq!(read(&store).unwrap()["values"]["model_post_turn_compact_threshold_percent"], 80);
}

#[test]
fn rejects_unknown_keys_and_out_of_range_values_without_partial_writes() {
    let (_temp, store) = store_with("", vec![profile("a", "glm-5.2", "")]);
    for bad in [
        json!([{ "key": "compact_prompt", "value": "ok" }, { "key": "model_provider", "value": "x" }]),
        json!([{ "key": "model_post_turn_compact_threshold_percent", "value": 150 }]),
        json!([{ "key": "model_post_turn_compact_threshold_percent", "value": 0 }]),
        json!([{ "key": "tool_output_token_limit", "value": "200" }]),
        json!([{ "key": "include_apps_instructions", "value": "no" }]),
    ] {
        assert!(write(&store, &edits(bad.clone())).is_err(), "{bad}");
    }
    assert!(store.load().unwrap().relay_common_config_contents.trim().is_empty(), "拒绝时一个键都不写");
}

#[test]
fn reports_when_active_profile_skips_common_config() {
    let mut own = profile("own", "glm-5.2", "");
    own.use_common_config = false;
    let (_temp, store) = store_with("", vec![own]);
    let state = read(&store).unwrap();
    assert_eq!(state["commonConfigApplies"], false);
    assert_eq!(state["activeProfileName"], "own");
}

/// 写进通用配置的键在切换供应商后仍在；改掉 / 删掉时，各快照里和旧值相同的副本一并清掉，
/// 不同的（给单个供应商专门设的）保留。
#[test]
fn edits_survive_switching_and_clear_stale_snapshot_copies() {
    let (_temp, store) = store_with(
        "",
        vec![
            profile("a", "glm-5.2", ""),
            profile("b", "kimi-k3", "tool_output_token_limit = 4000\n"),
            profile("c", "qwen-4", "tool_output_token_limit = 9000\n"),
        ],
    );
    write(&store, &edits(json!([{ "key": "tool_output_token_limit", "value": 4000 }]))).unwrap();
    let settings = store.load().unwrap();
    let home = tempfile::tempdir().unwrap();
    for id in ["a", "b", "c"] {
        let target = settings.relay_profiles.iter().find(|p| p.id == id).unwrap();
        apply_relay_profile_to_home_with_switch_rules(home.path(), target, &settings.relay_common_config_contents)
            .unwrap();
        let config = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(config.contains("tool_output_token_limit = 4000"), "{id}: {config}");
    }

    write(&store, &edits(json!([{ "key": "tool_output_token_limit", "value": null }]))).unwrap();
    let settings = store.load().unwrap();
    let snapshot = |id: &str| settings.relay_profiles.iter().find(|p| p.id == id).unwrap().config_contents.clone();
    assert!(!snapshot("b").contains("tool_output_token_limit"), "和旧通用值相同的副本清掉：{}", snapshot("b"));
    assert!(snapshot("c").contains("tool_output_token_limit = 9000"), "单独设的保留：{}", snapshot("c"));
    let b = settings.relay_profiles.iter().find(|p| p.id == "b").unwrap();
    apply_relay_profile_to_home_with_switch_rules(home.path(), b, &settings.relay_common_config_contents).unwrap();
    let config = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(!config.contains("tool_output_token_limit"), "{config}");
}

#[test]
fn relay_inline_and_session_page_share_tuning_validation_and_cleanup() {
    let inherited = "tool_output_token_limit = 4000\ninclude_apps_instructions = true";
    let mut own = profile("own", "model-b", inherited);
    own.use_common_config = false;
    own.config_contents = own
        .config_contents
        .replace("responses.example", "own.example");
    let mut custom = profile("custom", "model-c", "tool_output_token_limit = 999");
    custom.config_contents = custom
        .config_contents
        .replace("responses.example", "custom.example");
    let (_temp, ctx) = inline_context(
        &format!("{inherited}\nfixture_api_key = 'hidden'\n"),
        vec![profile("active", "model-a", inherited), own, custom],
    );
    let settings = ctx.settings.load().unwrap();
    apply_relay_profile_to_home_with_switch_rules(
        &ctx.codex_home,
        &settings.active_relay_profile(),
        &settings.relay_common_config_contents,
    )
    .unwrap();
    let path = ctx.codex_home.join("config.toml");
    let live_before = std::fs::read_to_string(&path).unwrap();
    for bad in [
        "tool_output_token_limit = 10000001",
        "include_apps_instructions = {}",
        "model_post_turn_compact_threshold_percent = 0",
        &format!("compact_prompt = '{}'", "x".repeat(20001)),
    ] {
        assert!(save_inline(&ctx, bad).is_err());
        assert_eq!(
            settings.settings_revision,
            ctx.settings.load().unwrap().settings_revision
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), live_before);
    }
    let draft = "fixture_api_key = 'hidden'\ntool_output_token_limit = 8000\nmodel_post_turn_compact_threshold_percent = 90\n\
                 compact_prompt = '  '\n\
                 [sandbox_workspace_write]\nnetwork_access = true\n[permissions.work.network]\nenabled = true";
    save_inline(&ctx, draft).unwrap();
    let state = read(&ctx.settings).unwrap();
    assert_eq!(
        state["values"]["model_post_turn_compact_threshold_percent"],
        90
    );
    assert_eq!(state["values"]["include_apps_instructions"], Value::Null);
    let settings = ctx.settings.load().unwrap();
    assert!(settings.relay_common_config_contents.contains("hidden"));
    assert!(!settings.relay_common_config_contents.contains("compact_prompt"));
    assert!(
        !settings
            .active_relay_profile()
            .config_contents
            .contains("tool_output_token_limit")
    );
    assert!(
        settings.relay_profiles[1]
            .config_contents
            .contains("tool_output_token_limit = 4000")
    );
    assert!(
        settings.relay_profiles[2]
            .config_contents
            .contains("tool_output_token_limit = 999")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), live_before);
    let foreign = live_before.replace("responses.example", "external.example");
    assert_ne!(foreign, live_before);
    std::fs::write(&path, &foreign).unwrap();
    save_inline(&ctx, "").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), foreign);
}

#[test]
fn unrelated_template_edits_preserve_legacy_tuning_and_unparseable_snapshot() {
    let common = "tool_output_token_limit = 12000000\ninclude_apps_instructions = true";
    let (temp, ctx) = inline_context(
        common,
        vec![
            profile("active", "model-a", ""),
            profile("legacy", "model-b", ""),
        ],
    );
    let path = temp.path().join("settings.json");
    let mut raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    raw["relayProfiles"][1]["configContents"] = json!("broken = [");
    std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
    save_inline(&ctx, &common.replace("true", "false")).unwrap();
    let saved = ctx.settings.load().unwrap();
    assert_eq!(saved.relay_profiles[1].config_contents.trim(), "broken = [");
    assert_eq!(
        read(&ctx.settings).unwrap()["values"]["tool_output_token_limit"],
        12000000
    );
    let mut raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    raw["relayCommonConfigContents"] = json!("broken = [");
    std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
    assert!(save_inline(&ctx, "tool_output_token_limit = -1").is_err());
    save_inline(&ctx, "tool_output_token_limit = 12").unwrap();
    assert_eq!(read(&ctx.settings).unwrap()["values"]["tool_output_token_limit"], 12);
    assert_eq!(ctx.settings.load().unwrap().relay_profiles, saved.relay_profiles);
}

#[test]
fn sandbox_edits_validate_known_fields_without_dropping_other_preferences() {
    let common = "[sandbox_workspace_write]\nnetwork_access = false\nwritable_roots = ['C:/old']\nextra_preference = 3";
    let (_temp, ctx) = inline_context(common, vec![profile("active", "model-a", "")]);
    let revision = ctx.settings.load().unwrap().settings_revision;
    for field in [
        "network_access = 'false'",
        "exclude_slash_tmp = 1",
        "exclude_tmpdir_env_var = []",
        "writable_roots = 'C:/new'",
        "writable_roots = ['C:/new', false]",
    ] {
        let draft = format!("[sandbox_workspace_write]\n{field}");
        assert!(save_inline(&ctx, &draft).is_err(), "{field}");
        assert_eq!(ctx.settings.load().unwrap().settings_revision, revision);
    }
    let draft = common
        .replace("C:/old", "C:/new")
        .replace("false", "true");
    save_inline(&ctx, &draft).unwrap();
    let readback: toml::Table = ctx.settings.load().unwrap().relay_common_config_contents.parse().unwrap();
    let sandbox = &readback["sandbox_workspace_write"];
    assert_eq!(sandbox["network_access"].as_bool(), Some(true));
    assert_eq!(sandbox["writable_roots"][0].as_str(), Some("C:/new"));
    assert_eq!(sandbox["extra_preference"].as_integer(), Some(3));
}

fn inline_context(common: &str, profiles: Vec<RelayProfile>) -> (tempfile::TempDir, ApplyContext) {
    let (temp, settings) = store_with(common, profiles);
    let ctx = ApplyContext {
        settings,
        codex_home: temp.path().join("codex"),
        ..ApplyContext::default_paths(0)
    };
    (temp, ctx)
}

fn save_inline(ctx: &ApplyContext, draft: &str) -> anyhow::Result<String> {
    let mut next = ctx.settings.load()?;
    next.relay_common_config_contents = draft.into();
    save_with_active_catalog_refresh(&ctx.settings, &ctx.codex_home, &next)
}
