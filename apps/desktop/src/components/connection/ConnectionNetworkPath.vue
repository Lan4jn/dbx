<script lang="ts">
import type { TransportLayerConfig, TunnelProfile } from "@/types/database";

export type ConnectionNetworkPathPhase = "idle" | "testing" | "success" | "failure";
export type ConnectionNetworkPathStatus = "idle" | "testing" | "success" | "failure";
export type ConnectionNetworkPathNodeKind = "client" | "ssh" | "proxy" | "http-tunnel" | "gateway-main" | "gateway-edge" | "target";

export interface ConnectionNetworkPathInput {
  layers: TransportLayerConfig[];
  profiles: TunnelProfile[];
  host: string;
  port: number;
  database?: string;
  gatewayRouteLabel: string;
  phase: ConnectionNetworkPathPhase;
  errorMessage: string;
}

export interface ConnectionNetworkPathNode {
  key: string;
  kind: ConnectionNetworkPathNodeKind;
  label: string;
  detail: string;
  status: ConnectionNetworkPathStatus;
  sourceLayerIndex?: number;
}

function resolvedLayer(layer: TransportLayerConfig, profiles: TunnelProfile[]): TransportLayerConfig {
  if (!layer.profile_id) return layer;
  const profile = profiles.find((candidate) => candidate.id === layer.profile_id && candidate.type === layer.type);
  if (!profile) return layer;
  if (layer.type === "dbx_gateway" && profile.type === "dbx_gateway") {
    return { ...profile, id: layer.id, enabled: layer.enabled, profile_id: layer.profile_id, edge_id: layer.edge_id, target_id: layer.target_id, use_as_connection_info: layer.use_as_connection_info };
  }
  return { ...profile, id: layer.id, enabled: layer.enabled, profile_id: layer.profile_id } as TransportLayerConfig;
}

function endpointLabel(host: string, port: number): string {
  const trimmed = host.trim();
  return trimmed ? `${trimmed}${port > 0 ? `:${port}` : ""}` : "Database";
}

