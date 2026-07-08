import { ref, onMounted, onUnmounted } from "vue";
import { isTauriRuntime } from "@/lib/backend/tauriRuntime";
import { isMacOS } from "@/lib/backend/platform";
import * as api from "@/lib/backend/api";

export function shouldReserveMacTrafficLightInset(isMac: boolean, isFullscreen: boolean, isDesktop = true): boolean {
  return isDesktop && isMac && !isFullscreen;
}

export function shouldShowWindowControls(isMac: boolean, isDesktop = true, nativeDecorations = false): boolean {
  return isDesktop && !isMac && !nativeDecorations;
}

export function useWindowControls() {
  const isMaximized = ref(false);
  const isFullscreen = ref(false);
  const nativeDecorations = ref(false);
  const isMac = isMacOS();
  const isDesktop = isTauriRuntime();
  const showControls = ref(shouldShowWindowControls(isMac, isDesktop, nativeDecorations.value));

  let unlisten: (() => void) | null = null;

  async function updateWindowState() {
    if (!isDesktop) return;
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    const currentWindow = getCurrentWindow();
    const [maximized, fullscreen] = await Promise.all([currentWindow.isMaximized(), currentWindow.isFullscreen()]);
    isMaximized.value = maximized;
    isFullscreen.value = fullscreen;
  }

  async function minimize() {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().minimize();
  }

  async function toggleMaximize() {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().toggleMaximize();
    setTimeout(updateWindowState, 50);
  }

  async function close() {
    if (!isDesktop) return;
    await api.requestAppClose();
  }

  onMounted(async () => {
    if (!isDesktop) return;
    try {
      nativeDecorations.value = await api.useNativeWindowDecorations();
      showControls.value = shouldShowWindowControls(isMac, isDesktop, nativeDecorations.value);
    } catch {
      showControls.value = shouldShowWindowControls(isMac, isDesktop, false);
    }
    await updateWindowState();
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    const unlistenFn = await getCurrentWindow().onResized(() => {
      void updateWindowState();
    });
    unlisten = unlistenFn;
  });

  onUnmounted(() => {
    unlisten?.();
  });

  return {
    isMac,
    isDesktop,
    showControls,
    isMaximized,
    isFullscreen,
    minimize,
    toggleMaximize,
    close,
  };
}
