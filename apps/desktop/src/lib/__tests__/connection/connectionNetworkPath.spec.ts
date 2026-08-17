import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { buildConnectionNetworkPath, type ConnectionNetworkPathInput } from "../../../components/connection/ConnectionNetworkPath.vue";

const dialogSource = readFileSync(new URL("../../../components/connection/ConnectionDialog.vue", import.meta.url), "utf8");

const input = (overrides: Partial<ConnectionNetworkPathInput> = {}): ConnectionNetworkPathInput => ({
  layers: [
    { type: "ssh", id: "ssh-1", name: "Bastion", enabled: true, host: "ssh.internal", port: 22, user: "dbx" },
    { type: "proxy", id: "proxy-1", name: "Office proxy", enabled: true, proxy_type: "socks5", host: "proxy.internal", port: 1080 },
    {
      type: "dbx_gateway",
      id: "gateway-1",
      name: "Production Gateway",
      enabled: true,
      main_url: "wss://main.internal:8443/dbx",
      identity_id: "identity-1",
      server_ca_pem: "",
      server_spki_sha256: "a".repeat(64),
      edge_id: "edge-shanghai",
      target_id: "orders-primary",
    },
  ],
  profiles: [],
  host: "orders.internal",
  port: 5432,
  database: "orders",
  gatewayRouteLabel: "Orders Primary",
  phase: "idle",
  errorMessage: "",
  ...overrides,
});

describe("connection network path", () => {
  it("builds enabled transports followed by Gateway Main, Edge, and logical target", () => {
    const nodes = buildConnectionNetworkPath(input());

    expect(nodes.map((node) => node.kind)).toEqual(["client", "ssh", "proxy", "gateway-main", "gateway-edge", "target"]);
    expect(nodes.map((node) => node.label)).toEqual(["DBX", "Bastion", "Office proxy", "Gateway Main", "edge-shanghai", "Orders Primary"]);
  });

  it("resolves shared profile labels and excludes disabled layers", () => {
    const nodes = buildConnectionNetworkPath(
      input({
        layers: [
          { type: "ssh", id: "ssh-ref", enabled: true, profile_id: "shared-ssh", host: "", port: 0, user: "" },
          { type: "http_tunnel", id: "http-off", name: "Disabled", enabled: false, url: "https://tunnel.invalid" },
        ],
        profiles: [{ type: "ssh", id: "shared-ssh", name: "Shared SSH", host: "bastion.internal", port: 22, user: "dbx" }],
        gatewayRouteLabel: "",
      }),
    );

    expect(nodes.map((node) => node.label)).toEqual(["DBX", "Shared SSH", "orders"]);
  });

  it("uses the manual host path when Gateway is not used as connection information", () => {
    const layers = input().layers.map((layer) => (layer.type === "dbx_gateway" ? { ...layer, use_as_connection_info: false } : layer));

    const nodes = buildConnectionNetworkPath(input({ layers, gatewayRouteLabel: "" }));

    expect(nodes.map((node) => node.kind)).toEqual(["client", "ssh", "proxy", "target"]);
    expect(nodes.at(-1)?.detail).toBe("orders.internal:5432");
  });

  it.each([
    ["idle", ["idle", "idle", "idle", "idle", "idle", "idle"]],
    ["testing", ["testing", "testing", "testing", "testing", "testing", "testing"]],
    ["success", ["success", "success", "success", "success", "success", "success"]],
  ] as const)("applies the %s state without changing node count", (phase, expected) => {
    expect(buildConnectionNetworkPath(input({ phase })).map((node) => node.status)).toEqual(expected);
  });

  it.each([
    ["SSH layer 1 failed: authentication rejected", ["success", "failure", "idle", "idle", "idle", "idle"]],
    ["Proxy layer 2 failed: connection refused", ["success", "success", "failure", "idle", "idle", "idle"]],
    ["DBX Gateway layer 3 failed: Gateway route failed: EdgeOffline", ["success", "success", "success", "success", "failure", "idle"]],
    ["DBX Gateway layer 3 failed: Gateway route failed: RouteDenied", ["success", "success", "success", "success", "success", "failure"]],
    ["DBX Gateway layer 3 failed: Gateway route failed: target_unavailable", ["success", "success", "success", "success", "success", "failure"]],
    ["DBX Gateway layer 3 failed: Gateway TLS handshake failed", ["success", "success", "success", "failure", "idle", "idle"]],
    ["database authentication failed", ["success", "success", "success", "success", "success", "failure"]],
  ])("locates the failing node for %s", (message, expected) => {
    expect(buildConnectionNetworkPath(input({ phase: "failure", errorMessage: message })).map((node) => node.status)).toEqual(expected);
  });

  it("renders above the connection tabs and receives live test state", () => {
    expect(dialogSource).toContain('import ConnectionNetworkPath from "@/components/connection/ConnectionNetworkPath.vue"');
    expect(dialogSource.indexOf("<ConnectionNetworkPath")).toBeGreaterThan(0);
    expect(dialogSource.indexOf("<ConnectionNetworkPath")).toBeLessThan(dialogSource.indexOf("<TabsList>"));
    expect(dialogSource).toContain(':phase="networkPathPhase"');
    expect(dialogSource).toContain(":error-message=\"testResult?.message || ''\"");
  });

  it("clears connectivity state when the represented configuration changes", () => {
    expect(dialogSource).toContain("const networkPathFingerprint = computed");
    const watcher = dialogSource.match(/watch\(networkPathFingerprint,[\s\S]*?\n\);/)?.[0] || "";
    expect(watcher).toContain("resetTestState()");
  });
});
