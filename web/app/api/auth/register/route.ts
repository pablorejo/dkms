import { NextResponse } from "next/server";
import { authzFetch, resolveUserFromToken, setOrchestratorTokenCookie, setOrchestratorUserCookie } from "@/lib/auth/orchestrator-session";
import { registerSchema } from "@/lib/validators/auth";

export async function POST(request: Request) {
  try {
    const body = await request.json();
    const parsed = registerSchema.safeParse(body);

    if (!parsed.success) {
      return NextResponse.json({ error: parsed.error.issues[0]?.message ?? "Invalid payload" }, { status: 400 });
    }

    const { username, email, password } = parsed.data;

    const upstream = await authzFetch("/register", {
      method: "POST",
      body: JSON.stringify({
        username,
        email,
        password,
        is_active: true
      })
    });

    const upstreamBody = await upstream.json().catch(() => ({}));
    if (!upstream.ok) {
      const detail =
        typeof upstreamBody?.detail === "string"
          ? upstreamBody.detail
          : upstreamBody?.error || "Register failed";
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
    if (/^user-\d+$/i.test(user.username) && username.trim()) {
      user.username = username.trim();
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
