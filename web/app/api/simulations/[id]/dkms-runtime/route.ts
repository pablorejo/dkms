import { NextResponse } from "next/server";
import { errorToResponse, listSimulationDkmsRuntimeLocators } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

function parseSimulationId(raw: string): number {
  const value = Number.parseInt(raw, 10);
  return Number.isFinite(value) && value > 0 ? value : 0;
}

function normalizeBaseUrl(raw: string): string | null {
  const value = String(raw ?? "").trim();
  if (!value) {
    return null;
  }
  if (value.startsWith("http://") || value.startsWith("https://")) {
    return value.replace(/\/+$/, "");
  }
  return `https://${value.replace(/\/+$/, "")}`;
}

function resolveRuntimeBaseUrl(request: Request): string | null {
  const configured = normalizeBaseUrl(process.env.WEB_RUNTIME_BASE_URL ?? "");
  if (configured) {
    return configured;
  }
  const host = request.headers.get("x-forwarded-host") ?? request.headers.get("host");
  if (!host || !host.trim()) {
    return null;
  }
  const protoRaw = (request.headers.get("x-forwarded-proto") ?? "http").split(",")[0]?.trim().toLowerCase();
  const protocol = protoRaw === "https" ? "https" : "http";
  return normalizeBaseUrl(`${protocol}://${host.trim()}`);
}

export async function GET(request: Request, context: { params: Promise<{ id: string }> }) {
  try {
    const { user, token } = await validateRequest();
    if (!user || !token) {
      return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
    }

    const params = await context.params;
    const simulationId = parseSimulationId(params.id);
    if (!simulationId) {
      return NextResponse.json({ error: "Invalid simulation id" }, { status: 400 });
    }

    const runtimeBaseUrl = resolveRuntimeBaseUrl(request);
    if (!runtimeBaseUrl) {
      return NextResponse.json({ error: "Cannot resolve runtime ingress host" }, { status: 500 });
    }

    const byNodeId = await listSimulationDkmsRuntimeLocators(user.id, token, simulationId, runtimeBaseUrl);
    return NextResponse.json({
      simulationId,
      runtimeBaseUrl,
      byNodeId
    });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
