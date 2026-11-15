export function fileName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}
