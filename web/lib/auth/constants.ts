export const ORCH_TOKEN_COOKIE_NAME = process.env.ORCH_TOKEN_COOKIE_NAME ?? "dkms_orch_token";
export const ORCH_USER_COOKIE_NAME = process.env.ORCH_USER_COOKIE_NAME ?? "dkms_orch_user";
const RAW_ORCH_BASE_URL = process.env.ORCH_BASE_URL?.trim();
const RAW_AUTHZ_BASE_URL = process.env.AUTHZ_BASE_URL?.trim();
const RAW_ORCH_COOKIE_SECURE = process.env.ORCH_COOKIE_SECURE?.trim().toLowerCase();

export const ORCH_BASE_URL = RAW_ORCH_BASE_URL || RAW_AUTHZ_BASE_URL || "http://localhost:8080";
function deriveAuthzBaseUrl(): string {
  if (RAW_AUTHZ_BASE_URL) {
    return RAW_AUTHZ_BASE_URL;
  }

  if (!RAW_ORCH_BASE_URL) {
    return "http://localhost:8081";
  }

  try {
    const parsed = new URL(RAW_ORCH_BASE_URL);
    const isLocal = parsed.hostname === "localhost" || parsed.hostname === "127.0.0.1";
    const isDefaultOrchPort = !parsed.port || parsed.port === "8080";
    if (isLocal && isDefaultOrchPort) {
      return `${parsed.protocol}//${parsed.hostname}:8081`;
    }
  } catch {
    return RAW_ORCH_BASE_URL;
  }

  return RAW_ORCH_BASE_URL;
}

export const AUTHZ_BASE_URL = deriveAuthzBaseUrl();
export const ORCH_REQUEST_TIMEOUT_MS = Number.parseInt(process.env.ORCH_REQUEST_TIMEOUT_MS ?? "300000", 10);
export const SESSION_COOKIE_NAME = ORCH_TOKEN_COOKIE_NAME;
export const ORCH_COOKIE_SECURE =
  RAW_ORCH_COOKIE_SECURE === "1" ||
  RAW_ORCH_COOKIE_SECURE === "true" ||
  RAW_ORCH_COOKIE_SECURE === "yes"
    ? true
    : RAW_ORCH_COOKIE_SECURE === "0" ||
        RAW_ORCH_COOKIE_SECURE === "false" ||
        RAW_ORCH_COOKIE_SECURE === "no"
      ? false
      : process.env.NODE_ENV === "production";
