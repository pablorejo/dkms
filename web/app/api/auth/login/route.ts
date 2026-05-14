import { NextResponse } from "next/server";
import { loginSchema } from "@/lib/validators/auth";
import { authzFetch, resolveUserFromToken, setOrchestratorTokenCookie, setOrchestratorUserCookie } from "@/lib/auth/orchestrator-session";

export async function POST(request: Request) {
  try {
    const body = await request.json();
    const parsed = loginSchema.safeParse(body);

    if (!parsed.success) {
      return NextResponse.json({ error: parsed.error.issues[0]?.message ?? "Invalid payload" }, { status: 400 });
    }

    const { identifier, password } = parsed.data;
    const normalized = identifier.trim();
    const isEmail = normalized.includes("@");

    const loginPayload = isEmail
      ? { email: normalized, password }
      : { username: normalized, password };

    const upstream = await authzFetch("/login", {
      method: "POST",
      body: JSON.stringify(loginPayload)
    });

    const upstreamBody = await upstream.json().catch(() => ({}));
    if (!upstream.ok) {
      const detail =
        typeof upstreamBody?.detail === "string"
          ? upstreamBody.detail
          : upstreamBody?.error || "Invalid credentials";
      return NextResponse.json({ error: detail }, { status: upstream.status });
    }

    const token = String(upstreamBody?.access_token ?? "").trim();
    if (!token) {
      return NextResponse.json({ error: "AuthZ did not return access_token" }, { status: 502 });
    }

    const user = await resolveUserFromToken(token);
    if (!user) {
      return NextResponse.json({ error: "Failed to fetch user profile" }, { status: 502 });
    }
    if (/^user-\d+$/i.test(user.username) && !isEmail && normalized) {
      user.username = normalized;
    }

    const response = NextResponse.json({
      user: {
        id: user.id,
        username: user.username,
        email: user.email
      }
    });
    setOrchestratorTokenCookie(response, token);
    setOrchestratorUserCookie(response, user);
    return response;
  } catch (error) {
    return NextResponse.json(
      { error: error instanceof Error ? error.message : "Internal server error" },
      { status: 500 }
    );
  }
}
