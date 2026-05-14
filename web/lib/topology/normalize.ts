import { safeInt } from "@/lib/utils";
import type {
  ChannelTypeCanonical,
  TopologyCanonicalV1,
  TopologyConnectionCanonical
} from "@/lib/topology/types";
import {
  DEFAULT_QUDITTO_RATE_ALPHA as DEFAULT_QUDITTO_RATE_ALPHA_VALUE,
  DEFAULT_QUDITTO_RATE_R0 as DEFAULT_QUDITTO_RATE_R0_VALUE
} from "@/lib/topology/types";

const LINK_TYPE_ALIASES: Record<string, ChannelTypeCanonical> = {
  qkd: "qkd",
  normal: "qkd",
  real: "qkd",
  direct: "qkd",
  hybrid: "qkd",
  "qkd+pqc": "qkd",
  "qkd-pqc": "qkd",
  // Classic not supported; normalize to qkd for compatibility.
  classic: "qkd",
  clasico: "qkd",
  "pqc-simulation": "pqc-simulation",
  pqc: "pqc-simulation",
  simul: "pqc-simulation",
  simulated: "pqc-simulation",
  simulation: "pqc-simulation"
};

function parseChannelType(raw: unknown): ChannelTypeCanonical {
  const text = String(raw ?? "qkd").trim().toLowerCase().replace(/_/g, "-");
  return LINK_TYPE_ALIASES[text] ?? "qkd";
}

function parseHybridEnabled(raw: unknown): boolean {
  const text = String(raw ?? "").trim().toLowerCase().replace(/_/g, "-");
  return text === "hybrid" || text === "qkd+pqc" || text === "qkd-pqc";
}

function parseBooleanFlag(raw: unknown): boolean {
  if (typeof raw === "boolean") return raw;
  const text = String(raw ?? "").trim().toLowerCase();
  return text === "1" || text === "true" || text === "yes" || text === "on";
}

function parseDistance(raw: unknown): number {
  const distance = safeInt(raw, 0) ?? 0;
  return distance >= 0 ? distance : 0;
}

function parseQudittoMaxBufferSize(raw: unknown): number {
  const value = safeInt(raw, 100) ?? 100;
  return value >= 1 ? value : 100;
}

function parseQudittoRateR0(raw: unknown): number {
  const value = Number(raw);
  if (!Number.isFinite(value) || value <= 0) {
    return DEFAULT_QUDITTO_RATE_R0_VALUE;
  }
  return value;
}

function parseQudittoRateAlpha(raw: unknown): number {
  const value = Number(raw);
  if (!Number.isFinite(value) || value < 0) {
    return DEFAULT_QUDITTO_RATE_ALPHA_VALUE;
  }
  return value;
}

