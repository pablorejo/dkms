import { NextResponse } from "next/server";
import { createSimulationRun, errorToResponse, getSimulation } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

function parseSimulationId(raw: string): number {
  const id = Number.parseInt(raw, 10);
  return Number.isFinite(id) ? id : 0;
}

export async function POST(_: Request, context: { params: Promise<{ id: string }> }) {
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

    const simulation = await getSimulation(user.id, token, simulationId);
    if (!simulation.nodes || simulation.nodes.length < 1) {
      return NextResponse.json(
        { error: "Simulation has no DKMS nodes persisted. Save topology before running." },
        { status: 409 }
      );
    }

    const run = await createSimulationRun(user.id, token, simulationId);
    return NextResponse.json({ run }, { status: 201 });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
