<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import PasswordInput from "@/components/ui/PasswordInput.vue";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { FolderOpen, Loader2, Network, Plus, ShieldCheck, Trash2, Upload } from "@lucide/vue";
import { isTauriRuntime } from "@/lib/backend/tauriRuntime";
import { useToast } from "@/composables/useToast";
import { useTunnelProfileStore } from "@/stores/tunnelProfileStore";
import { createTunnelProfile, createTunnelProfileTestGuard, tunnelProfileSummary, type TunnelProfileType } from "@/lib/connection/tunnelProfiles";
import { applySshConfigHostAliasPrefill as prefillSshConfigHostAlias } from "@/lib/connection/sshConfigHosts";
import * as api from "@/lib/backend/api";
import type { DbxGatewayConfig, GatewayIdentityMetadata, SshConfigHostEntry, TunnelProfile } from "@/types/database";
import { translateBackendError } from "@/i18n/backend-errors";

const { t } = useI18n();
const { toast } = useToast();
const store = useTunnelProfileStore();
const isDesktop = isTauriRuntime();

const draft = ref<TunnelProfile[]>([]);
const selectedId = ref<string | null>(null);
const isSaving = ref(false);
const hasInitializedDraft = ref(false);
const isTesting = ref(false);
const testResult = ref<{ ok: boolean; message: string } | null>(null);
const testGuard = createTunnelProfileTestGuard();
const sshConfigHosts = ref<SshConfigHostEntry[]>([]);
const gatewayIdentities = ref<GatewayIdentityMetadata[]>([]);
const gatewayIdentityName = ref("");
const gatewayIdentityPassword = ref("");
const gatewayIdentityPath = ref("");
const isImportingGatewayIdentity = ref(false);
const isLoadingGatewayIdentities = ref(false);

function cloneProfiles(profiles: TunnelProfile[]): TunnelProfile[] {
  return JSON.parse(JSON.stringify(profiles)) as TunnelProfile[];
}

function resetDraft() {
  draft.value = cloneProfiles(store.profiles);
  if (!draft.value.some((profile) => profile.id === selectedId.value)) {
    selectedId.value = draft.value[0]?.id || null;
  }
}

const isDirty = computed(() => JSON.stringify(draft.value) !== JSON.stringify(store.profiles));

void store.init();
watch(
  () => store.isLoaded,
  (loaded) => {
    // A remounted settings panel starts with an empty draft even when Pinia
    // already contains profiles, so the initial empty state is not a user edit.
    if (loaded && (!hasInitializedDraft.value || !isDirty.value)) {
      resetDraft();
      hasInitializedDraft.value = true;
    }
  },
  { immediate: true },
);

const selected = computed(() => draft.value.find((profile) => profile.id === selectedId.value) || null);
const selectedSsh = computed(() => (selected.value?.type === "ssh" ? selected.value : null));
const selectedProxy = computed(() => (selected.value?.type === "proxy" ? selected.value : null));
const selectedHttp = computed(() => (selected.value?.type === "http_tunnel" ? selected.value : null));
const selectedGateway = computed(() => (selected.value?.type === "dbx_gateway" ? selected.value : null));
const sshConfigHostAliases = computed(() => sshConfigHosts.value.map((entry) => entry.alias));
const canImportGatewayIdentity = computed(() => isDesktop && !!gatewayIdentityPath.value && !!gatewayIdentityPassword.value && !isImportingGatewayIdentity.value);
const gatewayIdentityFileName = computed(() => gatewayIdentityPath.value.split(/[\\/]/).pop() || "");

async function loadSshConfigHosts() {
  try {
    sshConfigHosts.value = await api.listSshConfigHosts();
  } catch {
    sshConfigHosts.value = [];
  }
}

function updateSelectedSshHost(value: string | number) {
  if (!selectedSsh.value) return;
  selectedSsh.value.host = String(value);
  prefillSshConfigHostAlias(selectedSsh.value, sshConfigHosts.value);
}

void loadSshConfigHosts();

