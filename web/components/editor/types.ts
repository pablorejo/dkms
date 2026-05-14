import type { Edge, Node } from "reactflow";
import {
  DEFAULT_QUDITTO_RATE_ALPHA,
  DEFAULT_QUDITTO_RATE_R0,
  type LinkType,
  type NodeType,
  type SimulationLinkInput,
  type SimulationNodeInput
} from "@/lib/topology/types";

export type EditorNodeKind = NodeType | "SAE";
export type EditorSaeStatus = "pending_cert" | "active" | "revoked" | "expired";

export interface EditorNodeData {
  label: string;
  nodeType: EditorNodeKind;
  nodeId: number | null;
  grafanaUrl?: string | null;
  grafanaStatus?: "disabled" | "checking" | "available" | "unavailable";
  connectionHandleEnabled?: boolean;
  healthState?: "up" | "down" | "unknown";
  stopDkmsVisible?: boolean;
  stopDkmsPending?: boolean;
  onStopDkms?: (() => void) | null;
  startDkmsVisible?: boolean;
  startDkmsPending?: boolean;
  onStartDkms?: (() => void) | null;
  saeId?: string | null;
  saeDisplayName?: string | null;
  saeStatus?: EditorSaeStatus;
  saeCertFingerprint?: string | null;
  saeCertNotAfter?: string | null;
  parentDkmsId?: number | null;
  runtimeBasePath?: string | null;
}

export interface EditorEdgeData {
  linkType: LinkType;
  distanceKm: number;
  qudittoMaxBufferSize: number;
  qudittoRateR0: number;
  qudittoRateAlpha: number;
  transient?: boolean;
}

export type FlowNode = Node<EditorNodeData>;
export type FlowEdge = Edge<EditorEdgeData>;

export interface ValidationResult {
  errors: string[];
  warnings: string[];
}

export function flowNodesToInput(nodes: FlowNode[]): SimulationNodeInput[] {
  return nodes
    .filter((node) => node.data.nodeType === "DKMS")
    .map((node) => ({
      uid: node.id,
      type: "DKMS",
      nodeId: node.data.nodeId,
      label: node.data.label,
      x: node.position.x,
      y: node.position.y
    }));
}

export function flowEdgesToInput(edges: FlowEdge[]): SimulationLinkInput[] {
  return edges
    .filter((edge) => !edge.data?.transient)
    .map((edge) => ({
      uid: edge.id,
      sourceUid: edge.source,
      targetUid: edge.target,
      linkType: edge.data?.linkType ?? "QKD",
      distanceKm:
        Number.isFinite(Number(edge.data?.distanceKm)) && Number(edge.data?.distanceKm) >= 0
          ? Math.trunc(Number(edge.data?.distanceKm))
          : 0,
      qudittoMaxBufferSize:
        Number.isFinite(Number(edge.data?.qudittoMaxBufferSize)) && Number(edge.data?.qudittoMaxBufferSize) >= 1
          ? Math.trunc(Number(edge.data?.qudittoMaxBufferSize))
          : 100,
      qudittoRateR0:
        Number.isFinite(Number(edge.data?.qudittoRateR0)) && Number(edge.data?.qudittoRateR0) > 0
          ? Number(edge.data?.qudittoRateR0)
          : DEFAULT_QUDITTO_RATE_R0,
      qudittoRateAlpha:
        Number.isFinite(Number(edge.data?.qudittoRateAlpha)) && Number(edge.data?.qudittoRateAlpha) >= 0
          ? Number(edge.data?.qudittoRateAlpha)
          : DEFAULT_QUDITTO_RATE_ALPHA
    }));
}
