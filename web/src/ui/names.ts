export function threadLabel(id: number, name?: string | null): string {
  return name ? `thread ${id} (${name})` : `thread ${id}`;
}