async function loadGatewayIdentities() {
  if (!isDesktop) return;
  isLoadingGatewayIdentities.value = true;
  try {
    gatewayIdentities.value = await api.listGatewayIdentities();
  } catch (error) {
    toast(translateBackendError(t, String(error)), 5000);
  } finally {
    isLoadingGatewayIdentities.value = false;
  }
}

void loadGatewayIdentities();

function invalidateProfileTest() {
  testGuard.invalidate();
  isTesting.value = false;
  testResult.value = null;
}

// Profile tests are asynchronous, so any selection or configuration change
// must invalidate the request before it can publish a stale result.
watch(
  [selectedId, selectedSsh, selectedProxy, selectedGateway],
  () => {
    invalidateProfileTest();
  },
  { deep: true },
);

function profileTypeLabel(profile: TunnelProfile): string {
  if (profile.type === "proxy") return "Proxy";
  if (profile.type === "http_tunnel") return t("connection.httpTunnel");
  if (profile.type === "dbx_gateway") return "Gateway";
  return "SSH";
}

function profileDisplayName(profile: TunnelProfile): string {
  return profile.name?.trim() || tunnelProfileSummary(profile) || t("settings.tunnelsUnnamedProfile");
}

function addProfile(type: TunnelProfileType) {
  const profile = createTunnelProfile(type);
  draft.value = [...draft.value, profile];
  selectedId.value = profile.id;
}

function removeSelected() {
  const current = selected.value;
  if (!current) return;
  draft.value = draft.value.filter((profile) => profile.id !== current.id);
  selectedId.value = draft.value[0]?.id || null;
}

function updateSshAuthMethod(value: unknown) {
  const profile = selectedSsh.value;
  if (!profile) return;
  profile.auth_method = value === "key" ? "key" : value === "key+password" ? "key+password" : value === "none" ? "none" : "password";
  if (profile.auth_method !== "password" && profile.auth_method !== "key+password") profile.password = "";
  if (profile.auth_method !== "key" && profile.auth_method !== "key+password") {
    profile.key_path = "";
    profile.key_passphrase = "";
  }
}

function updateProxyType(value: unknown) {
  const profile = selectedProxy.value;
  if (!profile) return;
  profile.proxy_type = value === "http" ? "http" : "socks5";
}

function gatewayIdentityReferenceCount(identityId: string): number {
  return draft.value.filter((profile) => profile.type === "dbx_gateway" && profile.identity_id === identityId).length;
}

async function selectGatewayIdentityFile() {
  if (!isDesktop || isImportingGatewayIdentity.value) return;
  const { open } = await import("@tauri-apps/plugin-dialog");
  const path = await open({
    title: t("settings.tunnelsGatewaySelectIdentity"),
    multiple: false,
    filters: [{ name: "PKCS#12", extensions: ["p12", "pfx"] }],
  });
  if (typeof path === "string") gatewayIdentityPath.value = path;
}

async function selectGatewayIdentityPasswordFile() {
  if (!isDesktop || isImportingGatewayIdentity.value) return;
  try {
    const [{ open }, { readTextFile }] = await Promise.all([import("@tauri-apps/plugin-dialog"), import("@tauri-apps/plugin-fs")]);
    const path = await open({
      title: t("settings.tunnelsGatewaySelectPasswordFile"),
      multiple: false,
    });
    if (typeof path === "string") {
      gatewayIdentityPassword.value = (await readTextFile(path)).replace(/\r?\n$/, "");
    }
  } catch (error) {
    toast(t("settings.tunnelsGatewayPasswordFileFailed", { message: translateBackendError(t, String(error)) }), 5000);
  }
}

