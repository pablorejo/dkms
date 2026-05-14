import { NextResponse } from "next/server";
import { getSimulation } from "@/api/simulations";
import { orchestratorFetch } from "@/lib/auth/orchestrator-session";
import { validateRequest } from "@/lib/auth/session";

function parseSimulationId(raw: string): number {
  const id = Number.parseInt(raw, 10);
  return Number.isFinite(id) ? id : 0;
}

interface DkmsRuntimeLocator {
  serviceName: string;
  healthPort: number;
}

function parsePositiveInt(value: unknown): number | null {
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    return null;
  }
  return Math.trunc(parsed);
}

function inferNodeIdFromHostPort(hostPort: unknown, hostIp: unknown): number | null {
  const port = parsePositiveInt(hostPort);
  if (port && port > 4000) {
    return port - 4000;
  }
  if (typeof hostIp === "string") {
    const match = hostIp.match(/^127\.0\.0\.(\d+)$/);
    if (match) {
      const lastOctet = Number.parseInt(match[1], 10);
      if (Number.isFinite(lastOctet) && lastOctet > 100) {
        return lastOctet - 100;
      }
    }
  }
  return null;
}

async function fetchRuntimeLocators(
  userId: number,
  token: string,
  simulationId: number
): Promise<Record<string, DkmsRuntimeLocator>> {
  const response = await orchestratorFetch(
    `/orch/api/sim/${simulationId}`,
    {
      method: "GET",
      headers: {
        "X-User-Id": String(userId),
        "X-Simulation-Id": String(simulationId)
      }
    },
    token
  );
  if (!response.ok) {
    return {};
  }

  const payload = await response.json().catch(() => ({}));
  const rawDkms = Array.isArray(payload?.list_dkms) ? payload.list_dkms : [];
  const locators: Record<string, DkmsRuntimeLocator> = {};

  for (const item of rawDkms as Array<Record<string, unknown>>) {
    const host = (item?.host as Record<string, unknown> | undefined) ?? {};
    const idHost = parsePositiveInt(item?.id_host ?? host?.id ?? item?.id);
    const nodeId = inferNodeIdFromHostPort(host?.port, host?.ip);
    const healthPort = parsePositiveInt(host?.port);
    if (!idHost || !nodeId || !healthPort) {
      continue;
    }
    locators[String(nodeId)] = {
      serviceName: `dkms-${idHost}`,
      healthPort
    };
  }

  return locators;
}

async function probeDkmsHealth(
  simulationId: number,
  nodeId: number,
  locator?: DkmsRuntimeLocator
): Promise<boolean> {
  const serviceName = locator?.serviceName ?? `dkms-${nodeId}`;
  const internalServiceHost = `${serviceName}.${simulationId}.svc.cluster.local`;
  // DKMS app ports are exposed as 400X (e.g. 4002/4003/4004/...) and some setups
  // also expose 8000. Probe a compact set of likely ports before ingress fallback.
  const candidatePorts = Array.from(
    new Set([
      locator?.healthPort ?? 0,
      8000,
      4001,
      4002,
      4003,
      4004,
      4005,
      4006,
      4007,
      4008,
      4009,
      4010
    ].filter((port) => port > 0))
  );
  for (const port of candidatePorts) {
    try {
      const internalResponse = await fetch(`http://${internalServiceHost}:${port}/health`, {
        method: "GET",
        cache: "no-store",
        signal: AbortSignal.timeout(1500)
      });
      if (internalResponse.ok) {
        return true;
      }
    } catch {
      // try next port
    }
  }
  return false;
}

export async function GET(_request: Request, context: { params: Promise<{ id: string }> }) {
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
    if (simulation.status !== "running") {
      return NextResponse.json({
        simulationStatus: simulation.status,
        healthByNodeId: {},
        totalNodes: simulation.nodes.length
      });
    }

    const uniqueNodeIds = Array.from(
      new Set(
        simulation.nodes
          .map((node) => node.nodeId)
          .filter((value): value is number => Number.isFinite(value as number) && Number(value) > 0)
          .map((value) => Number(value))
      )
    );
    const runtimeLocators = await fetchRuntimeLocators(user.id, token, simulationId);

    const checks = await Promise.all(
      uniqueNodeIds.map(async (nodeId) => ({
        nodeId,
        healthy: await probeDkmsHealth(
          simulationId,
          nodeId,
          runtimeLocators[String(nodeId)]
        )
      }))
    );

    const healthByNodeId: Record<string, boolean> = {};
    for (const check of checks) {
      healthByNodeId[String(check.nodeId)] = check.healthy;
    }

    return NextResponse.json({
      simulationStatus: simulation.status,
      healthByNodeId,
      totalNodes: simulation.nodes.length
    });
  } catch (error) {
    return NextResponse.json(
      { error: error instanceof Error ? error.message : "Internal server error" },
      { status: 500 }
    );
  }
}
