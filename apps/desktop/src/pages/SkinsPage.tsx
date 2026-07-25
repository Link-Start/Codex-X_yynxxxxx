import { useEffect, useMemo, useState } from "react";
import type { CSSProperties, ChangeEvent, RefObject } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  Check,
  Download,
  ImagePlus,
  Loader2,
  Palette,
  Pencil,
  Play,
  Power,
  RefreshCw,
  Settings2,
  Upload,
} from "lucide-react";

import { PromptCategoryManager } from "../components/PromptCategoryManager";
import type { PromptCategoryItem } from "../components/PromptCategoryManager";
import { PageTransition } from "../components/PageTransition";
import { SkinThemeEditor } from "../components/SkinThemeEditor";
import { Button, IconButton, StatusBadge, cx } from "../components/ui";
import { useManagedCategories } from "../promptCategories";
import type { ManagedCategoryOptions } from "../promptCategories";
import type { Lang, SkinCenterState, SkinThemeSummary } from "../types";
import "../styles/skins-page.css";

type MaybeAsyncAction = () => void | Promise<void>;

const SKIN_CATEGORY_OPTIONS: ManagedCategoryOptions = {
  storageKey: "codexx.skinCategories.v1",
  defaultCategories: (lang) => lang === "zh"
    ? [
        { id: "anime", name: "动漫" },
        { id: "scenery", name: "风景" },
        { id: "minimal", name: "简约" },
      ]
    : [
        { id: "anime", name: "Anime" },
        { id: "scenery", name: "Scenery" },
        { id: "minimal", name: "Minimal" },
      ],
};

export type SkinsPageProps = {
  lang: Lang;
  state: SkinCenterState | null;
  actionBusy: string;
  pauseBusy: boolean;
  zipInputRef: RefObject<HTMLInputElement>;
  imageInputRef: RefObject<HTMLInputElement>;
  onLoad: MaybeAsyncAction;
  onImportZip: (file?: File | null) => Promise<string | null>;
  onCreateFromImage: (file?: File | null) => Promise<string | null>;
  onUpdateThemeSettings: (id: string, name: string, tagline: string, surfaceOpacity: number) => Promise<boolean>;
  onEnableTheme: (id: string) => void | Promise<void>;
  onExportTheme: (id: string) => void | Promise<void>;
  onPauseTheme: MaybeAsyncAction;
};

function getCopy(lang: Lang) {
  return lang === "zh"
    ? {
        eyebrow: "SKIN CENTER",
        title: "皮肤中心",
        description: "在 Codex-X 中管理主题包，并直接应用到官方 Codex Desktop。",
        refresh: "刷新",
        refreshingButton: "刷新中",
        refreshing: "正在刷新主题库...",
        importZip: "导入主题包",
        createFromImage: "从图片创建皮肤",
        creatingFromImage: "正在创建",
        pause: "关闭皮肤",
        pausing: "关闭中",
        loading: "正在读取皮肤库...",
        builtin: "内置",
        imported: "导入",
        selected: "已选择",
        applied: "已启用",
        apply: "启用",
        export: "导出",
        edit: "编辑",
        applying: "启用中",
        exporting: "导出中",
        current: "当前皮肤",
        runtime: "Codex 运行状态",
        noCurrent: "尚未启用皮肤",
        noThemes: "还没有皮肤。选择一张图片即可创建。",
        manageCategories: "分类管理",
        emptyCategory: "该分类下暂无皮肤",
        statusReady: "主题库已就绪",
        selectTheme: (name: string) => `选择皮肤：${name}`,
      }
    : {
        eyebrow: "SKIN CENTER",
        title: "Skin Center",
        description: "Manage theme packs in Codex-X and apply them directly to the official Codex Desktop app.",
        refresh: "Refresh",
        refreshingButton: "Refreshing",
        refreshing: "Refreshing skin library...",
        importZip: "Import theme ZIP",
        createFromImage: "Create from image",
        creatingFromImage: "Creating",
        pause: "Turn off skin",
        pausing: "Turning off",
        loading: "Loading skin library...",
        builtin: "Built-in",
        imported: "Imported",
        selected: "Selected",
        applied: "Enabled",
        apply: "Enable",
        export: "Export",
        edit: "Edit",
        applying: "Enabling",
        exporting: "Exporting",
        current: "Current skin",
        runtime: "Codex runtime",
        noCurrent: "No skin enabled yet",
        noThemes: "No skins yet. Choose an image to create one.",
        manageCategories: "Manage categories",
        emptyCategory: "No skins in this category",
        statusReady: "Theme library ready",
        selectTheme: (name: string) => `Select skin: ${name}`,
      };
}

