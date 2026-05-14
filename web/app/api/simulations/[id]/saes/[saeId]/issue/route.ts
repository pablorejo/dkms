import { NextResponse } from "next/server";
import { errorToResponse, issueSimulationSaeServerSide } from "@/api/simulations";
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
    const keyTypeRaw = String(body?.keyType ?? "ec-p256").trim().toLowerCase();
    const keyType = keyTypeRaw === "rsa-2048" ? "rsa-2048" : "ec-p256";
    const daysValidRaw = Number.parseInt(String(body?.daysValid ?? "90"), 10);
    const daysValid = Number.isFinite(daysValidRaw) && daysValidRaw > 0 ? daysValidRaw : 90;
    const bundleFormatRaw = String(body?.bundleFormat ?? "pem").trim().toLowerCase();
    const bundleFormat = bundleFormatRaw === "pkcs12" ? "pkcs12" : "pem";
    const pkcs12Password =
      body?.pkcs12Password !== undefined ? String(body.pkcs12Password) : undefined;

    const issued = await issueSimulationSaeServerSide(user.id, token, simulationId, saeId, {
      keyType,
      daysValid,
      bundleFormat,
      pkcs12Password
    });
    return NextResponse.json({ issued });
  } catch (error) {
    const result = errorToResponse(error);
    return NextResponse.json({ error: result.message }, { status: result.status });
  }
}