function parseTopologyNeighbors(
  nodeId: number,
  neighbors: unknown
): Array<[number, ChannelTypeCanonical, number, number, number, number, boolean]> {
  if (neighbors && typeof neighbors === "object" && !Array.isArray(neighbors)) {
    return Object.entries(neighbors as Record<string, unknown>)
      .map(([key, val]) => {
        const neighborId = safeInt(key, null);
        if (!neighborId || neighborId <= 0) return null;
        let linkTypeRaw: unknown = val;
        let distanceRaw: unknown = 0;
        let maxBufferRaw: unknown = 100;
        let rateR0Raw: unknown = DEFAULT_QUDITTO_RATE_R0_VALUE;
        let rateAlphaRaw: unknown = DEFAULT_QUDITTO_RATE_ALPHA_VALUE;
        let hybridEnabledRaw: unknown = false;
        if (val && typeof val === "object") {
          const v = val as Record<string, unknown>;
          linkTypeRaw = v.type ?? v.type_channel ?? v.mode ?? v.channel_type ?? "qkd";
          distanceRaw = v.distance ?? v.distance_km ?? 0;
          maxBufferRaw = v.max_buffer_size ?? v.quditto_max_buffer_size ?? 100;
          rateR0Raw = v.rate_r0 ?? v.quditto_rate_r0 ?? v.r0 ?? DEFAULT_QUDITTO_RATE_R0_VALUE;
          rateAlphaRaw = v.rate_alpha ?? v.quditto_rate_alpha ?? v.alpha ?? DEFAULT_QUDITTO_RATE_ALPHA_VALUE;
          hybridEnabledRaw = v.hybrid_enabled ?? false;
        }
        const type = parseChannelType(linkTypeRaw);
        const hybridEnabled = (parseBooleanFlag(hybridEnabledRaw) || parseHybridEnabled(linkTypeRaw)) && type !== "pqc-simulation";
        return [
          neighborId,
          type,
          parseDistance(distanceRaw),
          parseQudittoMaxBufferSize(maxBufferRaw),
          parseQudittoRateR0(rateR0Raw),
          parseQudittoRateAlpha(rateAlphaRaw),
          hybridEnabled
        ] as [
          number,
          ChannelTypeCanonical,
          number,
          number,
          number,
          number,
          boolean
        ];
      })
      .filter((item): item is [number, ChannelTypeCanonical, number, number, number, number, boolean] => item !== null);
  }

  if (Array.isArray(neighbors)) {
    return neighbors
      .map((entry) => {
        if (typeof entry === "number") {
          return [
            entry,
            "qkd",
            0,
            100,
            DEFAULT_QUDITTO_RATE_R0_VALUE,
            DEFAULT_QUDITTO_RATE_ALPHA_VALUE,
            false
          ] as [number, ChannelTypeCanonical, number, number, number, number, boolean];
        }
        if (typeof entry === "string") {
          const [idPart, typePart] = entry.split(/[:|-]/g, 2);
          const neighborId = safeInt(idPart, null);
          if (!neighborId || neighborId <= 0) return null;
          const type = parseChannelType(typePart ?? "qkd");
          const hybridEnabled = parseHybridEnabled(typePart ?? "qkd") && type !== "pqc-simulation";
          return [
            neighborId,
            type,
            0,
            100,
            DEFAULT_QUDITTO_RATE_R0_VALUE,
            DEFAULT_QUDITTO_RATE_ALPHA_VALUE,
            hybridEnabled
          ] as [number, ChannelTypeCanonical, number, number, number, number, boolean];
        }
        if (entry && typeof entry === "object") {
          const obj = entry as Record<string, unknown>;
          const neighborId = safeInt(obj.id ?? obj.neighbor, null);
          if (!neighborId || neighborId <= 0) return null;
          const rawType = obj.type ?? obj.type_channel ?? obj.mode ?? "qkd";
          const rawDistance = obj.distance ?? obj.distance_km ?? 0;
          const rawMaxBuffer = obj.max_buffer_size ?? obj.quditto_max_buffer_size ?? 100;
          const rawRateR0 = obj.rate_r0 ?? obj.quditto_rate_r0 ?? obj.r0 ?? DEFAULT_QUDITTO_RATE_R0_VALUE;
          const rawRateAlpha =
            obj.rate_alpha ?? obj.quditto_rate_alpha ?? obj.alpha ?? DEFAULT_QUDITTO_RATE_ALPHA_VALUE;
          const type = parseChannelType(rawType);
          const hybridEnabled = (parseBooleanFlag(obj.hybrid_enabled) || parseHybridEnabled(rawType)) && type !== "pqc-simulation";
          return [
            neighborId,
            type,
            parseDistance(rawDistance),
            parseQudittoMaxBufferSize(rawMaxBuffer),
            parseQudittoRateR0(rawRateR0),
            parseQudittoRateAlpha(rawRateAlpha),
            hybridEnabled
          ] as [
            number,
            ChannelTypeCanonical,
            number,
            number,
            number,
            number,
            boolean
          ];
        }
        return null;
      })
      .filter((item): item is [number, ChannelTypeCanonical, number, number, number, number, boolean] => item !== null);
  }

  return [];
}

function edgeKey(a: number, b: number): string {
  return `${Math.min(a, b)}:${Math.max(a, b)}`;
}

function toConnection(
  a: number,
  b: number,
  type: ChannelTypeCanonical,
  distance: number,
  maxBufferSize: number,
  rateR0: number,
  rateAlpha: number,
  hybridEnabled = false
): TopologyConnectionCanonical {
  const isPqc = type === "pqc-simulation";
  return {
    init: Math.min(a, b),
    end: Math.max(a, b),
    channel: {
      type_channel: type,
      distance: isPqc ? 0 : parseDistance(distance),
      max_buffer_size: isPqc ? 100 : parseQudittoMaxBufferSize(maxBufferSize),
      rate_r0: isPqc ? DEFAULT_QUDITTO_RATE_R0_VALUE : parseQudittoRateR0(rateR0),
      rate_alpha: isPqc ? DEFAULT_QUDITTO_RATE_ALPHA_VALUE : parseQudittoRateAlpha(rateAlpha)
    },
    pqc_simulation: isPqc,
    ...(hybridEnabled && !isPqc ? { hybrid_enabled: true } : {})
  };
}