function run(action: MaybeAsyncAction) {
  void action();
}

function sourceLabel(theme: SkinThemeSummary, copy: ReturnType<typeof getCopy>) {
  return theme.source === "builtin" ? copy.builtin : copy.imported;
}

function ThemeCard({
  theme,
  copy,
  runtimeActive,
  activeThemeId,
  selected,
  onSelect,
}: {
  theme: SkinThemeSummary;
  copy: ReturnType<typeof getCopy>;
  runtimeActive: boolean;
  activeThemeId?: string | null;
  selected: boolean;
  onSelect: () => void;
}) {
  const applied = runtimeActive && activeThemeId === theme.id;
  const focusX = Math.round((theme.art.focusX ?? 0.5) * 100);
  const focusY = Math.round((theme.art.focusY ?? 0.5) * 100);
  const safeArea = theme.art.safeArea === "left" || theme.art.safeArea === "right" || theme.art.safeArea === "center"
    ? theme.art.safeArea
    : "center";
  const style = {
    "--skin-image": `url("${convertFileSrc(theme.imagePath || `${theme.directory}/${theme.image}`)}")`,
    "--skin-bg": theme.colors.background,
    "--skin-panel": theme.colors.panel,
    "--skin-accent": theme.colors.accent,
    "--skin-secondary": theme.colors.secondary,
    "--skin-highlight": theme.colors.highlight,
    "--skin-text": theme.colors.text,
    "--skin-muted": theme.colors.muted,
    "--skin-line": theme.colors.line,
    "--skin-focus-x": `${focusX}%`,
    "--skin-focus-y": `${focusY}%`,
  } as CSSProperties;

  return (
    <article
      className={cx(
        "cx-skins-card",
        (theme.enabled || applied) && "cx-skins-card--enabled",
        selected && "cx-skins-card--selected",
      )}
      style={style}
      role="button"
      tabIndex={0}
      aria-pressed={selected}
      aria-label={copy.selectTheme(theme.name)}
      onClick={onSelect}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          onSelect();
        }
      }}
    >
      <div className={cx("cx-skins-preview", theme.adaptive && "cx-skins-preview--adaptive")} aria-hidden="true">
        <div className={cx("cx-skins-preview-app", `cx-skins-preview-app--${safeArea}`)}>
          <aside className="cx-skins-preview-sidebar">
            <div className="cx-skins-preview-brand"><i /><span /></div>
            <div className="cx-skins-preview-nav">
              <span /><span className="is-active" /><span /><span />
            </div>
            <div className="cx-skins-preview-profile" />
          </aside>
          <div className="cx-skins-preview-main">
            <div className="cx-skins-preview-topbar"><span /><i /></div>
            <div className="cx-skins-preview-chat">
              <div className="cx-skins-preview-message"><i /><span /></div>
              <div className="cx-skins-preview-message cx-skins-preview-message--user"><span /></div>
            </div>
            <div className="cx-skins-preview-composer"><span /><i /></div>
          </div>
        </div>
      </div>

      <div className="cx-skins-card-body">
        <div className="cx-skins-card-title">
          <div>
            <strong>{theme.name}</strong>
            <span>{theme.tagline}</span>
          </div>
          <StatusBadge tone={applied ? "success" : theme.enabled ? "info" : "neutral"} dot={applied || theme.enabled}>
            {applied ? copy.applied : theme.enabled ? copy.selected : sourceLabel(theme, copy)}
          </StatusBadge>
        </div>
        <p aria-hidden={!theme.quote}>{theme.quote || "\u00a0"}</p>
      </div>
    </article>
  );
}

