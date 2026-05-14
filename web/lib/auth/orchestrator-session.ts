import { cookies } from "next/headers";
import { redirect } from "next/navigation";
import { NextResponse } from "next/server";
import {
  AUTHZ_BASE_URL,
  ORCH_BASE_URL,
  ORCH_REQUEST_TIMEOUT_MS,
  ORCH_TOKEN_COOKIE_NAME,
  ORCH_USER_COOKIE_NAME,
  ORCH_COOKIE_SECURE
} from "@/lib/auth/constants";

export interface OrchestratorUser {
  id: number;
  username: string;
  email: string;
  is_active: boolean;
}

interface CookieUserPayload {
  id: number;
  username: string;
  email: string;
  is_active: boolean;
}

function assertBaseUrl(baseUrl: string, label: string): string {
  const base = baseUrl.trim();
  if (!base) {
    throw new Error(`${label} is not configured`);
  }
  return base.replace(/\/+$/, "");
}

function mapFetchError(error: unknown, base: string, path: string): Error {
  if (error instanceof Error) {
    return new Error(`Cannot reach upstream ${base}${path}: ${error.message}`);
  }
  return new Error(`Cannot reach upstream ${base}${path}`);
}

export function authCookieAttributes() {
  return {
    httpOnly: true,
    secure: ORCH_COOKIE_SECURE,
    sameSite: "lax" as const,
    path: "/",
    maxAge: 60 * 60 * 24 * 30
  };
}

export function setOrchestratorTokenCookie(response: NextResponse, token: string) {
  response.cookies.set(ORCH_TOKEN_COOKIE_NAME, token, authCookieAttributes());
}

export function clearOrchestratorTokenCookie(response: NextResponse) {
  response.cookies.set(ORCH_TOKEN_COOKIE_NAME, "", {
    ...authCookieAttributes(),
    maxAge: 0
  });
}

function normalizeCookieUser(raw: unknown): OrchestratorUser | null {
  if (!raw || typeof raw !== "object") {
    return null;
  }
  const candidate = raw as Partial<CookieUserPayload>;
  const id = Number(candidate.id);
  const username = typeof candidate.username === "string" ? candidate.username.trim() : "";
  const email = typeof candidate.email === "string" ? candidate.email.trim() : "";
  if (!Number.isFinite(id) || id <= 0 || !username || !email) {
    return null;
  }
  return {
    id: Math.trunc(id),
    username,
    email,
    is_active: Boolean(candidate.is_active)
  };
}

function encodeUserCookie(user: OrchestratorUser): string {
  const payload: CookieUserPayload = {
    id: user.id,
    username: user.username,
    email: user.email,
    is_active: Boolean(user.is_active)
  };
  return Buffer.from(JSON.stringify(payload), "utf8").toString("base64url");
}

function decodeUserCookie(value: string | undefined): OrchestratorUser | null {
  if (!value) {
    return null;
  }
  try {
    const decoded = Buffer.from(value, "base64url").toString("utf8");
    return normalizeCookieUser(JSON.parse(decoded));
  } catch {
    return null;
  }
}

function isFallbackUsername(username: string): boolean {
  return /^user-\d+$/i.test(username.trim());
}

export function setOrchestratorUserCookie(response: NextResponse, user: OrchestratorUser) {
  response.cookies.set(ORCH_USER_COOKIE_NAME, encodeUserCookie(user), authCookieAttributes());
}

export function clearOrchestratorUserCookie(response: NextResponse) {
  response.cookies.set(ORCH_USER_COOKIE_NAME, "", {
    ...authCookieAttributes(),
    maxAge: 0
  });
}

async function upstreamFetch(baseUrl: string, path: string, init: RequestInit = {}, token?: string) {
  const base = assertBaseUrl(baseUrl, "Upstream base URL");
  const headers = new Headers(init.headers ?? {});

  if (token) {
    headers.set("Authorization", `Bearer ${token}`);
  }
  if (!headers.has("Content-Type") && init.body) {
    headers.set("Content-Type", "application/json");
  }

  try {
    return await fetch(`${base}${path}`, {
      ...init,
      headers,
      cache: "no-store",
      signal: AbortSignal.timeout(Math.max(1000, ORCH_REQUEST_TIMEOUT_MS || 15000))
    });
  } catch (error) {
    throw mapFetchError(error, base, path);
  }
}

