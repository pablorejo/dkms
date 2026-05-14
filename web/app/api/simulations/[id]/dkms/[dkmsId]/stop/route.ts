import { NextResponse } from "next/server";
import { errorToResponse, stopSimulationDkms } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

function parsePositiveInt(raw: string): number {
  const value = Number.parseInt(raw, 10);
  return Number.isFinite(value) && value > 0 ? value : 0;
}

export async function POST(
  _: Request,
  context: { params: Promise<{ id: string; dkmsId: string }> }
) {
  try {
    const { user, token } = await validateRequest();
    if (!user || !token) {
      return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
    }

    const params = await context.params;
    const simulationId = parsePositiveInt(params.id);
    const dkmsId = parsePositiveInt(params.dkmsId);
    if (!simulationId) {
      return NextResponse.json({ error: "Invalid simulation id" }, { status: 400 });
    }
    if (!dkmsId) {
      return NextResponse.json({ error: "Invalid dkms id" }, { status: 400 });
    }

    const result = await stopSimulationDkms(user.id, token, simulationId, dkmsId);
    return NextResponse.json({ result });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
