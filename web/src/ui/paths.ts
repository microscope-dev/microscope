/** The last part of a path, such as `server.c`. */
export function fileName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}