async function importGatewayIdentity() {
  if (!canImportGatewayIdentity.value) return;
  isImportingGatewayIdentity.value = true;
  try {
    const identity = await api.importGatewayIdentity(gatewayIdentityPath.value, gatewayIdentityPassword.value, gatewayIdentityName.value.trim());
    gatewayIdentityPassword.value = "";
    gatewayIdentityName.value = "";
    gatewayIdentityPath.value = "";
    await loadGatewayIdentities();
    if (selectedGateway.value) selectedGateway.value.identity_id = identity.id;
    toast(t("settings.tunnelsGatewayIdentityImported"));
  } catch (error) {
    toast(t("settings.tunnelsGatewayIdentityImportFailed", { message: translateBackendError(t, String(error)) }), 5000);
  } finally {
    isImportingGatewayIdentity.value = false;
  }
}

async function deleteGatewayIdentity(identity: GatewayIdentityMetadata) {
  if (!isDesktop) return;
  const references = gatewayIdentityReferenceCount(identity.id);
  const { ask } = await import("@tauri-apps/plugin-dialog");
  const confirmed = await ask(t("settings.tunnelsGatewayDeleteIdentityConfirm", { name: identity.name, count: references }), {
    title: t("settings.tunnelsGatewayDeleteIdentity"),
    kind: "warning",
  });
  if (!confirmed) return;
  try {
    await api.deleteGatewayIdentity(identity.id);
    for (const profile of draft.value) {
      if (profile.type === "dbx_gateway" && profile.identity_id === identity.id) profile.identity_id = "";
    }
    await loadGatewayIdentities();
    toast(t("settings.tunnelsGatewayIdentityDeleted"));
  } catch (error) {
    toast(t("settings.tunnelsGatewayIdentityDeleteFailed", { message: translateBackendError(t, String(error)) }), 5000);
  }
}

async function importGatewayCa(profile: DbxGatewayConfig) {
  if (!isDesktop) return;
  try {
    const [{ open }, { readTextFile }] = await Promise.all([import("@tauri-apps/plugin-dialog"), import("@tauri-apps/plugin-fs")]);
    const path = await open({
      title: t("settings.tunnelsGatewayImportCa"),
      multiple: false,
      filters: [{ name: "PEM", extensions: ["pem", "crt", "cer"] }],
    });
    if (typeof path === "string") profile.server_ca_pem = await readTextFile(path);
  } catch (error) {
    toast(t("settings.tunnelsGatewayCaImportFailed", { message: translateBackendError(t, String(error)) }), 5000);
  }
}

async function browseSshKeyPath(target?: { key_path?: string } | null) {
  if (isTauriRuntime()) {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const selected = await open({
      title: "Select SSH Private Key",
      multiple: false,
    });
    if (selected && typeof selected === "string" && target) {
      target.key_path = selected;
    }
  }
}

async function save() {
  if (isSaving.value) return;
  invalidateProfileTest();
  isSaving.value = true;
  try {
    await store.saveProfiles(cloneProfiles(draft.value));
    toast(t("settings.tunnelsSaved"));
  } catch (error) {
    toast(t("settings.tunnelsSaveFailed", { message: translateBackendError(t, error) }), 5000);
  } finally {
    isSaving.value = false;
  }
}

async function testSelected() {
  const profile = selectedSsh.value || selectedProxy.value || selectedGateway.value;
  if (!profile || isTesting.value) return;
  const profileSnapshot = cloneProfiles([profile])[0];
  const requestId = testGuard.start(profileSnapshot);
  isTesting.value = true;
  testResult.value = null;
  try {
    const message = profileSnapshot.type === "dbx_gateway" ? await api.testGatewayProfile(profileSnapshot) : await store.testProfile(profileSnapshot);
    if (!testGuard.isCurrent(requestId, profile)) return;
    testResult.value = { ok: true, message: message ? t("settings.tunnelsTestSuccess") + ": " + message : t("settings.tunnelsTestSuccess") };
  } catch (error) {
    if (!testGuard.isCurrent(requestId, profile)) return;
    testResult.value = { ok: false, message: t("settings.tunnelsTestFailed", { message: translateBackendError(t, error) }) };
  } finally {
    if (testGuard.isCurrent(requestId, selectedSsh.value || selectedProxy.value || selectedGateway.value)) isTesting.value = false;
  }
}
</script>

