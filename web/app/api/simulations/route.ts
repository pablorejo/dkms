import { NextResponse } from "next/server";
import { createSimulation, errorToResponse, listSimulationSaes, listSimulations } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

export async function GET() {
  try {
    const { user, token } = await validateRequest();
    if (!user || !token) {
      return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
    }

    const simulations = await listSimulations(user.id, token);
    const saeCounts = await Promise.all(
      simulations.map(async (simulation) => {
        try {
          const saes = await listSimulationSaes(user.id, token, simulation.id);
          return { id: simulation.id, count: saes.length };
        } catch {
          return { id: simulation.id, count: 0 };
        }
      })
    );
    const countById = new Map<number, number>(saeCounts.map((item) => [item.id, item.count]));
    const enriched = simulations.map((simulation) => ({
      ...simulation,
      saeCount: countById.get(simulation.id) ?? simulation.saeCount ?? 0
    }));
    return NextResponse.json({ simulations: enriched });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}

export async function POST(request: Request) {
  try {
    const { user, token } = await validateRequest();
    if (!user || !token) {
      return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
    }

    const body = await request.json();
    const created = await createSimulation(user.id, token, {
      name: String(body?.name ?? "").trim(),
      description: body?.description != null ? String(body.description) : null,
      sdn: body?.sdn
        ? {
            ip: body.sdn.ip != null ? String(body.sdn.ip) : undefined,
            port: body.sdn.port != null ? Number(body.sdn.port) : undefined,
            typeHttp: body.sdn.typeHttp === "https" ? "https" : "http"
          }
        : undefined
    });

    return NextResponse.json({ simulation: created }, { status: 201 });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
