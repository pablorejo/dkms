import { NextResponse } from "next/server";
import { errorToResponse, listSimulationRuns } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

function parseSimulationId(raw: string): number {
  const id = Number.parseInt(raw, 10);
  return Number.isFinite(id) ? id : 0;
}

export async function GET(_: Request, context: { params: Promise<{ id: string }> }) {
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

    const runs = await listSimulationRuns(user.id, token, simulationId);
    return NextResponse.json({ runs });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
