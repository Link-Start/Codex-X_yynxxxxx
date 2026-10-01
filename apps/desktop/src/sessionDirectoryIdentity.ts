export const SESSION_FOLDER_UNKNOWN_KEY = "__codexx_no_workspace__";
const SESSION_PINS_STORAGE_PREFIX = "codexx.sessionPins.v2";

export type SessionDirectoryIdentities = Readonly<Record<string, string>>;
export type SessionPinPreferences = { folders: string[]; sessions: string[] };
export type SessionDirectoryGroup<T> = { group: string; folderKey: string; items: T[] };

/** Only the backend can confirm that differently spelled paths name one directory. */
export function sessionDirectoryKey(
  path?: string | null,
  identities?: SessionDirectoryIdentities,
): string {
  if (path == null || path === "") return SESSION_FOLDER_UNKNOWN_KEY;
  if (identities && Object.prototype.hasOwnProperty.call(identities, path)) {
    return identities[path];
  }
  // Missing/offline directories keep their exact spelling. Case, backslashes,
  // roots and spaces can all distinguish legitimate paths on some filesystems.
  return `path:${path}`;
}

export function sessionPinStorageKey(codexDir: string, verifiedScope?: string | null) {
  // v1 discarded case and cannot safely assign its pins to a particular home.
  // A snapshot's file ID alone is not a permanent home identity after deletion.
  return `${SESSION_PINS_STORAGE_PREFIX}:${encodeURIComponent(verifiedScope || sessionDirectoryKey(codexDir))}`;
}

export function pinnedSessionDirectoryKeys(folders: readonly string[], identities?: SessionDirectoryIdentities): Set<string> {
  const keys = new Set<string>();
  for (const stored of folders) {
    if (stored === SESSION_FOLDER_UNKNOWN_KEY) keys.add(stored);
    else if (stored.startsWith("path:")) keys.add(sessionDirectoryKey(stored.slice(5), identities));
  }
  return keys;
}

/** Persist original paths so offline/recreated folders retain their pins. */
export function toggleFolderPins(
  folders: readonly string[],
  path?: string | null,
  identities?: SessionDirectoryIdentities,
): string[] {
  const currentKey = sessionDirectoryKey(path, identities);
  const belongsToCurrentFolder = (stored: string) => pinnedSessionDirectoryKeys([stored], identities).has(currentKey);
  if (folders.some(belongsToCurrentFolder)) return folders.filter((stored) => !belongsToCurrentFolder(stored));
  return [...new Set([...folders, sessionDirectoryKey(path)])];
}

export function readSessionPinPreferences(
  storageKey: string,
  storage: Pick<Storage, "getItem">,
): SessionPinPreferences {
  try {
    const parsed = JSON.parse(storage.getItem(storageKey) || "{}") as Partial<SessionPinPreferences>;
    return {
      folders: Array.isArray(parsed.folders) ? parsed.folders.filter((value): value is string => typeof value === "string") : [],
      sessions: Array.isArray(parsed.sessions) ? parsed.sessions.filter((value): value is string => typeof value === "string") : [],
    };
  } catch {
    return { folders: [], sessions: [] };
  }
}

export function mergeSessionDirectoryGroups<T extends { cwd?: string | null }>(
  groups: ReadonlyArray<readonly [string, readonly T[]]>,
  identities?: SessionDirectoryIdentities,
): SessionDirectoryGroup<T>[] {
  const result = new Map<string, SessionDirectoryGroup<T>>();
  for (const [group, items] of groups) {
    const folderKey = sessionDirectoryKey(items.find((item) => item.cwd)?.cwd, identities);
    const existing = result.get(folderKey);
    if (existing) existing.items.push(...items);
    else result.set(folderKey, { group, folderKey, items: [...items] });
  }
  return [...result.values()];
}

export function sessionsForDirectoryGroup<T>(groups: ReadonlyArray<SessionDirectoryGroup<T>>, key: string): T[] {
  return groups.find((group) => group.folderKey === key)?.items || [];
}
