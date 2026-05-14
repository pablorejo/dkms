import { normalizeTopologyDocument } from "@/lib/topology/normalize";
import type {
  LinkType,
  SimulationLinkInput,
  SimulationNodeInput,
  TopologySaeStatus
} from "@/lib/topology/types";
import {
  DEFAULT_QUDITTO_RATE_ALPHA as DEFAULT_QUDITTO_RATE_ALPHA_VALUE,
  DEFAULT_QUDITTO_RATE_R0 as DEFAULT_QUDITTO_RATE_R0_VALUE
} from "@/lib/topology/types";
import { safeInt } from "@/lib/utils";

export interface ImportedSimulationSae {
  saeId: string;
  displayName: string | null;
  dkmsId: number;
  status: TopologySaeStatus | null;
}

function toEditorLinkType(raw: unknown): LinkType {
  const normalized = String(raw ?? "QKD").toUpperCase();
  if (normalized === "HYBRID") return "HYBRID";
  if (normalized === "PQC" || normalized === "PQC-SIMULATION") return "PQC";
  // Classic is not supported in the editor; treat as QKD on import.
  return "QKD";
}

function channelTypeToEditorLinkType(raw: unknown, hybridEnabled: unknown): LinkType {
  const hybridText = String(hybridEnabled ?? "").trim().toLowerCase();
  if (hybridEnabled === true || hybridText === "1" || hybridText === "true" || hybridText === "yes" || hybridText === "on") {
    return "HYBRID";
  }
  const text = String(raw ?? "qkd").toLowerCase().replace(/_/g, "-");
  if (text === "hybrid" || text === "qkd+pqc" || text === "qkd-pqc") return "HYBRID";
  if (text === "pqc" || text === "pqc-simulation" || text === "simul") return "PQC";
  // Classic is not supported in the editor; treat as QKD on import.
  return "QKD";
}

function normalizeDistanceKm(raw: unknown): number {
  const parsed = safeInt(raw, 0) ?? 0;
  return parsed >= 0 ? parsed : 0;
}

function normalizeQudittoMaxBufferSize(raw: unknown): number {
  const parsed = safeInt(raw, 100) ?? 100;
  return parsed >= 1 ? parsed : 100;
}

function normalizeQudittoRateR0(raw: unknown): number {
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    return DEFAULT_QUDITTO_RATE_R0_VALUE;
  }
  return parsed;
}

function normalizeQudittoRateAlpha(raw: unknown): number {
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return DEFAULT_QUDITTO_RATE_ALPHA_VALUE;
  }
  return parsed;
}

function normalizeSaeStatus(raw: unknown): TopologySaeStatus | null {
  const normalized = String(raw ?? "").trim().toLowerCase();
  if (normalized === "pending_cert" || normalized === "active" || normalized === "revoked" || normalized === "expired") {
    return normalized;
  }
  return null;
}

function parseImportedSaes(rawSaes: unknown): ImportedSimulationSae[] {
  if (!Array.isArray(rawSaes)) {
    return [];
  }
  const parsed: ImportedSimulationSae[] = [];
  for (const item of rawSaes) {
    if (!item || typeof item !== "object") {
      continue;
    }
    const value = item as Record<string, unknown>;
    const saeId = String(value.sae_id ?? "").trim();
    const dkmsId = safeInt(value.dkms_id, null);
    if (!saeId || !dkmsId || dkmsId <= 0) {
      continue;
    }
    parsed.push({
      saeId,
      displayName:
        value.display_name === undefined || value.display_name === null
          ? null
          : String(value.display_name).trim() || null,
      dkmsId,
      status: normalizeSaeStatus(value.status)
    });
  }
  const dedup = new Map<string, ImportedSimulationSae>();
  for (const item of parsed) {
    dedup.set(item.saeId, item);
  }
  return Array.from(dedup.values()).sort((a, b) => a.saeId.localeCompare(b.saeId));
}

function circularPosition(index: number, total: number): { x: number; y: number } {
  if (total <= 1) return { x: 160, y: 140 };
  const radius = Math.max(180, total * 24);
  const angle = (2 * Math.PI * index) / total;
  return {
    x: Math.round(320 + radius * Math.cos(angle)),
    y: Math.round(220 + radius * Math.sin(angle))
  };
}

