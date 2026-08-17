import { uuid } from "@/lib/common/utils";
import type { DbxGatewayConfig, GatewayEdgeRoutes, GatewayRoute, TransportLayerConfig } from "@/types/database";

export type DbxGatewayProfile = { type: "dbx_gateway" } & DbxGatewayConfig;
export type GatewayRouteOption = GatewayRoute & { disabled: boolean };
export type GatewayRouteGroup = Omit<GatewayEdgeRoutes, "routes"> & { routes: GatewayRouteOption[] };

export interface GatewayLayerRoute {
  id?: string;
  enabled?: boolean;
  edge_id?: string;
  target_id?: string;
  use_as_connection_info?: boolean;
}

export function createDbxGatewayProfile(): DbxGatewayProfile {
  return {
    type: "dbx_gateway",
    id: uuid(),
    name: "",
    enabled: true,
    main_url: "",
    identity_id: "",
    server_ca_pem: "",
    server_spki_sha256: "",
    connect_timeout_secs: 10,
    edge_id: "",
    target_id: "",
  };
}

export function validateDbxGatewayProfile(profile: DbxGatewayProfile): string | null {
  let url: URL;
  try {
    url = new URL(profile.main_url.trim());
  } catch {
    return "Main URL must be a valid wss:// URL.";
  }
  if (url.protocol !== "wss:") return "Main URL must use wss://.";
  if (!url.hostname) return "Main URL must include a hostname.";
  if (!profile.identity_id.trim()) return "A client identity is required.";
  if (!profile.server_ca_pem.trim() && !profile.server_spki_sha256.trim()) return "A dedicated CA or SPKI pin is required.";
  if (profile.server_spki_sha256.trim() && !/^[a-fA-F0-9]{64}$/.test(profile.server_spki_sha256.trim())) {
    return "The SPKI pin must contain 64 hexadecimal characters.";
  }
  if (typeof profile.connect_timeout_secs !== "number" || !Number.isFinite(profile.connect_timeout_secs) || profile.connect_timeout_secs <= 0) {
    return "Connection timeout must be greater than zero.";
  }
  if (profile.edge_id || profile.target_id) return "Gateway profiles cannot contain a database route.";
  return null;
}

export function gatewayProfileReferenceLayer(profile: DbxGatewayProfile, previous: GatewayLayerRoute = {}): DbxGatewayProfile {
  return {
    ...createDbxGatewayProfile(),
    id: previous.id || uuid(),
    name: profile.name || "",
    enabled: previous.enabled !== false,
    profile_id: profile.id,
    edge_id: previous.edge_id || "",
    target_id: previous.target_id || "",
    use_as_connection_info: previous.use_as_connection_info === true,
  };
}

export function groupGatewayRoutes(edges: GatewayEdgeRoutes[]): GatewayRouteGroup[] {
  const grouped = new Map<string, { online: boolean; routes: Map<string, GatewayRoute> }>();
  for (const edge of edges) {
    const current = grouped.get(edge.edge_id) || { online: false, routes: new Map<string, GatewayRoute>() };
    current.online ||= edge.online;
    for (const route of edge.routes) current.routes.set(route.target_id, route);
    grouped.set(edge.edge_id, current);
  }

  return [...grouped.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([edge_id, edge]) => ({
      edge_id,
      online: edge.online,
      routes: [...edge.routes.values()].sort((left, right) => left.display_name.localeCompare(right.display_name) || left.target_id.localeCompare(right.target_id)).map((route) => ({ ...route, disabled: !edge.online })),
    }));
}

export function isDbxGatewayProfile(profile: TransportLayerConfig | undefined): profile is DbxGatewayProfile {
  return profile?.type === "dbx_gateway";
}
