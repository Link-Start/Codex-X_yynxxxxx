import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";

import type { AppTab } from "../components/AppShell";
import type {
  Lang,
  SkinActionResult,
  SkinCenterState,
  SkinExportResult,
} from "../types";

export type SkinRestartRequest = {
  themeId?: string;
  themeName?: string;
};

function exportFileName(value: string) {
  const normalized = value
    .trim()
    .replace(/[<>:"/\\|?*\u0000-\u001f]/g, "-")
    .replace(/[. ]+$/g, "")
    .slice(0, 80);
  return `${normalized || "codex-x-theme"}.zip`;
}

type UseSkinCenterOptions = {
  enabled: boolean;
  lang: Lang;
  tab: AppTab;
  ready: boolean;
  setActionBusy: React.Dispatch<React.SetStateAction<string>>;
  setError: React.Dispatch<React.SetStateAction<string>>;
  setToast: React.Dispatch<React.SetStateAction<string>>;
};

export function useSkinCenter({
  enabled,
  lang,
  tab,
  ready,
  setActionBusy,
  setError,
  setToast,
}: UseSkinCenterOptions) {
  const [state, setState] = React.useState<SkinCenterState | null>(null);
  const [restartRequest, setRestartRequest] = React.useState<SkinRestartRequest | null>(null);
  const [pauseBusy, setPauseBusy] = React.useState(false);
  const zipInputRef = React.useRef<HTMLInputElement | null>(null);
  const imageInputRef = React.useRef<HTMLInputElement | null>(null);
  const loadedRef = React.useRef(false);
  const pauseInFlightRef = React.useRef(false);

  const load = React.useCallback(async ({
    quiet = false,
    notify = false,
  }: { quiet?: boolean; notify?: boolean } = {}) => {
    if (!enabled) return;
    if (!quiet) {
      setActionBusy("loadSkins");
      setError("");
    }
    try {
      setState(await invoke<SkinCenterState>("get_skin_center_state"));
      if (notify) {
        setToast(lang === "zh" ? "皮肤库已刷新" : "Skin library refreshed");
      }
    } catch (error) {
      if (!quiet) setError(String(error));
    } finally {
      if (!quiet) setActionBusy("");
    }
  }, [enabled, lang, setActionBusy, setError, setToast]);

  const refresh = React.useCallback(() => load({ notify: true }), [load]);

  React.useEffect(() => {
    if (!enabled || tab !== "skins" || loadedRef.current) return;
    loadedRef.current = true;
    void load();
  }, [enabled, load, tab]);

  React.useEffect(() => {
    if (!enabled || !ready || loadedRef.current) return;
    loadedRef.current = true;
    void load({ quiet: true });
  }, [enabled, load, ready]);

  const importZip = async (file?: File | null): Promise<string | null> => {
    if (!file) return null;
    if (!file.name.toLowerCase().endsWith(".zip")) {
      setError(lang === "zh" ? "请选择 .zip 主题包" : "Please choose a .zip theme pack");
      return null;
    }
    if (file.size > 24 * 1024 * 1024) {
      setError(lang === "zh" ? "主题包不能超过 24MB" : "Theme ZIP must be smaller than 24MB");
      return null;
    }
    setActionBusy("importSkinZip");
    setError("");
    try {
      const previousIds = new Set(state?.themes.map((theme) => theme.id) ?? []);
      const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
      const result = await invoke<SkinActionResult>("import_skin_theme_zip", {
        fileName: file.name,
        bytes,
      });
      setState(result.state);
      setToast(result.message);
      return result.state.themes.find((theme) => !previousIds.has(theme.id))?.id ?? null;
    } catch (error) {
      setError(String(error));
      return null;
    } finally {
      setActionBusy("");
      if (zipInputRef.current) zipInputRef.current.value = "";
    }
  };

  const createFromImage = async (file?: File | null): Promise<string | null> => {
    if (!file) return null;
    if (!/\.(png|jpe?g|webp)$/i.test(file.name)) {
      setError(lang === "zh"
        ? "请选择 PNG、JPEG 或 WebP 图片"
        : "Please choose a PNG, JPEG, or WebP image");
      return null;
    }
    if (file.size > 16 * 1024 * 1024) {
      setError(lang === "zh" ? "主题图片不能超过 16MB" : "Theme image must be smaller than 16MB");
      return null;
    }
    setActionBusy("createSkinFromImage");
    setError("");
    try {
      const previousIds = new Set(state?.themes.map((theme) => theme.id) ?? []);
      const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
      const result = await invoke<SkinActionResult>("create_skin_theme_from_image", {
        fileName: file.name,
        bytes,
      });
      setState(result.state);
      setToast(result.message);
      return result.state.themes.find((theme) => !previousIds.has(theme.id))?.id ?? null;
    } catch (error) {
      setError(String(error));
      return null;
    } finally {
      setActionBusy("");
      if (imageInputRef.current) imageInputRef.current.value = "";
    }
  };

  const apply = async (id: string, restartExisting = false) => {
    setActionBusy(`skin:${id}`);
    setError("");
    try {
      const result = await invoke<SkinActionResult>("enable_skin_theme", { id, restartExisting });
      setState(result.state);
      if (result.restartRequired) {
        const themeName = result.state.themes.find((theme) => theme.id === id)?.name;
        setRestartRequest({ themeId: id, themeName });
      } else {
        setRestartRequest(null);
        setToast(result.message);
      }
    } catch (error) {
      setError(String(error));
      void load({ quiet: true });
    } finally {
      setActionBusy("");
    }
  };

  const updateSettings = async (
    id: string,
    name: string,
    tagline: string,
    surfaceOpacity: number,
  ): Promise<boolean> => {
    setActionBusy(`skinEdit:${id}`);
    setError("");
    try {
      const result = await invoke<SkinActionResult>("update_skin_theme_settings", {
        id,
        name,
        tagline,
        surfaceOpacity,
      });
      setState(result.state);
      setToast(result.message);
      return true;
    } catch (error) {
      setError(String(error));
      return false;
    } finally {
      setActionBusy("");
    }
  };

  const pause = async () => {
    if (pauseInFlightRef.current) return;
    pauseInFlightRef.current = true;
    setPauseBusy(true);
    setError("");
    try {
      const result = await invoke<SkinActionResult>("pause_skin_theme");
      setState(result.state);
      setToast(result.message);
    } catch (error) {
      setError(String(error));
      void load({ quiet: true });
    } finally {
      pauseInFlightRef.current = false;
      setPauseBusy(false);
    }
  };

  const confirmRestart = async () => {
    if (restartRequest?.themeId) {
      await apply(restartRequest.themeId, true);
    }
  };

  const exportTheme = async (id: string) => {
    setError("");
    try {
      const theme = state?.themes.find((item) => item.id === id);
      const destinationPath = await save({
        title: lang === "zh" ? "导出皮肤主题" : "Export skin theme",
        defaultPath: exportFileName(theme?.name || id),
        filters: [{ name: "ZIP", extensions: ["zip"] }],
      });
      if (!destinationPath) return;
      setActionBusy(`skinExport:${id}`);
      const result = await invoke<SkinExportResult>("export_skin_theme", { id, destinationPath });
      setToast(result.message || (lang === "zh" ? `主题包已导出：${result.path}` : `Theme exported: ${result.path}`));
    } catch (error) {
      setError(String(error));
    } finally {
      setActionBusy("");
    }
  };

  return {
    state,
    restartRequest,
    pauseBusy,
    zipInputRef,
    imageInputRef,
    refresh,
    importZip,
    createFromImage,
    updateSettings,
    apply,
    pause,
    confirmRestart,
    closeRestart: () => setRestartRequest(null),
    exportTheme,
  };
}
