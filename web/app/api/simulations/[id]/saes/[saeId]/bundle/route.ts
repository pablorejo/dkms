import { NextResponse } from "next/server";
import { errorToResponse, getSimulationSaeBundle } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

function parsePositiveInt(raw: string): number {
  const value = Number.parseInt(raw, 10);
  return Number.isFinite(value) && value > 0 ? value : 0;
}

export async function GET(
  request: Request,
  context: { params: Promise<{ id: string; saeId: string }> }
) {
  try {
    const { user, token } = await validateRequest();
    if (!user || !token) {
      return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
    }

    const params = await context.params;
    const simulationId = parsePositiveInt(params.id);
    if (!simulationId) {
      return NextResponse.json({ error: "Invalid simulation id" }, { status: 400 });
    }

    const saeId = String(params.saeId ?? "").trim();
    if (!saeId) {
      return NextResponse.json({ error: "Invalid SAE id" }, { status: 400 });
    }

    const url = new URL(request.url);
    const formatRaw = String(url.searchParams.get("format") ?? "pem").trim().toLowerCase();
    const format = formatRaw === "pkcs12" ? "pkcs12" : "pem";
    const pkcs12Password = url.searchParams.get("pkcs12Password") ?? undefined;

    const bundle = await getSimulationSaeBundle(user.id, token, simulationId, saeId, {
      format,
      pkcs12Password
    });
    return NextResponse.json({ bundle });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
