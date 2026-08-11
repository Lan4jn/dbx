import { ref } from "vue";
import { defineStore } from "pinia";
import * as api from "@/lib/backend/api";
import { groupGatewayRoutes, isDbxGatewayProfile, type GatewayRouteGroup } from "@/lib/connection/gatewayProfiles";
import type { DbxGatewayConfig, GatewayEdgeRoutes, GatewayIdentityMetadata, TunnelProfile } from "@/types/database";

type GatewayBackendApi = {
  listGatewayIdentities: () => Promise<GatewayIdentityMetadata[]>;
  listGatewayRoutes: (profile: { type: "dbx_gateway" } & DbxGatewayConfig) => Promise<GatewayEdgeRoutes[]>;
};

const gatewayApi = api as typeof api & GatewayBackendApi;

/**
 * Shared tunnel profiles (Settings > Tunnels). Connections reference a
 * profile by id via `transport_layers[].profile_id`; the backend resolves
 * the reference at connect time, so profile edits reach every referencing
 * connection without touching the stored connections.
 */
export const useTunnelProfileStore = defineStore("tunnelProfiles", () => {
  const profiles = ref<TunnelProfile[]>([]);
  const gatewayIdentities = ref<GatewayIdentityMetadata[]>([]);
  const gatewayRoutesByProfileId = ref<Record<string, GatewayRouteGroup[]>>({});
  const isLoaded = ref(false);
  const gatewayRouteRequestIds = new Map<string, number>();

  async function init() {
    if (isLoaded.value) return;
    await refresh();
  }

  async function refresh() {
    try {
      profiles.value = (await api.loadTunnelProfiles()) || [];
      isLoaded.value = true;
      await refreshGatewayIdentities();
    } catch {
      // Backend unavailable (e.g. stale web session): keep previous state and
      // retry on the next init/refresh call.
    }
  }

  async function refreshGatewayIdentities() {
    try {
      gatewayIdentities.value = (await gatewayApi.listGatewayIdentities()) || [];
    } catch {
      gatewayIdentities.value = [];
    }
  }

  async function refreshGatewayRoutes(profileId: string): Promise<GatewayRouteGroup[]> {
    const profile = profileById(profileId);
    if (!isDbxGatewayProfile(profile)) {
      delete gatewayRoutesByProfileId.value[profileId];
      return [];
    }

    const requestId = (gatewayRouteRequestIds.get(profileId) || 0) + 1;
    gatewayRouteRequestIds.set(profileId, requestId);
    const profileSnapshot = JSON.stringify(profile);
    const routes = groupGatewayRoutes((await gatewayApi.listGatewayRoutes(profile)) || []);
    const currentProfile = profileById(profileId);
    if (gatewayRouteRequestIds.get(profileId) !== requestId || !isDbxGatewayProfile(currentProfile) || JSON.stringify(currentProfile) !== profileSnapshot) {
      return gatewayRoutesByProfileId.value[profileId] || [];
    }
    gatewayRoutesByProfileId.value = { ...gatewayRoutesByProfileId.value, [profileId]: routes };
    return routes;
  }

  function profileById(id: string | undefined): TunnelProfile | undefined {
    if (!id) return undefined;
    return profiles.value.find((profile) => profile.id === id);
  }

  async function saveProfiles(next: TunnelProfile[]) {
    const previous = profiles.value;
    profiles.value = next;
    try {
      await api.saveTunnelProfiles(next);
    } catch (error) {
      profiles.value = previous;
      throw error;
    }
  }

  /**
   * Tests a profile's transport layer in isolation (no downstream database).
   * The draft profile is passed straight through, so it validates the values
   * currently in the editor rather than the last-saved copy.
   */
  async function testProfile(profile: TunnelProfile): Promise<string> {
    return api.testTunnelProfile(profile);
  }

  return {
    profiles,
    gatewayIdentities,
    gatewayRoutesByProfileId,
    isLoaded,
    init,
    refresh,
    refreshGatewayIdentities,
    refreshGatewayRoutes,
    profileById,
    saveProfiles,
    testProfile,
  };
});
