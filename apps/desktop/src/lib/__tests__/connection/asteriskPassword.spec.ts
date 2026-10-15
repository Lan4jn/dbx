// @vitest-environment happy-dom
import { createApp, nextTick } from "vue";
import { createI18n } from "vue-i18n";
import { describe, expect, it } from "vitest";
import PasswordInput from "@/components/ui/PasswordInput.vue";
import { isSavedCredential } from "@/lib/security/passwordViewer";
import { buildConnectionUrlCopy, type ConnectionUrlCopyConfig } from "@/lib/connection/connectionUrlBuilder";
import { parseConnectionUrl } from "@/lib/connection/connectionUrl";

const passwords = ["*", "***", "********", "Abc*123", "*Abc123", "Abc123*", "Ab**cd", "a*%40@:/#?&+'\\ b", "星号*密码", "Abc＊123"];

describe("literal asterisk password regression", () => {
  it.each(passwords)("preserves input, emitted value, reveal and JSON for %j", async (password) => {
    expect(isSavedCredential(password)).toBe(false);
    const container = document.createElement("div");
    document.body.append(container);
    let emitted: unknown;
    const app = createApp(PasswordInput, { modelValue: password, "onUpdate:modelValue": (value: unknown) => { emitted = value; } });
    app.use(createI18n({ legacy: false, locale: "en", messages: { en: { common: { showPassword: "Show", hidePassword: "Hide" } } } }));
    try {
      app.mount(container);
      await nextTick();
      const input = container.querySelector("input")!;
      expect(input.value).toBe(password);
      container.querySelector("button")?.click();
      await nextTick();
      expect(input.value).toBe(password);
      input.value = password + "*";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      await nextTick();
      await nextTick();
      expect(emitted).toBe(password + "*");
      expect(JSON.parse(JSON.stringify({ password })).password).toBe(password);
    } finally {
      app.unmount();
      container.remove();
    }
  });

  for (const scheme of ["mysql", "postgresql", "redis", "mongodb"]) {
    it.each(passwords.slice(0, 7))(`${scheme} raw URL accepts literal %j`, (password) => {
      const database = scheme === "redis" ? "0" : "testdb";
      expect(parseConnectionUrl(`${scheme}://test_user:${password}@db.example.com:5432/${database}`).password).toBe(password);
    });
  }

  for (const db_type of ["mysql", "postgres", "redis", "mongodb", "oracle"] as const) {
    it.each(passwords)(`${db_type} URL copy/import preserves %j`, (password) => {
      const config: ConnectionUrlCopyConfig = { db_type, host: "db.example.com", port: db_type === "redis" ? 6379 : 5432, username: "test_user", password, database: db_type === "redis" ? "0" : "testdb", ssl: false, url_params: "" };
      const url = buildConnectionUrlCopy(config, "urlWithPassword");
      expect(parseConnectionUrl(url).password).toBe(password);
    });
  }
});
