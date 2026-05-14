import {
  DEFAULT_QUDITTO_RATE_ALPHA,
  DEFAULT_QUDITTO_RATE_R0,
  type LinkType,
  type SimulationDTO,
  type TopologyCanonicalV1,
  type TopologySaeStatus
} from "@/lib/topology/types";

interface ExportableSae {
  saeId: string;
  displayName?: string | null;
  dkmsId: number | null;
  status?: string | null;
}

function toChannelType(linkType: LinkType): "qkd" | "pqc-simulation" {
  if (linkType === "PQC") return "pqc-simulation";
  return "qkd";
}

function normalizeDistanceKm(value: unknown): number {
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return 0;
  }
  return Math.trunc(parsed);
}

function normalizeQudittoMaxBufferSize(value: unknown): number {
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 1) {
    return 100;
  }
  return Math.trunc(parsed);
}

function normalizeQudittoRateR0(value: unknown): number {
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    return DEFAULT_QUDITTO_RATE_R0;
  }
  return parsed;
}

function normalizeQudittoRateAlpha(value: unknown): number {
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return DEFAULT_QUDITTO_RATE_ALPHA;
  }
  return parsed;
}

function normalizeSaeStatus(raw: unknown): TopologySaeStatus | null {
  const normalized = String(raw ?? "").trim().toLowerCase();
  if (normalized === "active" || normalized === "revoked" || normalized === "expired" || normalized === "pending_cert") {
    return normalized;
  }
  return null;
}

export function exportSimulationTopology(
  simulation: SimulationDTO,
  options?: { saes?: ExportableSae[] }
): TopologyCanonicalV1 {
  const dkmsNodes = simulation.nodes
    .filter((node) => node.type === "DKMS" && typeof node.nodeId === "number")
    .sort((a, b) => (a.nodeId ?? 0) - (b.nodeId ?? 0));

  const uidToNodeId = new Map(dkmsNodes.map((node) => [node.uid, node.nodeId as number]));

  const nodes = Array.from(new Set(dkmsNodes.map((node) => node.nodeId as number))).sort((a, b) => a - b);

  const connections = simulation.links
    .map((link) => {
      const sourceNodeId = uidToNodeId.get(link.sourceUid);
      const targetNodeId = uidToNodeId.get(link.targetUid);
      if (!sourceNodeId || !targetNodeId || sourceNodeId === targetNodeId) {
        return null;
      }
      const init = Math.min(sourceNodeId, targetNodeId);
      const end = Math.max(sourceNodeId, targetNodeId);
      return {
        init,
        end,
        channel: {
          type_channel: toChannelType(link.linkType),
          distance: link.linkType === "PQC" ? 0 : normalizeDistanceKm(link.distanceKm),
          max_buffer_size: link.linkType === "PQC" ? 100 : normalizeQudittoMaxBufferSize(link.qudittoMaxBufferSize),
          rate_r0: link.linkType === "PQC" ? DEFAULT_QUDITTO_RATE_R0 : normalizeQudittoRateR0(link.qudittoRateR0),
          rate_alpha:
            link.linkType === "PQC" ? DEFAULT_QUDITTO_RATE_ALPHA : normalizeQudittoRateAlpha(link.qudittoRateAlpha)
        },
        pqc_simulation: link.linkType === "PQC",
        hybrid_enabled: link.linkType === "HYBRID"
      };
    })
    .filter((item): item is NonNullable<typeof item> => item !== null)
    .sort((a, b) => a.init - b.init || a.end - b.end);

  return {
    version: "1.0",
    nodes,
    connections,
    sdn: {
      ip: simulation.sdn.ip,
      port: simulation.sdn.port,
      type_http: simulation.sdn.typeHttp
    },
    extensions: {
      simulation: {
        name: simulation.name,
        ...(simulation.description ? { description: simulation.description } : {})
      },
      editor: {
        nodes: simulation.nodes
          .map((node) => ({
            uid: node.uid,
            node_type: node.type,
            ...(typeof node.nodeId === "number" ? { node_id: node.nodeId } : {}),
            ...(node.label ? { label: node.label } : {}),
            x: node.x,
            y: node.y
          }))
          .sort((a, b) => a.uid.localeCompare(b.uid)),
        links: simulation.links
          .map((link) => ({
            uid: link.uid,
            source_uid: link.sourceUid,
            target_uid: link.targetUid,
            link_type: link.linkType,
            distance_km: link.linkType === "PQC" ? 0 : normalizeDistanceKm(link.distanceKm),
            quditto_max_buffer_size:
              link.linkType === "PQC" ? 100 : normalizeQudittoMaxBufferSize(link.qudittoMaxBufferSize),
            quditto_rate_r0:
              link.linkType === "PQC" ? DEFAULT_QUDITTO_RATE_R0 : normalizeQudittoRateR0(link.qudittoRateR0),
            quditto_rate_alpha:
              link.linkType === "PQC"
                ? DEFAULT_QUDITTO_RATE_ALPHA
                : normalizeQudittoRateAlpha(link.qudittoRateAlpha)
          }))
          .sort((a, b) => a.uid.localeCompare(b.uid)),
        ...(Array.isArray(options?.saes)
          ? {
              saes: options.saes
                .map((sae) => {
                  const saeId = String(sae.saeId ?? "").trim();
                  const dkmsId = Number(sae.dkmsId);
                  if (!saeId || !Number.isFinite(dkmsId) || dkmsId <= 0) {
                    return null;
                  }
                  const normalizedStatus = normalizeSaeStatus(sae.status);
                  return {
                    sae_id: saeId,
                    ...(sae.displayName ? { display_name: String(sae.displayName).trim() } : {}),
                    dkms_id: Math.trunc(dkmsId),
                    ...(normalizedStatus ? { status: normalizedStatus } : {})
                  };
                })
                .filter((item): item is NonNullable<typeof item> => item !== null)
                .sort((a, b) => a.sae_id.localeCompare(b.sae_id))
            }
          : {})
      }
    }
  };
}
