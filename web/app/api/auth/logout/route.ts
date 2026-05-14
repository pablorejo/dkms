import { NextResponse } from "next/server";
import { clearOrchestratorTokenCookie, clearOrchestratorUserCookie } from "@/lib/auth/orchestrator-session";

export async function POST() {
  const response = NextResponse.json({ ok: true });
  clearOrchestratorTokenCookie(response);
  clearOrchestratorUserCookie(response);
  return response;
}
