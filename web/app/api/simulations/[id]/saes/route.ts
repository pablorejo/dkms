import { NextResponse } from "next/server";
import {
  createSimulationSae,
  errorToResponse,
  listSimulationSaes
} from "@/api/simulations";
import { validateRequest } from "@/lib/auth/session";

function parsePositiveInt(raw: string): number {
  const value = Number.parseInt(raw, 10);
  return Number.isFinite(value) && value > 0 ? value : 0;
}

export async function GET(request: Request, context: { params: Promise<{ id: string }> }) {
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

    const url = new URL(request.url);
    const dkmsIdParam = url.searchParams.get("dkmsId");
    const dkmsId = dkmsIdParam ? parsePositiveInt(dkmsIdParam) : 0;
    const saes = await listSimulationSaes(
      user.id,
      token,
      simulationId,
      dkmsId > 0 ? dkmsId : undefined
    );
    return NextResponse.json({ saes });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}

export async function POST(request: Request, context: { params: Promise<{ id: string }> }) {
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

    const body = await request.json();
    const saeId = String(body?.saeId ?? "").trim();
    const dkmsId = parsePositiveInt(String(body?.dkmsId ?? ""));
    const displayName = body?.displayName !== undefined ? String(body.displayName) : undefined;

    if (!saeId) {
      return NextResponse.json({ error: "saeId is required" }, { status: 400 });
    }
    if (!dkmsId) {
      return NextResponse.json({ error: "dkmsId is required" }, { status: 400 });
    }

    const sae = await createSimulationSae(user.id, token, {
      simulationId,
      dkmsId,
      saeId,
      displayName
    });
    return NextResponse.json({ sae }, { status: 201 });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
