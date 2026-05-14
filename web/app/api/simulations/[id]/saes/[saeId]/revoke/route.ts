import { NextResponse } from "next/server";
import { errorToResponse, revokeSimulationSae } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

function parsePositiveInt(raw: string): number {
  const value = Number.parseInt(raw, 10);
  return Number.isFinite(value) && value > 0 ? value : 0;
}

export async function POST(
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

    const body = await request.json().catch(() => ({}));
    const reason = body?.reason !== undefined ? String(body.reason) : undefined;

    const sae = await revokeSimulationSae(user.id, token, simulationId, saeId, reason);
    return NextResponse.json({ sae });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
