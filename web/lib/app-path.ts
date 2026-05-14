const DEFAULT_BASE_PATH = "/web";

function normalizeBasePath(value: string | undefined): string {
  const trimmed = (value ?? "").trim();
  if (!trimmed || trimmed === "/") {
    return "";
  }
  const prefixed = trimmed.startsWith("/") ? trimmed : `/${trimmed}`;
  return prefixed.endsWith("/") ? prefixed.slice(0, -1) : prefixed;
}

function normalizePath(path: string): string {
  if (!path) {
    return "/";
  }
  if (path.startsWith("http://") || path.startsWith("https://")) {
    return path;
  }
  return path.startsWith("/") ? path : `/${path}`;
}

export const APP_BASE_PATH = normalizeBasePath(process.env.NEXT_PUBLIC_BASE_PATH ?? DEFAULT_BASE_PATH);

export function stripBasePath(pathname: string): string {
  const normalized = normalizePath(pathname);
  if (!APP_BASE_PATH || normalized.startsWith("http")) {
    return normalized;
  }
  if (normalized === APP_BASE_PATH) {
    return "/";
  }
  if (normalized.startsWith(`${APP_BASE_PATH}/`)) {
    return normalized.slice(APP_BASE_PATH.length);
  }
  return normalized;
}

export function withBasePath(path: string): string {
  const normalized = normalizePath(path);
  if (!APP_BASE_PATH || normalized.startsWith("http")) {
    return normalized;
  }
  if (normalized === APP_BASE_PATH || normalized.startsWith(`${APP_BASE_PATH}/`)) {
    return normalized;
  }
  return `${APP_BASE_PATH}${normalized}`;
}

export function apiPath(path: string): string {
  const normalized = normalizePath(path);
  const apiNormalized =
    normalized === "/api" || normalized.startsWith("/api/") ? normalized : `/api${normalized}`;
  return withBasePath(apiNormalized);
}
