import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { setImmediate as settle } from "node:timers/promises";
import vm from "node:vm";
import ts from "typescript";
import * as jsxRuntime from "react/jsx-runtime";
import { mergeRotatedAuthContents, rebaseSettingsEdits } from "./settings-rebase.ts";

const read = (file: string) => readFileSync(new URL(file, import.meta.url), "utf8");
const compile = (code: string) => ts.transpileModule(code, {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
const dialogCode = compile(read("./AgentConfigEditor.tsx"));
const app = ts.createSourceFile("App.tsx", read("./App.tsx"), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const translate = { t: (value: string) => value, tf: (value: string, args: string[]) => value.replace("{0}", args[0]) };

function nodes(tree: any): any[] {
  if (!tree || typeof tree !== "object") return [];
  if (Array.isArray(tree)) return tree.flatMap(nodes);
  return [tree, ...nodes(tree.props.children)];
}

test("Pi 配置弹窗读取原文，保存失败保留草稿，成功才关闭", async (t) => {
  const state: any[] = [];
  let cursor = 0;
  const close = t.mock.fn();
  const draft = '{"theme":"dark"}';
  const saveArgs = { agent: "pi", original: { source: "pi", contents: "{}" }, contents: draft };
  let conflict = true;
  const invoke = async (command: string, args: unknown) => {
    if (command === "read_agent_config") return { source: "pi", contents: "{}", inputFormat: "JSON", editable: true };
    assert.equal(command, "save_agent_config");
    assert.deepEqual(structuredClone(args), saveArgs);
    if (conflict) throw new Error("配置已变更");
  };
  const modules: Record<string, unknown> = {
    "react/jsx-runtime": jsxRuntime,
    "react-dom": { createPortal: (child: unknown) => child },
    react: {
      createContext: () => null, useId: () => "editor", useRef: () => ({ current: null }),
      useState: (initial: unknown) => {
        const index = cursor++;
        if (!(index in state)) state[index] = initial;
        return [state[index], (value: unknown) => { state[index] = value; }];
      },
      useEffect: (effect: () => void) => { const index = cursor++; if (!state[index]) { state[index] = true; effect(); } },
    },
    "./i18n": translate,
    "./use-escape-layer": { useEscapeLayer() {} }, "./use-modal-focus": { useModalFocus() {} },
    "@/components/ui/button": { Button: "button" },
    "@tauri-apps/api/core": { invoke },
  };
  const context = vm.createContext({ exports: {}, document: { body: {} }, require: (name: string) => modules[name] });
  vm.runInContext(dialogCode, context);
  const render = () => {
    cursor = 0;
    return nodes(context.exports.AgentConfigDialog({ agent: "pi", name: "Pi", onClose: close }));
  };
  const textarea = () => render().find((item) => item.type === "textarea").props;
  const save = () => render().find((item) => item.props.children === "保存").props;
  render();
  await settle();
  assert.equal(textarea().value, "{}");
  assert.equal(save().disabled, true);
  textarea().onChange({ currentTarget: { value: draft } });
  save().onClick();
  await settle();
  assert.match(render().find((node) => node.props.role === "alert").props.children, /配置已变更/);
  assert.equal(close.mock.callCount(), 0);
  assert.equal(textarea().value, draft);
  conflict = false;
  save().onClick();
  await settle();
  assert.equal(close.mock.callCount(), 1);
});

const appBody = app.statements.filter(ts.isFunctionDeclaration).find((node) => node.name?.text === "App")!.body!;
const commonHook = app.statements.filter(ts.isFunctionDeclaration).find(node => node.name?.text === "useRelayCommonConfig")!.getText(app);
const templateDraft = 'developer_instructions = """\n示例表头：\n[mcp_servers.example]\n这仍然是普通文本。\n"""\n';
test("从 Agent 接入打开 Codex 时定位到通用配置，其他 Agent 仍开自己的弹窗", () => {
  function provider(node: ts.Node): ts.JsxOpeningElement | undefined {
    if (ts.isJsxOpeningElement(node) && node.tagName.getText(app) === "OpenAgentConfig.Provider") return node;
    return ts.forEachChild(node, provider);
  }
  const attribute = provider(appBody)!.attributes.properties[0] as ts.JsxAttribute;
  const handler = (attribute.initializer as ts.JsxExpression).expression!.getText(app);
  const calls: unknown[] = [];
  const open = vm.runInNewContext(compile(`(${handler})`), {
    setRelayCommonOpen: (opened: boolean) => calls.push(opened), setConfigTarget: (agent: unknown) => calls.push(agent),
  });
  open({ agent: "codex", name: "Codex" });
  open({ agent: "pi", name: "Pi" });
  assert.deepEqual(calls, [true, { agent: "pi", name: "Pi" }]);
});

test("通用配置拒绝过期草稿；保存失败保留原文，成功后才提示生效条件", async () => {
  const opened = "tool_output_token_limit = 2000";
  let edit: { original: string; text: string } | null = { original: opened, text: templateDraft };
  const settings = { activeRelayId: "codex-plus-gateway", relayCommonConfigContents: opened };
  const writes: unknown[] = [];
  const context = vm.createContext({
    ...translate, useState: () => [edit, (next: typeof edit) => { edit = next; }],
    settingsRef: { current: { status: "ok", settings } }, isSuccessStatus: (status: string) => status === "ok",
    run: (task: () => unknown) => task(), normalizeSettings: (value: unknown) => value, acceptExternalSettings() {},
    call: (command: string, args: { settings: typeof settings }) => {
      writes.push([command, structuredClone(args.settings)]);
      return { status: "ok", settings: args.settings };
    },
  });
  vm.runInContext(appFunctions(["saveCommonTemplate"]), context);
  const useEditor = vm.runInContext(compile(`(${commonHook})`), context);
  const notices: string[] = [];
  const editor = (original: string) => useEditor(original, {
    saveCommonTemplate: context.saveCommonTemplate,
    showMessage: (_title: string, message: string) => notices.push(message),
  });
  assert.equal(await editor("").save(), null);
  await editor(opened).save((text: string) => { assert.equal(text, templateDraft); return null; });
  assert.equal(editor(opened).contents, templateDraft);
  assert.deepEqual(notices, []);
  await editor(opened).save();
  assert.deepEqual(writes, [["save_settings", { ...settings, relayCommonConfigContents: templateDraft }]]);
  assert.equal(editor(opened).dirty, false);
  assert.match(notices[0], /重新应用或切换到使用通用配置的供应商后生效/);
});

function onboardingCheck(node: ts.Node): string | undefined {
  if (ts.isJsxAttribute(node) && node.name.getText(app) === "onCheckCodex"
    && node.initializer && ts.isJsxExpression(node.initializer)) return node.initializer.expression?.getText(app);
  return ts.forEachChild(node, onboardingCheck);
}
const checkHandler = onboardingCheck(appBody);
assert.ok(checkHandler);
const checkCode = compile(`globalThis.checkEnhancements = ${checkHandler};`);
const appFunctions = (names: string[]) => compile(appBody.statements.filter(ts.isVariableStatement)
  .flatMap((statement) => [...statement.declarationList.declarations])
  .filter((decl) => ["rebaseConfigDraft", ...names].includes(decl.name.getText(app))).map((decl) => `globalThis.${decl.name.getText(app)} = ${decl.initializer!.getText(app)};`).join("\n"));

async function checkPendingSave(scenario: "autosave" | "refresh" | "conflict") {
  const editConfig = scenario === "conflict";
  const oldConfig = 'model = "a"\ntool_output_token_limit = 4096\n';
  const cleanConfig = 'model = "a"\n';
  const old = { activeRelayId: "A", revision: "r1", relayCommonConfigContents: "tool_output_token_limit = 4096\n", relayProfiles: [
    { id: "A", name: "A", configContents: oldConfig }, { id: "B", name: "B", configContents: "" },
  ] };
  const latest = { ...old, revision: "r2", relayCommonConfigContents: "", relayProfiles: [
    { ...old.relayProfiles[0], configContents: cleanConfig }, old.relayProfiles[1],
  ] };
  let form = { ...old, relayProfiles: [
    { ...old.relayProfiles[0], configContents: editConfig ? oldConfig + "user_edit = true\n" : oldConfig },
    { ...old.relayProfiles[1], name: "renamed B" },
  ] };
  let disk = latest;
  let release!: () => void;
  const conflict = new Promise<void>((resolve) => { release = resolve; });
  const context = vm.createContext({
    settingsRef: { current: { settings: old } }, normalizeSettings: (value: unknown) => value,
    backendSettingsEqual: (left: unknown, right: unknown) => JSON.stringify(left) === JSON.stringify(right),
    relayFilesRead: { current: 0 }, setRelayFiles: (value: unknown) => assert.equal(value, null), stringifyError: String,
    run: (task: () => Promise<unknown>) => task(), setOverview: () => {}, hasCodexEnhancements: () => true,
    rebaseSettingsEdits, mergeRotatedAuthContents, isSuccessStatus: (status: string) => status === "ok",
    setSettings: (result: { settings: typeof old }) => { context.settingsRef.current = result; },
    setSettingsForm: (update: (current: typeof old) => typeof old) => { form = update(form); },
    invoke: async (command: string, args?: { expectedRevision: string; settings: typeof old }) => {
      if (command === "load_overview") return { status: "ok", codex_app: { status: "found" } };
      if (command === "load_settings") return { status: "ok", settings: disk };
      assert.equal(command, "save_settings");
      if (args!.expectedRevision !== disk.revision) {
        await conflict;
        return { status: "failed", message: "settings_revision_conflict" };
      }
      disk = { ...args!.settings, revision: "r3" };
      return { status: "ok", settings: disk };
    },
  });
  vm.runInContext(appFunctions(["call", "acceptExternalSettings", "autoSaveSettings", "retrySaveSettingsAfterConflict"]), context);
  vm.runInContext(checkCode, context);
  if (scenario === "autosave") {
    const pending = context.autoSaveSettings(form);
    const renamed = { ...form.relayProfiles[1], name: "B after submit" };
    form = { ...form, relayProfiles: [form.relayProfiles[0], renamed] };
    release();
    assert.equal((await pending).status, "saved");
    assert.deepEqual(structuredClone(form), { ...latest, revision: "r3", relayProfiles: [latest.relayProfiles[0], renamed] });
  } else {
    const pending = context.call("save_settings", { settings: form });
    context.acceptExternalSettings({ status: "ok", settings: latest });
    assert.equal(await context.checkEnhancements(), true);
    assert.equal(form.revision, editConfig ? "r1" : "r2");
    release();
    if (editConfig) {
      await assert.rejects(pending, /settings_revision_conflict/);
      assert.deepEqual(disk, latest);
      assert.match(form.relayProfiles[0].configContents, /user_edit = true/);
      return;
    }
    await pending;
  }
  assert.deepEqual(structuredClone(disk), { ...latest, revision: "r3", relayProfiles: [latest.relayProfiles[0], { ...old.relayProfiles[1], name: "renamed B" }] });
}

test("在途保存保留后续改名，刷新后不恢复旧配置，也不覆盖冲突草稿", async () => {
  for (const scenario of ["autosave", "refresh", "conflict"] as const) await checkPendingSave(scenario);
});
