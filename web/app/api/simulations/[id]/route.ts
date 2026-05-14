import { NextResponse } from "next/server";
import { deleteSimulation, errorToResponse, getSimulation, updateSimulation } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";
import type { SimulationLinkInput, SimulationNodeInput } from "@/lib/topology/types";

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

    const simulation = await getSimulation(user.id, token, simulationId);
    return NextResponse.json({ simulation });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}

export async function PATCH(request: Request, context: { params: Promise<{ id: string }> }) {
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

    const body = await request.json();
    const payload: {
      name?: string;
      description?: string | null;
      sdn?: {
        ip?: string;
        port?: number;
        typeHttp?: "http" | "https";
      };
      nodes?: SimulationNodeInput[];
      links?: SimulationLinkInput[];
    } = {};

    if (body?.name !== undefined) {
      payload.name = String(body.name);
    }

    if (body?.description !== undefined) {
      payload.description = body.description === null ? null : String(body.description);
    }

    if (body?.sdn !== undefined) {
      payload.sdn = {
        ip: body?.sdn?.ip !== undefined ? String(body.sdn.ip) : undefined,
        port: body?.sdn?.port !== undefined ? Number(body.sdn.port) : undefined,
        typeHttp: body?.sdn?.typeHttp === "https" ? "https" : "http"
      };
    }

    if (Array.isArray(body?.nodes)) {
      payload.nodes = body.nodes as SimulationNodeInput[];
    }

    if (Array.isArray(body?.links)) {
      payload.links = body.links as SimulationLinkInput[];
    }

    const updated = await updateSimulation(user.id, token, simulationId, payload);
    return NextResponse.json({ simulation: updated });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}

export async function DELETE(_: Request, context: { params: Promise<{ id: string }> }) {
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

    await deleteSimulation(user.id, token, simulationId);
    return NextResponse.json({ ok: true });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
