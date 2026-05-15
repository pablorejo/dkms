import { NextResponse } from "next/server";
import { createSimulation, errorToResponse, listSimulations } from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

export async function GET() {
  try {
    const { user, token } = await validateRequest();
    if (!user || !token) {
      return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
    }

    const simulations = await listSimulations(user.id, token);
    return NextResponse.json({ simulations });
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