export function SkinsPage({
  lang,
  state,
  actionBusy,
  pauseBusy,
  zipInputRef,
  imageInputRef,
  onLoad,
  onImportZip,
  onCreateFromImage,
  onUpdateThemeSettings,
  onEnableTheme,
  onExportTheme,
  onPauseTheme,
}: SkinsPageProps) {
  const copy = getCopy(lang);
  const themes = state?.themes ?? [];
  const current = themes.find((theme) => theme.enabled);
  const loading = actionBusy === "loadSkins";
  const controlsBusy = Boolean(actionBusy) || pauseBusy;
  const runtime = state?.runtime;
  const [categoryManagerOpen, setCategoryManagerOpen] = useState(false);
  const [editorOpen, setEditorOpen] = useState(false);
  const [selectedThemeId, setSelectedThemeId] = useState<string | null>(null);
  const skinCategories = useManagedCategories(lang, SKIN_CATEGORY_OPTIONS);
  const categoryItems = useMemo<PromptCategoryItem[]>(() => themes.map((theme) => ({
    key: `skin:${theme.id.trim().toLowerCase()}`,
    title: theme.name,
  })), [themes]);
  const visibleThemes = themes.filter((theme) =>
    skinCategories.categoryForPrompt(`skin:${theme.id.trim().toLowerCase()}`) === skinCategories.activeCategoryId);
  const selectedTheme = themes.find((theme) => theme.id === selectedThemeId);
  const selectedThemeApplied = Boolean(
    selectedTheme && runtime?.active && runtime.themeId === selectedTheme.id,
  );

  useEffect(() => {
    if (selectedThemeId && !themes.some((theme) => theme.id === selectedThemeId)) {
      setSelectedThemeId(null);
    }
  }, [selectedThemeId, themes]);

  const selectCreatedTheme = (themeId: string | null) => {
    if (!themeId) return;
    const categoryId = skinCategories.categoryForPrompt(`skin:${themeId.trim().toLowerCase()}`);
    skinCategories.setActiveCategoryId(categoryId);
    setSelectedThemeId(themeId);
  };

  const handleZipChange = (event: ChangeEvent<HTMLInputElement>) => {
    void onImportZip(event.currentTarget.files?.[0] ?? null).then(selectCreatedTheme);
  };

  const handleImageChange = (event: ChangeEvent<HTMLInputElement>) => {
    void onCreateFromImage(event.currentTarget.files?.[0] ?? null).then(selectCreatedTheme);
  };

  return (
    <section className={cx("cx-skins-page", loading && "cx-skins-page--refreshing")} aria-busy={loading}>
      <PromptCategoryManager
        open={categoryManagerOpen}
        lang={lang}
        categories={skinCategories.categories}
        prompts={categoryItems}
        categoryForPrompt={skinCategories.categoryForPrompt}
        onClose={() => setCategoryManagerOpen(false)}
        onAddCategory={skinCategories.addCategory}
        onRenameCategory={skinCategories.renameCategory}
        onDeleteCategory={skinCategories.deleteCategory}
        onMovePrompt={skinCategories.movePrompt}
        subject="skin"
      />
      <SkinThemeEditor
        lang={lang}
        open={editorOpen}
        theme={selectedTheme || null}
        busy={Boolean(selectedTheme && actionBusy === `skinEdit:${selectedTheme.id}`)}
        onClose={() => setEditorOpen(false)}
        onSave={onUpdateThemeSettings}
      />
      <input
        ref={zipInputRef}
        className="cx-skins-file-input"
        type="file"
        accept=".zip,application/zip"
        onChange={handleZipChange}
      />
      <input
        ref={imageInputRef}
        className="cx-skins-file-input"
        type="file"
        accept=".png,.jpg,.jpeg,.webp,image/png,image/jpeg,image/webp"
        onChange={handleImageChange}
      />

      <header className="cx-skins-header">
        <div className="cx-skins-heading">
          <p><Palette size={15} aria-hidden="true" />{copy.eyebrow}</p>
          <h2>{copy.title}</h2>
          <span>{copy.description}</span>
        </div>
        <div className="cx-skins-actions">
          {selectedTheme && (
            <>
              <Button
                variant="secondary"
                onClick={() => setEditorOpen(true)}
                disabled={controlsBusy}
                icon={<Pencil size={15} />}
              >
                {copy.edit}
              </Button>
              <Button
                variant="secondary"
                onClick={() => run(() => onEnableTheme(selectedTheme.id))}
                disabled={controlsBusy || selectedThemeApplied}
                icon={actionBusy === `skin:${selectedTheme.id}`
                  ? <Loader2 className="cx-skins-spin" size={15} />
                  : selectedThemeApplied ? <Check size={15} /> : <Play size={15} />}
              >
                {actionBusy === `skin:${selectedTheme.id}`
                  ? copy.applying
                  : selectedThemeApplied ? copy.applied : copy.apply}
              </Button>
              <Button
                variant="secondary"
                onClick={() => run(() => onExportTheme(selectedTheme.id))}
                disabled={controlsBusy}
                icon={actionBusy === `skinExport:${selectedTheme.id}`
                  ? <Loader2 className="cx-skins-spin" size={15} />
                  : <Download size={15} />}
              >
                {actionBusy === `skinExport:${selectedTheme.id}` ? copy.exporting : copy.export}
              </Button>
            </>
          )}
          <Button
            variant="secondary"
            onClick={() => run(onPauseTheme)}
            disabled={pauseBusy}
            icon={pauseBusy ? <Loader2 className="cx-skins-spin" size={15} /> : <Power size={15} />}
          >
            {pauseBusy ? copy.pausing : copy.pause}
          </Button>
          <Button
            variant="secondary"
            onClick={() => zipInputRef.current?.click()}
            disabled={controlsBusy || !state}
            icon={actionBusy === "importSkinZip" ? <Loader2 className="cx-skins-spin" size={15} /> : <Upload size={15} />}
          >
            {copy.importZip}
          </Button>
          <Button
            variant="primary"
            onClick={() => imageInputRef.current?.click()}
            disabled={controlsBusy || !state}
            icon={actionBusy === "createSkinFromImage"
              ? <Loader2 className="cx-skins-spin" size={15} />
              : <ImagePlus size={15} />}
          >
            {actionBusy === "createSkinFromImage" ? copy.creatingFromImage : copy.createFromImage}
          </Button>
        </div>
      </header>

      <section className="cx-skins-status" aria-label={copy.statusReady}>
        <div>
          <span>{copy.current}</span>
          <strong>{current?.name || copy.noCurrent}</strong>
        </div>
        <div>
          <span>{copy.runtime}</span>
          <strong title={runtime?.message || ""}>{runtime?.message || "-"}</strong>
        </div>
        <IconButton
          className="cx-skins-refresh-button"
          icon={loading ? <Loader2 className="cx-skins-spin" /> : <RefreshCw />}
          label={loading ? copy.refreshingButton : copy.refresh}
          variant="ghost"
          size="sm"
          onClick={() => run(onLoad)}
          disabled={controlsBusy}
        />
      </section>

      {loading && state && (
        <div className="cx-skins-refresh-status" role="status" aria-live="polite">
          <Loader2 className="cx-skins-spin" size={15} aria-hidden="true" />
          <span>{copy.refreshing}</span>
        </div>
      )}

      {themes.length > 0 && (
        <div className="cx-prompt-category-toolbar cx-skins-category-toolbar">
          <div className="cx-skills-tabs cx-prompt-category-tabs" role="group" aria-label={copy.manageCategories}>
            {skinCategories.categories.map((category) => (
              <button
                type="button"
                aria-pressed={category.id === skinCategories.activeCategoryId}
                className={cx(
                  "cx-skills-tab",
                  "cx-prompt-category-tab",
                  category.id === skinCategories.activeCategoryId && "cx-skills-tab--active",
                )}
                key={category.id}
                onClick={() => {
                  setSelectedThemeId(null);
                  skinCategories.setActiveCategoryId(category.id);
                }}
              >
                {category.name}
              </button>
            ))}
          </div>
          <button type="button" className="cx-prompt-category-manage" onClick={() => setCategoryManagerOpen(true)}>
            <Settings2 size={16} aria-hidden="true" />{copy.manageCategories}
          </button>
        </div>
      )}

      <PageTransition pageKey={`skins-category:${skinCategories.activeCategoryId}`}>
        {themes.length === 0 ? (
          <div className="cx-skins-empty">
            <Palette size={24} strokeWidth={1.7} aria-hidden="true" />
            <span>{loading ? copy.loading : copy.noThemes}</span>
          </div>
        ) : visibleThemes.length === 0 ? (
          <div className="cx-skins-empty cx-skins-empty--category">
            <Palette size={24} strokeWidth={1.7} aria-hidden="true" />
            <span>{copy.emptyCategory}</span>
          </div>
        ) : (
          <div className={cx("cx-skins-grid", `cx-skins-grid--${Math.min(visibleThemes.length, 4)}`)}>
            {visibleThemes.map((theme) => (
              <ThemeCard
                key={theme.id}
                theme={theme}
                copy={copy}
                runtimeActive={Boolean(runtime?.active)}
                activeThemeId={runtime?.themeId}
                selected={theme.id === selectedThemeId}
                onSelect={() => setSelectedThemeId(theme.id)}
              />
            ))}
          </div>
        )}
      </PageTransition>
    </section>
  );
}