export async function orchestratorFetch(path: string, init: RequestInit = {}, token?: string) {
  return upstreamFetch(ORCH_BASE_URL, path, init, token);
}

export async function authzFetch(path: string, init: RequestInit = {}, token?: string) {
  return upstreamFetch(AUTHZ_BASE_URL, path, init, token);
}

export async function fetchMe(token: string): Promise<OrchestratorUser | null> {
  const response = await authzFetch("/me", { method: "GET" }, token);
  if (!response.ok) {
    return null;
  }
  const payload = await response.json().catch(() => null);
  if (!payload || typeof payload !== "object") {
    return null;
  }

  const user = payload as Partial<OrchestratorUser>;
  if (typeof user.id !== "number" || typeof user.username !== "string" || typeof user.email !== "string") {
    return null;
  }

  return {
    id: user.id,
    username: user.username,
    email: user.email,
    is_active: Boolean(user.is_active)
  };
}

function decodeJwtPayload(token: string): Record<string, unknown> | null {
  const parts = token.split(".");
  if (parts.length < 2) {
    return null;
  }
  try {
    const payloadBase64 = parts[1].replace(/-/g, "+").replace(/_/g, "/");
    const padded = payloadBase64.padEnd(Math.ceil(payloadBase64.length / 4) * 4, "=");
    const parsed = JSON.parse(Buffer.from(padded, "base64").toString("utf8"));
    return parsed && typeof parsed === "object" ? (parsed as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

function userFromJwt(token: string): OrchestratorUser | null {
  const payload = decodeJwtPayload(token);
  if (!payload) {
    return null;
  }

  const idCandidate = payload.uid ?? payload.sub ?? payload.user_id ?? payload.id;
  const idValue = Number(idCandidate);
  if (!Number.isFinite(idValue) || idValue <= 0) {
    return null;
  }

  const usernameCandidate = payload.username ?? payload.preferred_username ?? payload.name;
  const emailCandidate = payload.email;

  const username =
    typeof usernameCandidate === "string" && usernameCandidate.trim()
      ? usernameCandidate.trim()
      : `user-${idValue}`;
  const email =
    typeof emailCandidate === "string" && emailCandidate.trim()
      ? emailCandidate.trim()
      : `${username}@unknown.local`;

  return {
    id: idValue,
    username,
    email,
    is_active: true
  };
}

export async function resolveUserFromToken(token: string): Promise<OrchestratorUser | null> {
  try {
    const user = await fetchMe(token);
    if (user) {
      return user;
    }
  } catch {
    // ignore and fallback to JWT payload
  }
  return userFromJwt(token);
}

export async function validateRequest() {
  const cookieStore = await cookies();
  const token = cookieStore.get(ORCH_TOKEN_COOKIE_NAME)?.value ?? null;
  const cookieUser = decodeUserCookie(cookieStore.get(ORCH_USER_COOKIE_NAME)?.value);
  if (!token) {
    return {
      user: null,
      token: null
    };
  }

  try {
    const user = await resolveUserFromToken(token);
    if (!user && cookieUser) {
      return {
        user: cookieUser,
        token
      };
    }
    if (!user) {
      return {
        user: null,
        token: null
      };
    }
    if (
      cookieUser &&
      cookieUser.id === user.id &&
      isFallbackUsername(user.username) &&
      !isFallbackUsername(cookieUser.username)
    ) {
      return {
        user: cookieUser,
        token
      };
    }
    return {
      user,
      token
    };
  } catch {
    return {
      user: null,
      token: null
    };
  }
}

export async function requireUser() {
  const { user } = await validateRequest();
  if (!user) {
    redirect("/login");
  }
  return user;
}
