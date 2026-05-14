export type NodeType = "DKMS";
export type LinkType = "QKD" | "PQC" | "HYBRID";
export type RunStatus = "QUEUED" | "RUNNING" | "DONE" | "FAILED";
export type SimulationStatus = "pending" | "running" | "finished" | "error";

export const DEFAULT_QUDITTO_RATE_R0 = 120;
export const DEFAULT_QUDITTO_RATE_ALPHA = 0.2;

export interface SimulationNodeInput {
  uid: string;
  type: NodeType;
  nodeId: number | null;
  label: string;
  x: number;
  y: number;
}

export interface SimulationLinkInput {
  uid: string;
  sourceUid: string;
  targetUid: string;
  linkType: LinkType;
  distanceKm: number;
  qudittoMaxBufferSize: number;
  qudittoRateR0: number;
  qudittoRateAlpha: number;
}

export interface SimulationDTO {
  id: number;
  name: string;
  description: string | null;
  status: SimulationStatus;
  sdn: {
    ip: string;
    port: number;
    typeHttp: "http" | "https";
  };
  createdAt: string;
  updatedAt: string;
  nodes: SimulationNodeInput[];
  links: SimulationLinkInput[];
}

export interface SimulationSummaryDTO {
  id: number;
  name: string;
  description: string | null;
  status: SimulationStatus;
  createdAt: string;
  updatedAt: string;
  nodeCount: number;
  linkCount: number;
  saeCount: number;
}

export interface SimulationRunDTO {
  id: number;
  simulationId: number;
  status: RunStatus;
  message: string | null;
  queuedAt: string | null;
  startedAt: string | null;
  finishedAt: string | null;
  createdAt: string;
}

export type ChannelTypeCanonical = "qkd" | "pqc-simulation";

export interface TopologyConnectionCanonical {
  init: number;
  end: number;
  channel: {
    type_channel: ChannelTypeCanonical;
    distance: number;
    max_buffer_size: number;
    rate_r0: number;
    rate_alpha: number;
  };
  pqc_simulation: boolean;
  hybrid_enabled?: boolean;
}

export interface TopologyNodeExtension {
  uid: string;
  node_type: NodeType;
  node_id?: number;
  label?: string;
  x: number;
  y: number;
}

export interface TopologyLinkExtension {
  uid: string;
  source_uid: string;
  target_uid: string;
  link_type: LinkType;
  distance_km?: number;
  quditto_max_buffer_size?: number;
  quditto_rate_r0?: number;
  quditto_rate_alpha?: number;
}

export type TopologySaeStatus = "pending_cert" | "active" | "revoked" | "expired";

export interface TopologySaeExtension {
  sae_id: string;
  display_name?: string;
  dkms_id: number;
  status?: TopologySaeStatus;
}

export interface TopologyCanonicalV1 {
  version: "1.0";
  nodes: number[];
  connections: TopologyConnectionCanonical[];
  sdn: {
    ip: string;
    port: number;
    type_http: "http" | "https";
  };
  extensions?: {
    simulation?: {
      name?: string;
      description?: string;
    };
    editor?: {
      nodes: TopologyNodeExtension[];
      links: TopologyLinkExtension[];
      saes?: TopologySaeExtension[];
    };
  };
}