function gatewayMainDetail(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

function failedNodeIndex(nodes: ConnectionNetworkPathNode[], errorMessage: string): number {
  const normalized = errorMessage.replaceAll("_", "").toLowerCase();
  if (normalized.includes("edgeoffline")) {
    const edge = nodes.findIndex((node) => node.kind === "gateway-edge");
    if (edge >= 0) return edge;
  }
  if (normalized.includes("routedenied") || normalized.includes("targetunavailable")) {
    return nodes.findIndex((node) => node.kind === "target");
  }

  const layerFailure = errorMessage.match(/(?:SSH|Proxy|HTTP tunnel|DBX Gateway) layer (\d+) failed/i);
  if (layerFailure) {
    const sourceLayerIndex = Number(layerFailure[1]) - 1;
    const layerNode = nodes.findIndex((node) => node.sourceLayerIndex === sourceLayerIndex);
    if (layerNode >= 0) return layerNode;
  }

  return nodes.findIndex((node) => node.kind === "target");
}

export function buildConnectionNetworkPath(input: ConnectionNetworkPathInput): ConnectionNetworkPathNode[] {
  const nodes: ConnectionNetworkPathNode[] = [{ key: "client", kind: "client", label: "DBX", detail: "DBX", status: "idle" }];
  const layers = input.layers.filter((layer) => layer.enabled !== false && (layer.type !== "dbx_gateway" || layer.use_as_connection_info !== false)).map((layer) => resolvedLayer(layer, input.profiles));
  let gatewayTargetId = "";

  layers.forEach((layer, sourceLayerIndex) => {
    if (layer.type === "ssh") {
      nodes.push({
        key: layer.id,
        kind: "ssh",
        label: layer.name?.trim() || "SSH",
        detail: endpointLabel(layer.host, layer.port),
        status: "idle",
        sourceLayerIndex,
      });
    } else if (layer.type === "proxy") {
      nodes.push({
        key: layer.id,
        kind: "proxy",
        label: layer.name?.trim() || (layer.proxy_type === "http" ? "HTTP Proxy" : "SOCKS5"),
        detail: endpointLabel(layer.host, layer.port),
        status: "idle",
        sourceLayerIndex,
      });
    } else if (layer.type === "http_tunnel") {
      nodes.push({ key: layer.id, kind: "http-tunnel", label: layer.name?.trim() || "HTTP Tunnel", detail: layer.url, status: "idle", sourceLayerIndex });
    } else {
      gatewayTargetId = layer.target_id;
      nodes.push({
        key: `${layer.id}-main`,
        kind: "gateway-main",
        label: "Gateway Main",
        detail: gatewayMainDetail(layer.main_url),
        status: "idle",
        sourceLayerIndex,
      });
      nodes.push({
        key: `${layer.id}-edge`,
        kind: "gateway-edge",
        label: layer.edge_id || "Gateway Edge",
        detail: layer.edge_id,
        status: "idle",
      });
    }
  });

  nodes.push({
    key: "target",
    kind: "target",
    label: input.gatewayRouteLabel.trim() || input.database?.trim() || endpointLabel(input.host, input.port),
    detail: gatewayTargetId || endpointLabel(input.host, input.port),
    status: "idle",
  });

  if (input.phase === "testing" || input.phase === "success") {
    return nodes.map((node) => ({ ...node, status: input.phase }));
  }
  if (input.phase !== "failure") return nodes;

  const failedIndex = failedNodeIndex(nodes, input.errorMessage);
  return nodes.map((node, index) => ({ ...node, status: index < failedIndex ? "success" : index === failedIndex ? "failure" : "idle" }));
}
</script>

<script setup lang="ts">
import type { Component } from "vue";
import { computed } from "vue";
import { Check, ChevronRight, Circle, Database, Globe2, KeyRound, LoaderCircle, Monitor, Network, Route, Server, X } from "@lucide/vue";
import { useI18n } from "vue-i18n";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

const props = defineProps<{
  layers: TransportLayerConfig[];
  profiles: TunnelProfile[];
  host: string;
  port: number;
  database?: string;
  gatewayRouteLabel?: string;
  phase?: ConnectionNetworkPathPhase;
  errorMessage?: string;
}>();

const { t } = useI18n();
const nodes = computed(() =>
  buildConnectionNetworkPath({
    layers: props.layers,
    profiles: props.profiles,
    host: props.host,
    port: props.port,
    database: props.database,
    gatewayRouteLabel: props.gatewayRouteLabel || "",
    phase: props.phase || "idle",
    errorMessage: props.errorMessage || "",
  }),
);

const kindIcons: Record<ConnectionNetworkPathNodeKind, Component> = {
  client: Monitor,
  ssh: KeyRound,
  proxy: Route,
  "http-tunnel": Globe2,
  "gateway-main": Server,
  "gateway-edge": Network,
  target: Database,
};

const statusIcons: Record<ConnectionNetworkPathStatus, Component> = {
  idle: Circle,
  testing: LoaderCircle,
  success: Check,
  failure: X,
};

function nodeLabel(node: ConnectionNetworkPathNode): string {
  if (node.kind === "client") return t("connection.networkPathClient");
  if (node.kind === "gateway-main") return t("connection.networkPathGatewayMain");
  if (node.kind === "gateway-edge" && !node.detail) return t("connection.networkPathGatewayEdge");
  if (node.kind === "target" && !props.gatewayRouteLabel && !props.database?.trim() && !props.host.trim()) return t("connection.networkPathDatabase");
  return node.label;
}

function statusLabel(status: ConnectionNetworkPathStatus): string {
  return t(`connection.networkPathStatus.${status}`);
}

function nodeClass(status: ConnectionNetworkPathStatus): string {
  if (status === "success") return "border-green-500 bg-green-50 text-green-700 dark:bg-green-950/30 dark:text-green-300";
  if (status === "failure") return "border-red-500 bg-red-50 text-red-700 dark:bg-red-950/30 dark:text-red-300";
  if (status === "testing") return "border-primary bg-primary/5 text-primary";
  return "border-border bg-background text-muted-foreground";
}
</script>

<template>
  <div class="connection-network-path w-full overflow-x-auto" :aria-label="t('connection.networkPath')">
    <div class="flex min-w-max items-start px-1 py-2" role="list">
      <template v-for="(node, index) in nodes" :key="node.key">
        <ChevronRight v-if="index" class="mt-3 h-4 w-5 shrink-0 text-muted-foreground/60" aria-hidden="true" />
        <Tooltip>
          <TooltipTrigger as-child>
            <div class="flex w-[76px] shrink-0 flex-col items-center gap-1 text-center" role="listitem">
              <span class="relative flex h-8 w-8 shrink-0 items-center justify-center rounded-full border transition-colors" :class="nodeClass(node.status)">
                <component :is="kindIcons[node.kind]" class="h-4 w-4" aria-hidden="true" />
                <span class="absolute -right-1 -bottom-1 flex h-4 w-4 items-center justify-center rounded-full border bg-background" :class="nodeClass(node.status)">
                  <component :is="statusIcons[node.status]" class="h-2.5 w-2.5" :class="node.status === 'testing' ? 'animate-spin' : ''" aria-hidden="true" />
                </span>
              </span>
              <span class="w-full truncate text-[11px] leading-4" :title="nodeLabel(node)">{{ nodeLabel(node) }}</span>
            </div>
          </TooltipTrigger>
          <TooltipContent>
            <p class="font-medium">{{ nodeLabel(node) }}</p>
            <p v-if="node.detail && node.detail !== nodeLabel(node)" class="max-w-64 break-all text-xs opacity-80">{{ node.detail }}</p>
            <p class="text-xs opacity-80">{{ statusLabel(node.status) }}</p>
          </TooltipContent>
        </Tooltip>
      </template>
    </div>
  </div>
</template>
