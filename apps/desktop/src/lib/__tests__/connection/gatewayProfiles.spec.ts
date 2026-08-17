import { describe, expect, it } from "vitest";
import { createDbxGatewayProfile, gatewayProfileReferenceLayer, groupGatewayRoutes, validateDbxGatewayProfile } from "@/lib/connection/gatewayProfiles";

describe("createDbxGatewayProfile", () => {
  it("creates a route-free profile with secure defaults", () => {
    const profile = createDbxGatewayProfile();

    expect(profile.type).toBe("dbx_gateway");
    expect(profile.enabled).toBe(true);
    expect(profile.connect_timeout_secs).toBe(10);
    expect(profile.edge_id).toBe("");
    expect(profile.target_id).toBe("");
  });
});

describe("validateDbxGatewayProfile", () => {
  it("requires a wss Main URL", () => {
    const profile = createDbxGatewayProfile();
    profile.main_url = "https://gateway.example.com/_dbx/client";
    profile.identity_id = "identity-1";
    profile.server_ca_pem = "ca";

    expect(validateDbxGatewayProfile(profile)).toContain("wss://");
  });

  it("requires either a dedicated CA or an SPKI pin", () => {
    const profile = createDbxGatewayProfile();
    profile.main_url = "wss://gateway.example.com/_dbx/client";
    profile.identity_id = "identity-1";

    expect(validateDbxGatewayProfile(profile)).toContain("CA or SPKI");

    profile.server_spki_sha256 = "a".repeat(64);
    expect(validateDbxGatewayProfile(profile)).toBeNull();
  });
});

describe("groupGatewayRoutes", () => {
  it("groups routes by Edge and keeps offline routes disabled", () => {
    const groups = groupGatewayRoutes([
      { edge_id: "edge-b", online: false, routes: [{ target_id: "archive", display_name: "Archive" }] },
      { edge_id: "edge-a", online: true, routes: [{ target_id: "primary", display_name: "Primary" }] },
      { edge_id: "edge-a", online: true, routes: [{ target_id: "replica", display_name: "Replica" }] },
    ]);

    expect(groups.map((group) => group.edge_id)).toEqual(["edge-a", "edge-b"]);
    expect(groups[0].routes.map((route) => route.target_id)).toEqual(["primary", "replica"]);
    expect(groups[0].routes.every((route) => !route.disabled)).toBe(true);
    expect(groups[1].online).toBe(false);
    expect(groups[1].routes[0].disabled).toBe(true);
  });
});

describe("gatewayProfileReferenceLayer", () => {
  it("keeps the route on a gateway profile reference", () => {
    const profile = createDbxGatewayProfile();
    profile.id = "profile-1";
    profile.name = "Production Gateway";
    profile.main_url = "wss://gateway.example.com/_dbx/client";
    profile.identity_id = "identity-1";
    profile.server_ca_pem = "ca";

    const layer = gatewayProfileReferenceLayer(profile, {
      id: "layer-1",
      edge_id: "edge-prod-01",
      target_id: "postgres-primary",
      use_as_connection_info: true,
    });

    expect(layer.profile_id).toBe(profile.id);
    expect(layer.edge_id).toBe("edge-prod-01");
    expect(layer.target_id).toBe("postgres-primary");
    expect(layer.use_as_connection_info).toBe(true);
    expect(layer.main_url).toBe("");
    expect(layer.identity_id).toBe("");
  });
});