<template>
  <div class="flex flex-col gap-4">
    <p class="text-xs text-muted-foreground">{{ t("settings.tunnelsDescription") }}</p>

    <div class="grid min-w-0 gap-2">
      <p v-if="!draft.length" class="rounded-md border border-dashed px-3 py-4 text-center text-xs text-muted-foreground">
        {{ t("settings.tunnelsEmpty") }}
      </p>
      <button
        v-for="profile in draft"
        :key="profile.id"
        type="button"
        class="flex min-h-10 items-center gap-2 rounded-md border px-3 text-left text-xs transition-colors"
        :class="profile.id === selectedId ? 'tunnel-profile-option--selected border-primary bg-primary/5' : 'hover:bg-muted/50'"
        @click="selectedId = profile.id"
      >
        <span class="shrink-0 rounded border bg-muted/40 px-1.5 py-0.5 text-[10px] uppercase text-muted-foreground">{{ profileTypeLabel(profile) }}</span>
        <span class="min-w-0 flex-1 truncate">{{ profileDisplayName(profile) }}</span>
        <span class="min-w-0 truncate text-muted-foreground">{{ tunnelProfileSummary(profile) }}</span>
      </button>
    </div>

    <div class="flex flex-wrap items-center gap-2">
      <Button type="button" variant="outline" size="sm" @click="addProfile('ssh')">
        <Plus class="mr-1.5 h-3.5 w-3.5" />
        {{ t("settings.tunnelsAddSsh") }}
      </Button>
      <Button type="button" variant="outline" size="sm" @click="addProfile('proxy')">
        <Plus class="mr-1.5 h-3.5 w-3.5" />
        {{ t("settings.tunnelsAddProxy") }}
      </Button>
      <Button type="button" variant="outline" size="sm" @click="addProfile('http_tunnel')">
        <Plus class="mr-1.5 h-3.5 w-3.5" />
        {{ t("settings.tunnelsAddHttp") }}
      </Button>
      <Button type="button" variant="outline" size="sm" @click="addProfile('dbx_gateway')">
        <Network class="mr-1.5 h-3.5 w-3.5" />
        {{ t("settings.tunnelsAddGateway") }}
      </Button>
      <Button v-if="selected" type="button" variant="outline" size="sm" @click="removeSelected">
        <Trash2 class="mr-1.5 h-3.5 w-3.5" />
        {{ t("settings.tunnelsDelete") }}
      </Button>
    </div>

    <template v-if="selected">
      <div class="grid grid-cols-4 items-center gap-4">
        <Label class="text-xs">{{ t("settings.tunnelsProfileName") }}</Label>
        <Input v-model="selected.name" class="col-span-3" :placeholder="t('settings.tunnelsProfileNamePlaceholder')" />
      </div>

      <template v-if="selectedSsh">
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.sshHost") }}</Label>
          <Input :model-value="selectedSsh.host" class="col-span-2" list="tunnel-profile-ssh-config-host-aliases" :placeholder="t('connection.sshHostPlaceholder')" @update:model-value="updateSelectedSshHost" />
          <datalist id="tunnel-profile-ssh-config-host-aliases">
            <option v-for="alias in sshConfigHostAliases" :key="alias" :value="alias" />
          </datalist>
          <Input v-model.number="selectedSsh.port" type="number" min="1" max="65535" class="col-span-1" />
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.sshUser") }}</Label>
          <Input v-model="selectedSsh.user" class="col-span-3" placeholder="root" />
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.sshAuthMethod") }}</Label>
          <Select :model-value="selectedSsh.auth_method || 'password'" @update:model-value="updateSshAuthMethod">
            <SelectTrigger class="col-span-3 h-9">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="password">{{ t("connection.sshAuthMethodPassword") }}</SelectItem>
              <SelectItem value="key">{{ t("connection.sshAuthMethodKey") }}</SelectItem>
              <SelectItem value="key+password">{{ t("connection.sshAuthMethodKeyPassword") }}</SelectItem>
              <SelectItem value="none">{{ t("connection.sshAuthMethodNone") }}</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div v-if="!selectedSsh.auth_method || selectedSsh.auth_method === 'password' || selectedSsh.auth_method === 'key+password'" class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.sshPassword") }}</Label>
          <PasswordInput v-model="selectedSsh.password" class="col-span-3" :placeholder="t('connection.sshPasswordPlaceholder')" />
        </div>
        <div v-if="selectedSsh.auth_method === 'key' || selectedSsh.auth_method === 'key+password'" class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.sshKeyPath") }}</Label>
          <div class="col-span-3 flex items-center gap-1">
            <Input v-model="selectedSsh.key_path" class="flex-1" placeholder="~/.ssh/id_rsa" />
            <Tooltip v-if="isTauriRuntime()">
              <TooltipTrigger as-child>
                <Button variant="outline" size="icon" class="h-9 w-9 shrink-0" @click="browseSshKeyPath(selectedSsh)">
                  <FolderOpen class="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>{{ t("connection.sshKeyPathBrowse") }}</TooltipContent>
            </Tooltip>
          </div>
        </div>
        <div v-if="selectedSsh.auth_method === 'key' || selectedSsh.auth_method === 'key+password'" class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.sshKeyPassphrase") }}</Label>
          <PasswordInput v-model="selectedSsh.key_passphrase" class="col-span-3" :placeholder="t('connection.sshKeyPassphrasePlaceholder')" />
        </div>
        <div v-if="selectedSsh.auth_method === 'none'" class="grid grid-cols-4 items-center gap-4">
          <span />
          <p class="col-span-3 text-xs text-muted-foreground">{{ t("connection.sshAuthMethodNoneHint") }}</p>
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <span />
          <label class="col-span-3 flex cursor-pointer items-center gap-2">
            <input v-model="selectedSsh.expose_lan" type="checkbox" class="mr-0" />
            <span class="text-xs text-muted-foreground">{{ t("connection.sshExposeLan") }}</span>
          </label>
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.sshConnectTimeout") }}</Label>
          <Input v-model.number="selectedSsh.connect_timeout_secs" type="number" min="1" max="300" step="1" class="col-span-3" />
        </div>
      </template>

      <template v-else-if="selectedProxy">
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.proxyType") }}</Label>
          <Select :model-value="selectedProxy.proxy_type || 'socks5'" @update:model-value="updateProxyType">
            <SelectTrigger class="col-span-3 h-9">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="socks5">SOCKS5</SelectItem>
              <SelectItem value="http">HTTP CONNECT</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.proxyHost") }}</Label>
          <Input v-model="selectedProxy.host" class="col-span-2" placeholder="127.0.0.1" />
          <Input v-model.number="selectedProxy.port" type="number" class="col-span-1" />
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.proxyUsername") }}</Label>
          <Input v-model="selectedProxy.username" class="col-span-3" :placeholder="t('connection.proxyUsernamePlaceholder')" />
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.proxyPassword") }}</Label>
          <PasswordInput v-model="selectedProxy.password" class="col-span-3" :placeholder="t('connection.proxyPasswordPlaceholder')" />
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.proxyTestTarget") }}</Label>
          <Input v-model="selectedProxy.test_target" class="col-span-3" :placeholder="t('connection.proxyTestTargetPlaceholder')" />
        </div>
      </template>

      <template v-else-if="selectedHttp">
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.httpTunnelUrl") }}</Label>
          <Input v-model="selectedHttp.url" class="col-span-3" placeholder="https://dbx.example.com/dbx_tunnel.php" />
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.httpTunnelToken") }}</Label>
          <PasswordInput v-model="selectedHttp.token" class="col-span-3" :placeholder="t('connection.httpTunnelTokenPlaceholder')" />
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("connection.httpTunnelConnectTimeout") }}</Label>
          <Input v-model.number="selectedHttp.connect_timeout_secs" type="number" min="1" max="300" step="1" class="col-span-3" />
        </div>
      </template>

      <template v-else-if="selectedGateway">
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("settings.tunnelsGatewayMainUrl") }}</Label>
          <Input v-model="selectedGateway.main_url" class="col-span-3" placeholder="wss://gateway.example.com/dbx" />
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("settings.tunnelsGatewayIdentity") }}</Label>
          <Select v-model="selectedGateway.identity_id" :disabled="!isDesktop || isLoadingGatewayIdentities">
            <SelectTrigger class="col-span-3 h-9 min-w-0">
              <SelectValue :placeholder="t('settings.tunnelsGatewayIdentityPlaceholder')" />
            </SelectTrigger>
            <SelectContent>
              <SelectItem v-for="identity in gatewayIdentities" :key="identity.id" :value="identity.id"> {{ identity.name }} · {{ identity.subject }} </SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div class="grid grid-cols-4 items-start gap-4">
          <Label class="pt-2 text-xs">{{ t("settings.tunnelsGatewayIdentityImport") }}</Label>
          <div class="col-span-3 grid min-w-0 gap-2">
            <div class="grid min-w-0 gap-2 sm:grid-cols-2">
              <Input v-model="gatewayIdentityName" :placeholder="t('settings.tunnelsGatewayIdentityNamePlaceholder')" :disabled="!isDesktop || isImportingGatewayIdentity" />
              <div class="flex min-w-0 items-center gap-1">
                <PasswordInput v-model="gatewayIdentityPassword" class="min-w-0 flex-1" :placeholder="t('settings.tunnelsGatewayIdentityPasswordPlaceholder')" :disabled="!isDesktop || isImportingGatewayIdentity" />
                <Tooltip>
                  <TooltipTrigger as-child>
                    <Button type="button" variant="outline" size="icon" class="h-9 w-9 shrink-0" :disabled="!isDesktop || isImportingGatewayIdentity" @click="selectGatewayIdentityPasswordFile">
                      <FolderOpen class="h-4 w-4" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>{{ t("settings.tunnelsGatewaySelectPasswordFile") }}</TooltipContent>
                </Tooltip>
              </div>
            </div>
            <div class="flex min-w-0 items-center gap-2">
              <Button type="button" variant="outline" size="sm" class="shrink-0" :disabled="!isDesktop || isImportingGatewayIdentity" @click="selectGatewayIdentityFile">
                <FolderOpen class="mr-1.5 h-3.5 w-3.5" />
                {{ t("settings.tunnelsGatewaySelectIdentity") }}
              </Button>
              <span class="min-w-0 flex-1 truncate text-xs text-muted-foreground" :title="gatewayIdentityPath">{{ gatewayIdentityFileName || t("settings.tunnelsGatewayNoIdentitySelected") }}</span>
              <Button type="button" size="sm" class="shrink-0" :disabled="!canImportGatewayIdentity" @click="importGatewayIdentity">
                <Loader2 v-if="isImportingGatewayIdentity" class="mr-1.5 h-3.5 w-3.5 animate-spin" />
                <Upload v-else class="mr-1.5 h-3.5 w-3.5" />
                {{ t("settings.tunnelsGatewayImportIdentity") }}
              </Button>
            </div>
            <span v-if="!isDesktop" class="text-xs text-muted-foreground">{{ t("settings.tunnelsGatewayDesktopOnly") }}</span>
            <div v-for="identity in gatewayIdentities" :key="identity.id" class="flex min-w-0 items-center gap-2 border-t py-2 text-xs">
              <ShieldCheck class="h-4 w-4 shrink-0 text-emerald-600" />
              <span class="min-w-0 flex-1 truncate" :title="`${identity.subject} · ${identity.fingerprint_sha256}`">{{ identity.name }} · {{ identity.subject }}</span>
              <span class="shrink-0 text-muted-foreground">{{ t("settings.tunnelsGatewayIdentityReferences", { count: gatewayIdentityReferenceCount(identity.id) }) }}</span>
              <Tooltip>
                <TooltipTrigger as-child>
                  <Button type="button" variant="ghost" size="icon" class="h-8 w-8 shrink-0" @click="deleteGatewayIdentity(identity)">
                    <Trash2 class="h-3.5 w-3.5" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>{{ t("settings.tunnelsGatewayDeleteIdentity") }}</TooltipContent>
              </Tooltip>
            </div>
          </div>
        </div>
        <div class="grid grid-cols-4 items-start gap-4">
          <Label class="pt-2 text-xs">{{ t("settings.tunnelsGatewayServerCa") }}</Label>
          <div class="col-span-3 grid min-w-0 gap-2">
            <textarea v-model="selectedGateway.server_ca_pem" rows="4" class="flex w-full resize-y rounded-md border border-input bg-transparent px-3 py-2 text-xs shadow-sm outline-none focus-visible:ring-1 focus-visible:ring-ring" :placeholder="t('settings.tunnelsGatewayServerCaPlaceholder')" />
            <Button type="button" variant="outline" size="sm" class="w-fit" :disabled="!isDesktop" @click="importGatewayCa(selectedGateway)">
              <Upload class="mr-1.5 h-3.5 w-3.5" />
              {{ t("settings.tunnelsGatewayImportCa") }}
            </Button>
          </div>
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("settings.tunnelsGatewaySpkiPin") }}</Label>
          <Input v-model="selectedGateway.server_spki_sha256" class="col-span-3 font-mono text-xs" :placeholder="t('settings.tunnelsGatewaySpkiPinPlaceholder')" />
        </div>
        <div class="grid grid-cols-4 items-center gap-4">
          <Label class="text-xs">{{ t("settings.tunnelsGatewayTimeout") }}</Label>
          <Input v-model.number="selectedGateway.connect_timeout_secs" type="number" min="1" max="300" step="1" class="col-span-3" />
        </div>
      </template>
    </template>

    <div class="flex flex-wrap items-center gap-2">
      <Button type="button" size="sm" :disabled="!isDirty || isSaving" @click="save">
        <Loader2 v-if="isSaving" class="mr-1.5 h-3.5 w-3.5 animate-spin" />
        {{ t("settings.tunnelsSave") }}
      </Button>
      <Button type="button" variant="outline" size="sm" :disabled="!isDirty || isSaving" @click="resetDraft">
        {{ t("settings.tunnelsReset") }}
      </Button>
      <Button
        v-if="selectedSsh || selectedProxy || selectedGateway"
        type="button"
        variant="outline"
        size="sm"
        :disabled="isTesting || isSaving || (!selectedSsh?.host?.trim() && !selectedProxy?.host?.trim() && (!selectedGateway?.main_url?.trim() || !selectedGateway?.identity_id))"
        @click="testSelected"
      >
        <Loader2 v-if="isTesting" class="mr-1.5 h-3.5 w-3.5 animate-spin" />
        <ShieldCheck v-else-if="selectedGateway" class="mr-1.5 h-3.5 w-3.5" />
        {{ isTesting ? t("settings.tunnelsTesting") : t("settings.tunnelsTest") }}
      </Button>
      <p v-if="isDirty" class="text-xs text-muted-foreground">{{ t("settings.tunnelsUnsavedHint") }}</p>
    </div>

    <p v-if="testResult" class="text-xs" :class="testResult.ok ? 'text-emerald-600 dark:text-emerald-400' : 'text-red-500'">
      {{ testResult.message }}
    </p>
  </div>
</template>

<style>
html.dbx-legacy-webview .tunnel-profile-option--selected {
  color: var(--foreground) !important;
  border-color: var(--ring) !important;
  background-color: var(--muted) !important;
  box-shadow: inset 0 0 0 1px var(--border);
}

html.dbx-legacy-webview .tunnel-profile-option--selected:hover {
  background-color: var(--accent) !important;
}

html.dbx-legacy-webview .tunnel-profile-option--selected .text-muted-foreground {
  color: var(--muted-foreground) !important;
}
</style>