export function importTopologyToSimulationGraph(payload: unknown): {
  name?: string;
  description?: string;
  sdn?: {
    ip: string;
    port: number;
    typeHttp: "http" | "https";
  };
  nodes: SimulationNodeInput[];
  links: SimulationLinkInput[];
  saes?: ImportedSimulationSae[];
} {
  const normalized = normalizeTopologyDocument(payload);
  const ext = normalized.extensions?.editor;

  const sdn = {
    ip: normalized.sdn.ip,
    port: normalized.sdn.port,
    typeHttp: normalized.sdn.type_http
  } as const;

  if (ext && Array.isArray(ext.nodes) && Array.isArray(ext.links)) {
    const nodes: SimulationNodeInput[] = ext.nodes
      .filter((node) => String(node.node_type).toUpperCase() === "DKMS")
      .map((node, idx) => ({
        uid: String(node.uid ?? `node-${idx + 1}`),
        type: "DKMS",
        nodeId: safeInt(node.node_id, null),
        label: String(node.label ?? node.uid ?? `Node ${idx + 1}`),
        x: Number(node.x ?? 100 + idx * 120),
        y: Number(node.y ?? 120)
      }));

    const nodeIds = new Set(nodes.map((node) => node.uid));
    const links: SimulationLinkInput[] = ext.links
      .map((link, idx) => {
        const linkType = toEditorLinkType(link.link_type);
        return {
          uid: String(link.uid ?? `edge-${idx + 1}`),
          sourceUid: String(link.source_uid),
          targetUid: String(link.target_uid),
          linkType,
          distanceKm: linkType === "PQC" ? 0 : normalizeDistanceKm(link.distance_km),
          qudittoMaxBufferSize: linkType === "PQC" ? 100 : normalizeQudittoMaxBufferSize(link.quditto_max_buffer_size),
          qudittoRateR0: linkType === "PQC" ? DEFAULT_QUDITTO_RATE_R0_VALUE : normalizeQudittoRateR0(link.quditto_rate_r0),
          qudittoRateAlpha:
            linkType === "PQC"
              ? DEFAULT_QUDITTO_RATE_ALPHA_VALUE
              : normalizeQudittoRateAlpha(link.quditto_rate_alpha)
        };
      })
      .filter((link) => nodeIds.has(link.sourceUid) && nodeIds.has(link.targetUid));

    return {
      ...(normalized.extensions?.simulation?.name ? { name: normalized.extensions.simulation.name } : {}),
      ...(normalized.extensions?.simulation?.description
        ? { description: normalized.extensions.simulation.description }
        : {}),
      sdn,
      nodes,
      links,
      ...(Array.isArray(ext.saes) ? { saes: parseImportedSaes(ext.saes) } : {})
    };
  }

  const sortedNodeIds = [...normalized.nodes].sort((a, b) => a - b);
  const nodeIdToUid = new Map<number, string>();

  const nodes: SimulationNodeInput[] = sortedNodeIds.map((nodeId, index) => {
    const uid = `dkms-${nodeId}`;
    nodeIdToUid.set(nodeId, uid);
    const pos = circularPosition(index, sortedNodeIds.length);
    return {
      uid,
      type: "DKMS",
      nodeId,
      label: `DKMS ${nodeId}`,
      x: pos.x,
      y: pos.y
    };
  });

  const links: SimulationLinkInput[] = normalized.connections
    .map((conn, index) => {
      const sourceUid = nodeIdToUid.get(conn.init);
      const targetUid = nodeIdToUid.get(conn.end);
      if (!sourceUid || !targetUid) return null;
      const linkType = channelTypeToEditorLinkType(conn.channel.type_channel, conn.hybrid_enabled);

      return {
        uid: `edge-${index + 1}`,
        sourceUid,
        targetUid,
        linkType,
        distanceKm: linkType === "PQC" ? 0 : normalizeDistanceKm(conn.channel.distance),
        qudittoMaxBufferSize: linkType === "PQC" ? 100 : normalizeQudittoMaxBufferSize(conn.channel.max_buffer_size),
        qudittoRateR0: linkType === "PQC" ? DEFAULT_QUDITTO_RATE_R0_VALUE : normalizeQudittoRateR0(conn.channel.rate_r0),
        qudittoRateAlpha:
          linkType === "PQC"
            ? DEFAULT_QUDITTO_RATE_ALPHA_VALUE
            : normalizeQudittoRateAlpha(conn.channel.rate_alpha)
      } as SimulationLinkInput;
    })
    .filter((link): link is SimulationLinkInput => link !== null);

  return {
    ...(normalized.extensions?.simulation?.name ? { name: normalized.extensions.simulation.name } : {}),
    ...(normalized.extensions?.simulation?.description
      ? { description: normalized.extensions.simulation.description }
      : {}),
    sdn,
    nodes,
    links,
    ...(Array.isArray(ext?.saes) ? { saes: parseImportedSaes(ext.saes) } : {})
  };
}
