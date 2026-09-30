type OrderedProviderRow = {
  isCurrent: boolean;
};

type IdentifiedProviderRow = { source: string; id: string };

export function providerRowKey(row: IdentifiedProviderRow): string {
  return `${row.source}:${row.id}`;
}

export function applyProviderRowOrder<Row extends IdentifiedProviderRow>(rows: readonly Row[], order: readonly string[]): Row[] {
  const remaining = new Map(rows.map((row) => [providerRowKey(row), row]));
  const sorted: Row[] = [];
  for (const key of order) {
    const row = remaining.get(key);
    if (row) {
      sorted.push(row);
      remaining.delete(key);
    }
  }
  return [...sorted, ...remaining.values()];
}

export function moveProviderRow(order: readonly string[], source: string, target: string, position: "before" | "after"): string[] {
  if (source === target || !order.includes(source) || !order.includes(target)) return [...order];
  const next = order.filter((key) => key !== source);
  const targetIndex = next.indexOf(target);
  next.splice(targetIndex + (position === "after" ? 1 : 0), 0, source);
  return next;
}

export function orderProviderRows<
  Official extends OrderedProviderRow,
  Detected extends OrderedProviderRow,
  Local extends OrderedProviderRow,
>(
  official: Official,
  detected: readonly Detected[],
  local: readonly Local[],
): Array<Official | Detected | Local> {
  const rows: Array<Official | Detected | Local> = [official];
  if (!local.some((row) => row.isCurrent)) rows.push(...detected);
  rows.push(...local);
  return rows;
}
