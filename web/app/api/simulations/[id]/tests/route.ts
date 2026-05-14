import { NextResponse } from "next/server";
import {
  errorToResponse,
  listSimulationLoadTests,
  startSimulationLoadTest
} from "@/api/simulations";
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
    const tests = await listSimulationLoadTests(user.id, token, simulationId);
    return NextResponse.json({ tests });
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
    const simulationId = parseSimulationId(params.id);
    if (!simulationId) {
      return NextResponse.json({ error: "Invalid simulation id" }, { status: 400 });
    }
    const body = await request.json().catch(() => ({}));
    const test = await startSimulationLoadTest(user.id, token, simulationId, {
      startSaes: Number(body.startSaes ?? body.start_saes ?? 5),
      endSaes: Number(body.endSaes ?? body.end_saes ?? 50),
      stepSaes: Number(body.stepSaes ?? body.step_saes ?? 5),
      intervalSeconds: Number(body.intervalSeconds ?? body.interval_seconds ?? 15),
      offsetSeconds:
        body.offsetSeconds === undefined && body.offset_seconds === undefined
          ? null
          : Number(body.offsetSeconds ?? body.offset_seconds),
      warmupSeconds: Number(body.warmupSeconds ?? body.warmup_seconds ?? 30),
      keySizeBits: Number(body.keySizeBits ?? body.key_size_bits ?? 256),
      perSaeLambda: Number(body.perSaeLambda ?? body.per_sae_lambda ?? 0.5),
      requestTimeoutSeconds: Number(
        body.requestTimeoutSeconds ?? body.request_timeout_seconds ?? 60
      )
    });
    return NextResponse.json({ test }, { status: 201 });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
