import { NextResponse } from "next/server";
import { errorToResponse, stopSimulationLoadTest } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

function parseSimulationId(raw: string): number {
  const id = Number.parseInt(raw, 10);
  return Number.isFinite(id) ? id : 0;
}

export async function DELETE(
  _: Request,
  context: { params: Promise<{ id: string; testId: string }> }
) {
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
    const testId = String(params.testId || "").trim();
    if (!testId) {
      return NextResponse.json({ error: "Invalid test id" }, { status: 400 });
    }
    const result = await stopSimulationLoadTest(user.id, token, simulationId, testId);
    return NextResponse.json(result);
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
