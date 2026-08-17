import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const dialogSource = readFileSync(new URL("../../../components/connection/ConnectionDialog.vue", import.meta.url), "utf8");

describe("DBX Gateway connection editor", () => {
  it("adds at most one Gateway as the final transport layer", () => {
    expect(dialogSource).toContain("function addGatewayTunnel()");
    expect(dialogSource).toContain('type: "dbx_gateway"');
    expect(dialogSource).toContain("hasGatewayLayer");
    expect(dialogSource).toContain("gateway must be the final transport layer");
    expect(dialogSource).toContain("gateway layers are normalized to the end");
    for (const functionName of ["addSshTunnel", "addProxyTunnel", "duplicateTransportLayer"]) {
      const body = dialogSource.match(new RegExp(`function ${functionName}\\([\\s\\S]*?\\n}`))?.[0] ?? "";
      expect(body).toContain("normalizeGatewayLayerOrder");
    }
  });

  it("selects only a shared Gateway profile and stores only its logical route", () => {
    expect(dialogSource).toContain("gatewayTunnelProfiles");
    expect(dialogSource).toContain("selectedTransportLayer.value?.profile_id");
    expect(dialogSource).toContain("connectionInfoGatewayLayer.value.edge_id");
    expect(dialogSource).toContain("connectionInfoGatewayLayer.value.target_id");
    expect(dialogSource).not.toContain("selectedGatewayLayer.main_url");
    expect(dialogSource).not.toContain("selectedGatewayLayer.identity_id");
  });

  it("shows searchable grouped routes and disables offline Edges", () => {
    expect(dialogSource).toContain("gatewayRouteSearch");
    expect(dialogSource).toContain("filteredGatewayRouteGroups");
    expect(dialogSource).toContain("api.listGatewayRoutes");
    expect(dialogSource).toContain(':disabled="!edge.online"');
    expect(dialogSource).toContain("connection.gatewayEdgeOffline");
    expect(dialogSource).toContain("RefreshCw");
    expect(dialogSource).toContain('class="h-9 w-9 shrink-0"');
  });

  it("fails closed for missing profile, identity, route, Main, or offline Edge", () => {
    expect(dialogSource).toContain("connection.gatewayProfileRequired");
    expect(dialogSource).toContain("connection.gatewayIdentityMissing");
    expect(dialogSource).toContain("connection.gatewayRouteRequired");
    expect(dialogSource).toContain("connection.gatewayEdgeOffline");
    expect(dialogSource).toContain("gatewayRoutesError");
  });

  it("lets a Gateway route replace the connection host without duplicating source controls", () => {
    expect(dialogSource).toContain("use_as_connection_info");
    expect(dialogSource).toContain("gatewayAsConnectionInfo");
    expect(dialogSource).toContain("connection.gatewayUseAsConnectionInfo");
    expect(dialogSource).toContain("connection.gatewayUseAsConnectionInfoHint");
    expect(dialogSource).toContain("connection.gatewayHostOverrideHint");
    expect(dialogSource).toContain("connection.gatewayRouteManagedInConnectionInfo");
    expect(dialogSource).not.toContain("gatewayTargetSource");
  });

  it("makes the connection host picker authoritative while Gateway owns the target", () => {
    expect(dialogSource).toContain(':disabled="gatewayAsConnectionInfo || !selectedLayerProfileId || isLoadingGatewayRoutes"');
    expect(dialogSource).toContain('v-if="gatewayAsConnectionInfo" data-gateway-connection-host');
    expect(dialogSource).toContain('@click="selectConnectionInfoGatewayRoute(edge, route.target_id)"');
  });

  it("allows Gateway target mode before choosing a route and accepts the logical route as the connection target", () => {
    expect(dialogSource).not.toContain(':disabled="!selectedGatewayLayer.edge_id || !selectedGatewayLayer.target_id"');
    expect(dialogSource).toContain("if (gatewayAsConnectionInfo.value) return !!connectionInfoGatewayLayer.value?.edge_id && !!connectionInfoGatewayLayer.value?.target_id;");
  });
});
