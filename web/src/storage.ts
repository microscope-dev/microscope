export type Area = "local" | "session";

const storage = (area: Area) => (area === "local" ? localStorage : sessionStorage);

export function read<T>(
  key: string,
  fallback: T,
  valid: (value: unknown) => value is T,
  area: Area = "local",
): T {
  try {
    const text = storage(area).getItem(key);
    if (text === null) {
      return fallback;
    }
    const value: unknown = JSON.parse(text);
    return valid(value) ? value : fallback;
  } catch {
    return fallback;
  }
}

export function write(key: string, value: unknown, area: Area = "local"): void {
  try {
    storage(area).setItem(key, JSON.stringify(value));
  } catch {}
}

export const isString = (value: unknown): value is string => typeof value === "string";

export const isBoolean = (value: unknown): value is boolean => typeof value === "boolean";

export interface RecentLaunch {
  program: string;
  arguments: string;
  cwd: string;
  environment: string;
}

export const isRecentLaunches = (value: unknown): value is RecentLaunch[] =>
  Array.isArray(value) &&
  value.every(
    (item) =>
      typeof item === "object" &&
      item !== null &&
      ["program", "arguments", "cwd", "environment"].every(
        (key) => typeof (item as Record<string, unknown>)[key] === "string",
      ),
  );

export function rememberLaunch(launch: RecentLaunch): RecentLaunch[] {
  const previous = read("microscope-recent", [], isRecentLaunches).filter(
    (item) => item.program !== launch.program || item.arguments !== launch.arguments,
  );
  const recent = [launch, ...previous].slice(0, 8);
  write("microscope-recent", recent);
  return recent;
}