export function normalizeTopologyDocument(rawInput: unknown): TopologyCanonicalV1 {
  const input = rawInput && typeof rawInput === "object" ? (rawInput as Record<string, unknown>) : {};

  const edgeMap = new Map<string, TopologyConnectionCanonical>();
  const nodesSet = new Set<number>();

  const rawConnections = input.connections;
  if (Array.isArray(rawConnections)) {
    for (const conn of rawConnections) {
      if (!conn || typeof conn !== "object") continue;
      const c = conn as Record<string, unknown>;
      const a = safeInt(c.init, null);
      const b = safeInt(c.end, null);
      if (!a || !b || a <= 0 || b <= 0 || a === b) continue;

      const channelObj = c.channel && typeof c.channel === "object" ? (c.channel as Record<string, unknown>) : null;
      const rawType =
        channelObj?.type_channel ??
        channelObj?.type ??
        c.type_channel ??
        c.type ??
        c.channel_type ??
        (c.pqc_simulation ? "pqc-simulation" : "qkd");
      const type = parseChannelType(rawType);
      const hybridEnabled =
        (parseBooleanFlag(channelObj?.hybrid_enabled) ||
          parseBooleanFlag(c.hybrid_enabled) ||
          parseHybridEnabled(rawType)) &&
        type !== "pqc-simulation";
      const distance = parseDistance(channelObj?.distance ?? c.distance ?? c.distance_km);
      const maxBufferSize = parseQudittoMaxBufferSize(
        channelObj?.max_buffer_size ?? c.max_buffer_size ?? c.quditto_max_buffer_size
      );
      const rateR0 = parseQudittoRateR0(
        channelObj?.rate_r0 ?? c.rate_r0 ?? c.quditto_rate_r0 ?? c.r0
      );
      const rateAlpha = parseQudittoRateAlpha(
        channelObj?.rate_alpha ?? c.rate_alpha ?? c.quditto_rate_alpha ?? c.alpha
      );

      nodesSet.add(a);
      nodesSet.add(b);
      edgeMap.set(edgeKey(a, b), toConnection(a, b, type, distance, maxBufferSize, rateR0, rateAlpha, hybridEnabled));
    }
  }

  const rawTopology = input.topology;
  if (rawTopology && typeof rawTopology === "object" && !Array.isArray(rawTopology)) {
    for (const [rawNodeId, rawNodeEntry] of Object.entries(rawTopology as Record<string, unknown>)) {
      const nodeId = safeInt(rawNodeId, null);
      if (!nodeId || nodeId <= 0) continue;
      nodesSet.add(nodeId);

      const entryObj = rawNodeEntry && typeof rawNodeEntry === "object" ? (rawNodeEntry as Record<string, unknown>) : {};
      const neighbors =
        entryObj.neighbors ??
        entryObj.neighbours ??
        entryObj.links ??
        entryObj.adjacency ??
        rawNodeEntry;
      const parsedNeighbors = parseTopologyNeighbors(nodeId, neighbors);
      for (const [neighborId, type, distance, maxBufferSize, rateR0, rateAlpha, hybridEnabled] of parsedNeighbors) {
        if (neighborId === nodeId) continue;
        nodesSet.add(neighborId);
        const key = edgeKey(nodeId, neighborId);
        if (!edgeMap.has(key)) {
          edgeMap.set(key, toConnection(nodeId, neighborId, type, distance, maxBufferSize, rateR0, rateAlpha, hybridEnabled));
        }
      }
    }
  }

  const rawNodes = input.nodes;
  if (Array.isArray(rawNodes)) {
    for (const n of rawNodes) {
      if (typeof n === "number") {
        if (n > 0) nodesSet.add(n);
        continue;
      }
      if (n && typeof n === "object") {
        const nodeId = safeInt((n as Record<string, unknown>).id, null);
        if (nodeId && nodeId > 0) nodesSet.add(nodeId);
      }
    }
  }

  const rawSdn = input.sdn && typeof input.sdn === "object" ? (input.sdn as Record<string, unknown>) : {};
  const normalized: TopologyCanonicalV1 = {
    version: "1.0",
    nodes: Array.from(nodesSet).sort((a, b) => a - b),
    connections: Array.from(edgeMap.values()).sort((a, b) => (a.init - b.init) || (a.end - b.end)),
    sdn: {
      ip: String(rawSdn.ip ?? "172.30.0.2"),
      port: safeInt(rawSdn.port, 3000) ?? 3000,
      type_http: String(rawSdn.type_http ?? "http").toLowerCase() === "https" ? "https" : "http"
    }
  };

  const rawExtensions = input.extensions;
  if (rawExtensions && typeof rawExtensions === "object") {
    normalized.extensions = rawExtensions as TopologyCanonicalV1["extensions"];
  }

  if (input.name || input.description) {
    normalized.extensions = normalized.extensions ?? {};
    normalized.extensions.simulation = {
      ...(normalized.extensions.simulation ?? {}),
      ...(input.name ? { name: String(input.name) } : {}),
      ...(input.description ? { description: String(input.description) } : {})
    };
  }

  return normalized;
}
