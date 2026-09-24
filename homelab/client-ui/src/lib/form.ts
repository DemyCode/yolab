export type ShowIf = Record<string, unknown>;

export function showIfMet(
  condition: ShowIf | undefined,
  formData: Record<string, unknown>,
): boolean {
  if (!condition) return true;
  return Object.entries(condition).every(
    ([key, value]) => formData[key] === value,
  );
}
