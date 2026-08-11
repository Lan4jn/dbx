import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const managerSource = readFileSync(new URL("../../../components/connection/TunnelProfileManager.vue", import.meta.url), "utf8");

describe("DBX Gateway tunnel profile UI", () => {
  it("configures Main and TLS identity without connection routes", () => {
    expect(managerSource).toContain("addProfile('dbx_gateway')");
    expect(managerSource).toContain("selectedGateway.main_url");
    expect(managerSource).toContain("selectedGateway.identity_id");
    expect(managerSource).toContain("selectedGateway.server_ca_pem");
    expect(managerSource).toContain("selectedGateway.server_spki_sha256");
    expect(managerSource).toContain("selectedGateway.connect_timeout_secs");

    const gatewayFields = managerSource.match(/<template v-else-if="selectedGateway">([\s\S]*?)<\/template>/)?.[1] ?? "";
    expect(gatewayFields).not.toContain("edge_id");
    expect(gatewayFields).not.toContain("target_id");
  });

  it("imports and deletes desktop identities and tests Main without a route", () => {
    expect(managerSource).toMatch(/filters:\s*\[\{[\s\S]*extensions:\s*\["p12", "pfx"\]/);
    expect(managerSource).toContain("api.importGatewayIdentity");
    expect(managerSource).toContain("api.deleteGatewayIdentity");
    expect(managerSource).toContain("api.testGatewayProfile");
    expect(managerSource).toContain('gatewayIdentityPassword.value = ""');
    expect(managerSource).toContain("gatewayIdentityReferenceCount");
    expect(managerSource).toContain("settings.tunnelsGatewayDesktopOnly");
    expect(managerSource).toContain(':disabled="!isDesktop');
  });

  it("uses the prescribed Gateway icons", () => {
    expect(managerSource).toContain("Network");
    expect(managerSource).toContain("ShieldCheck");
    expect(managerSource).toContain("Upload");
    expect(managerSource).toContain("Trash2");
  });
});
