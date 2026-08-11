import { afterEach, describe, expect, it, vi } from "vitest";
import type { DbxGatewayConfig } from "@/types/database";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));

const profile: DbxGatewayConfig = {
  id: "gateway-1",
  main_url: "wss://gateway.example.com/dbx",
  identity_id: "identity-1",
  server_ca_pem: "test-ca",
  server_spki_sha256: "a".repeat(64),
  edge_id: "edge-1",
  target_id: "target-1",
};

afterEach(() => {
  mocks.invoke.mockReset();
  vi.unstubAllGlobals();
  vi.resetModules();
});

describe("desktop DBX Gateway API", () => {
  it("maps identity and route operations to Tauri commands", async () => {
    mocks.invoke.mockResolvedValue(undefined);
    const backend = await import("@/lib/backend/tauri");

    await backend.importGatewayIdentity("/tmp/client.p12", "secret", "Client");
    await backend.listGatewayIdentities();
    await backend.deleteGatewayIdentity("identity-1");
    await backend.listGatewayRoutes(profile);
    await backend.testGatewayProfile(profile);

    expect(mocks.invoke.mock.calls).toEqual([
      ["import_gateway_identity", { path: "/tmp/client.p12", password: "secret", name: "Client" }],
      ["list_gateway_identities"],
      ["delete_gateway_identity", { identityId: "identity-1" }],
      ["list_gateway_routes", { profile }],
      ["test_gateway_profile", { profile }],
    ]);
  });
});

describe("web DBX Gateway API", () => {
  it("rejects desktop identity and route operations without making HTTP requests", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    const backend = await import("@/lib/backend/http");
    const expected = "DBX Gateway identities are only available in the desktop app.";

    await expect(backend.importGatewayIdentity("/tmp/client.p12", "secret", "Client")).rejects.toThrow(expected);
    await expect(backend.listGatewayIdentities()).rejects.toThrow(expected);
    await expect(backend.deleteGatewayIdentity("identity-1")).rejects.toThrow(expected);
    await expect(backend.listGatewayRoutes(profile)).rejects.toThrow(expected);
    await expect(backend.testGatewayProfile(profile)).rejects.toThrow(expected);
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
