"use client";

import { type CSSProperties, type MouseEvent as ReactMouseEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ChangeEvent } from "react";
import {
  addEdge,
  applyEdgeChanges,
  applyNodeChanges,
  MarkerType,
  type Connection,
  type Edge,
  type EdgeMouseHandler,
  type EdgeChange,
  type NodeChange,
  type ReactFlowInstance
} from "reactflow";
import { ArrowLeft } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { EditorCanvas } from "@/components/editor/canvas";
import { Inspector } from "@/components/editor/inspector";
import { RunsPanel } from "@/components/editor/runs-panel";
import { EditorToolbox } from "@/components/editor/toolbox";
import { DkmsNode } from "@/components/editor/dkms-node";
import { SaeNode } from "@/components/editor/sae-node";
import type { EditorEdgeData, EditorNodeData, FlowEdge, FlowNode } from "@/components/editor/types";
import { flowEdgesToInput, flowNodesToInput } from "@/components/editor/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { ThemeToggle } from "@/components/ui/theme-toggle";
import { apiPath, withBasePath } from "@/lib/app-path";
import {
  DEFAULT_QUDITTO_RATE_ALPHA,
  DEFAULT_QUDITTO_RATE_R0,
  type SimulationDTO,
  type SimulationStatus
} from "@/lib/topology/types";

interface Props {
  simulationId: number;
  ingressBaseUrl: string;
}

type SaeAdminStatus = "pending_cert" | "active" | "revoked" | "expired";

interface SaeAdminRecord {
  id: number;
  saeId: string;
  displayName: string | null;
  dkmsId: number | null;
  status: SaeAdminStatus;
  certFingerprint: string | null;
  certNotAfter: string | null;
}

interface DkmsRuntimeInfo {
  nodeId: number;
  dkmsId: number;
  ingressId: number;
  runtimeBasePath: string;
}

type GrafanaStatus = NonNullable<EditorNodeData["grafanaStatus"]>;

function resolveSaeNodeId(
  saeDkmsId: number | null,
  runtimeByNode: Record<string, DkmsRuntimeInfo>
): number | null {
  if (!Number.isFinite(Number(saeDkmsId)) || Number(saeDkmsId) <= 0) {
    return null;
  }
  const dkmsId = Number(saeDkmsId);

  // Caso directo: el ID del SAE ya es el nodeId lógico (1..N).
  const direct = runtimeByNode[String(dkmsId)];
  if (direct && Number.isFinite(direct.nodeId) && direct.nodeId > 0) {
    return direct.nodeId;
  }

  // Caso habitual tras replace de topología: SAE.dkmsId usa ID interno runtime.
  for (const info of Object.values(runtimeByNode)) {
    if (Number(info.dkmsId) === dkmsId && Number(info.nodeId) > 0) {
      return Number(info.nodeId);
    }
    if (Number(info.ingressId) === dkmsId && Number(info.nodeId) > 0) {
      return Number(info.nodeId);
    }
  }

  // Fallback para entornos sin mapa runtime disponible.
  return dkmsId;
}

function randomId(prefix: string): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return `${prefix}-${crypto.randomUUID()}`;
  }
  return `${prefix}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => {
    window.setTimeout(resolve, ms);
  });
}

function nodeStyleByType() {
  return {
    border: "none",
    background: "transparent",
    padding: 0
  };
}

function edgeStyleByType(type: EditorEdgeData["linkType"]) {
  // Colores derivados de tokens HSL del tema (globals.css). Esto permite
  // que los enlaces QKD/PQC/HYBRID sigan la paleta activa y soporten modo
  // oscuro sin tocar valores hex.
  if (type === "PQC") {
    return { stroke: "hsl(var(--warning))", strokeWidth: 2.8 };
  }
  if (type === "HYBRID") {
    return { stroke: "hsl(var(--success))", strokeWidth: 2.8 };
  }
  return { stroke: "hsl(var(--primary))", strokeWidth: 2.8 };
}

function edgeVisualProps(type: EditorEdgeData["linkType"]) {
  const style = edgeStyleByType(type);
  return {
    type: "straight" as const,
    style,
    labelStyle: {
      fill: style.stroke,
      fontWeight: 700,
      fontSize: 12
    },
    labelBgStyle: {
      fill: "#ffffff",
      fillOpacity: 0.95
    },
    labelBgPadding: [6, 2] as [number, number],
    markerEnd: {
      type: MarkerType.ArrowClosed,
      color: style.stroke,
      width: 20,
      height: 20
    }
  };
}

function normalizeDistanceKm(raw: unknown): number {
  const value = Number(raw);
  if (!Number.isFinite(value) || value < 0) {
    return 0;
  }
  return Math.trunc(value);
}

function normalizeQudittoMaxBufferSize(raw: unknown): number {
  const value = Number(raw);
  if (!Number.isFinite(value) || value < 1) {
    return 100;
  }
  return Math.trunc(value);
}

function normalizeQudittoRateR0(raw: unknown): number {
  const value = Number(raw);
  if (!Number.isFinite(value) || value <= 0) {
    return DEFAULT_QUDITTO_RATE_R0;
  }
  return value;
}

function normalizeQudittoRateAlpha(raw: unknown): number {
  const value = Number(raw);
  if (!Number.isFinite(value) || value < 0) {
    return DEFAULT_QUDITTO_RATE_ALPHA;
  }
  return value;
}

function parsePositiveInt(raw: string, fallback = 100): number {
  const value = Number.parseInt(raw.trim(), 10);
  if (!Number.isFinite(value) || value < 1) {
    return fallback;
  }
  return value;
}

function sameStringArray(left: string[], right: string[]): boolean {
  if (left.length !== right.length) {
    return false;
  }
  for (let index = 0; index < left.length; index += 1) {
    if (left[index] !== right[index]) {
      return false;
    }
  }
  return true;
}

function mapSaeRecords(payload: any): SaeAdminRecord[] {
  if (!Array.isArray(payload?.saes)) {
    return [];
  }
  return payload.saes.map((item: any) => ({
    id: Number(item.id ?? 0),
    saeId: String(item.saeId ?? ""),
    displayName: item.displayName ?? null,
    dkmsId: Number.isFinite(Number(item.dkmsId)) && Number(item.dkmsId) > 0 ? Number(item.dkmsId) : null,
    status: String(item.status ?? "pending_cert") as SaeAdminStatus,
    certFingerprint: item.certFingerprint ?? null,
    certNotAfter: item.certNotAfter ?? null
  }));
}

function sanitizeFileStem(raw: string): string {
  const value = String(raw ?? "").trim();
  if (!value) {
    return "sae";
  }
  return value.replace(/[^a-zA-Z0-9._-]+/g, "_");
}

function downloadBlob(filename: string, blob: Blob): void {
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  URL.revokeObjectURL(url);
}

function buildMtlsScriptContent(args: {
  runtimeBasePath: string;
  callerSaeId: string;
  certFilename: string;
  keyFilename: string;
  caFilename: string;
}): string {
  return [
    "#!/usr/bin/env bash",
    "set -euo pipefail",
    "",
    `RUNTIME_BASE_URL="${args.runtimeBasePath}"`,
    `CALLER_SAE_ID="${args.callerSaeId}"`,
    `CERT_FILE="${args.certFilename}"`,
    `KEY_FILE="${args.keyFilename}"`,
    "",
    "usage() {",
    '  echo "Usage:"',
    '  echo "  $0 enc <target_sae_id> [number] [size] [additional_saes_csv]"',
    '  echo "  $0 dec <key_id_or_csv>"',
    '  echo ""',
    '  echo "Backwards compatible shortcut:"',
    '  echo "  $0 <target_sae_id> [number] [size] [additional_saes_csv]"',
    '  echo ""',
    '  echo "Examples:"',
    '  echo "  $0 enc 41"',
    '  echo "  $0 enc 41 1 32 42,43"',
    '  echo "  $0 dec 1001-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"',
    '  echo "  $0 dec 1001-uuidA,1001-uuidB"',
    "}",
    "",
    "build_json_array_from_csv() {",
    '  local csv="${1:-}"',
    '  local json_list=""',
    '  IFS=\',\' read -r -a raw_items <<< "${csv}"',
    '  for raw_item in "${raw_items[@]}"; do',
    '    local item="${raw_item//[[:space:]]/}"',
    '    if [[ -z "${item}" ]]; then',
    "      continue",
    "    fi",
    '    if [[ -n "${json_list}" ]]; then',
    '      json_list="${json_list},"',
    "    fi",
    '    json_list="${json_list}\\"${item}\\""',
    "  done",
    '  printf "%s" "${json_list}"',
    "}",
    "",
    "request_enc_keys() {",
    '  local target_sae_id="${1:-}"',
    '  local number="${2:-1}"',
    '  local size="${3:-32}"',
    '  local additional_saes_csv="${4:-${ADDITIONAL_SAES:-}}"',
    '  if [[ -z "${target_sae_id}" ]]; then',
    '    echo "Missing target_sae_id for enc mode." >&2',
    "    usage",
    "    exit 1",
    "  fi",
    '  local payload="{\\"number\\":${number},\\"size\\":${size}}"',
    '  if [[ -n "${additional_saes_csv}" ]]; then',
    '    local addl_json',
    '    addl_json="$(build_json_array_from_csv "${additional_saes_csv}")"',
    '    if [[ -n "${addl_json}" ]]; then',
    '      payload="{\\"number\\":${number},\\"size\\":${size},\\"additional_slave_SAE_IDs\\":[${addl_json}]}"',
    "    fi",
    "  fi",
    '  echo "Requesting enc_keys from ${RUNTIME_BASE_URL} for target SAE=${target_sae_id} (additional=${additional_saes_csv:-none})"',
    "  curl --silent --show-error --fail \\",
    '    --cert "${CERT_FILE}" \\',
    '    --key "${KEY_FILE}" \\',
    '    -H "Content-Type: application/json" \\',
    "    -X POST \\",
    '    -d "${payload}" \\',
    '    "${RUNTIME_BASE_URL}/api/v1/keys/${target_sae_id}/enc_keys"',
    "}",
    "",
    "request_dec_keys() {",
    '  local key_ids_csv="${1:-${KEY_IDS:-}}"',
    '  if [[ -z "${key_ids_csv}" ]]; then',
    '    echo "Missing key_id or key_ids for dec mode." >&2',
    "    usage",
    "    exit 1",
    "  fi",
    '  if [[ "${key_ids_csv}" == *","* ]]; then',
    '    local ids_json',
    '    ids_json="$(build_json_array_from_csv "${key_ids_csv}")"',
    '    local payload="{\\"key_IDs\\":{\\"key_IDs\\":[${ids_json}]}}"',
    '    echo "Requesting dec_keys (multiple ids) from ${RUNTIME_BASE_URL} for SAE=${CALLER_SAE_ID}"',
    "    curl --silent --show-error --fail \\",
    '      --cert "${CERT_FILE}" \\',
    '      --key "${KEY_FILE}" \\',
    '      -H "Content-Type: application/json" \\',
    "      -X POST \\",
    '      -d "${payload}" \\',
    '      "${RUNTIME_BASE_URL}/api/v1/keys/${CALLER_SAE_ID}/dec_keys"',
    "  else",
    '    echo "Requesting dec_keys (single id=${key_ids_csv}) from ${RUNTIME_BASE_URL} for SAE=${CALLER_SAE_ID}"',
    "    curl --silent --show-error --fail \\",
    '      --cert "${CERT_FILE}" \\',
    '      --key "${KEY_FILE}" \\',
    "      --get \\",
    '      --data-urlencode "key_id=${key_ids_csv}" \\',
    '      "${RUNTIME_BASE_URL}/api/v1/keys/${CALLER_SAE_ID}/dec_keys"',
    "  fi",
    "}",
    "",
    'MODE="${1:-enc}"',
    'if [[ "${MODE}" == "enc" ]]; then',
    "  shift || true",
    '  request_enc_keys "${1:-}" "${2:-1}" "${3:-32}" "${4:-}"',
    'elif [[ "${MODE}" == "dec" ]]; then',
    "  shift || true",
    '  request_dec_keys "${1:-}"',
    "elif [[ \"${MODE}\" == \"-h\" || \"${MODE}\" == \"--help\" ]]; then",
    "  usage",
    "else",
    "  # Backwards compatibility: first arg is target SAE id for enc_keys.",
    '  request_enc_keys "${MODE}" "${2:-1}" "${3:-32}" "${4:-}"',
    "fi",
    "",
    `# Note: ${args.caFilename} is the SAE/runtime CA bundle, not the public HTTPS server CA.`,
    "# Optional: add --cacert <server-ca.pem> only if your runtime HTTPS cert is not publicly trusted.",
    ""
  ].join("\n");
}

function edgeLabel(data: EditorEdgeData): string {
  if (data.linkType === "PQC") {
    return "PQC";
  }
  return (
    `${data.linkType} · ${normalizeDistanceKm(data.distanceKm)} km · ` +
    `R0 ${normalizeQudittoRateR0(data.qudittoRateR0)} · ` +
    `a ${normalizeQudittoRateAlpha(data.qudittoRateAlpha)} · ` +
    `buffer ${normalizeQudittoMaxBufferSize(data.qudittoMaxBufferSize)}`
  );
}

function edgePairKey(source: string, target: string): string {
  return source < target ? `${source}::${target}` : `${target}::${source}`;
}

function hasEdgeBetween(nodes: FlowEdge[], source: string, target: string, excludingEdgeId?: string): boolean {
  const key = edgePairKey(source, target);
  return nodes.some((edge) => {
    if (excludingEdgeId && edge.id === excludingEdgeId) {
      return false;
    }
    if (edge.data?.transient) {
      return false;
    }
    return edgePairKey(edge.source, edge.target) === key;
  });
}

function resolvePublicBaseUrl(ingressBaseUrl: string): string {
  if (typeof window !== "undefined" && window.location?.origin) {
    return window.location.origin.replace(/\/+$/, "");
  }
  const base = ingressBaseUrl.trim().replace(/\/+$/, "");
  if (!base || base.includes(".svc.cluster.local")) {
    return "";
  }
  return base;
}

function buildGrafanaUrl(ingressBaseUrl: string, simulationId: number): string | null {
  const base = resolvePublicBaseUrl(ingressBaseUrl);
  if (!base) {
    return null;
  }
  return `${base}/api/sim/${simulationId}/grafana`;
}

function simulationToFlow(
  simulation: SimulationDTO,
  simulationId: number,
  ingressBaseUrl: string
): { nodes: FlowNode[]; edges: FlowEdge[] } {
  const nodes: FlowNode[] = simulation.nodes.map((node) => ({
    id: node.uid,
    type: "dkms",
    position: { x: node.x, y: node.y },
    data: {
      label: node.label,
      nodeType: node.type,
      nodeId: node.nodeId,
      grafanaUrl: buildGrafanaUrl(ingressBaseUrl, simulationId)
    },
    style: nodeStyleByType()
  }));

  const edges: FlowEdge[] = simulation.links.map((link) => ({
    id: link.uid,
    source: link.sourceUid,
    target: link.targetUid,
    data: {
      linkType: link.linkType,
      distanceKm: normalizeDistanceKm(link.distanceKm),
      qudittoMaxBufferSize: normalizeQudittoMaxBufferSize(link.qudittoMaxBufferSize),
      qudittoRateR0: normalizeQudittoRateR0(link.qudittoRateR0),
      qudittoRateAlpha: normalizeQudittoRateAlpha(link.qudittoRateAlpha)
    },
    label: edgeLabel({
      linkType: link.linkType,
      distanceKm: normalizeDistanceKm(link.distanceKm),
      qudittoMaxBufferSize: normalizeQudittoMaxBufferSize(link.qudittoMaxBufferSize),
      qudittoRateR0: normalizeQudittoRateR0(link.qudittoRateR0),
      qudittoRateAlpha: normalizeQudittoRateAlpha(link.qudittoRateAlpha)
    }),
    ...edgeVisualProps(link.linkType)
  }));

  return { nodes, edges };
}

function applyNodeRuntimeState(
  node: FlowNode,
  isInfrastructureRunning: boolean,
  isInfrastructureTransitioning: boolean,
  connectionModeEnabled: boolean,
  grafanaStatus: GrafanaStatus
): FlowNode {
  const canDragNode =
    node.data.nodeType === "SAE"
      ? !isInfrastructureTransitioning
      : !isInfrastructureRunning && !isInfrastructureTransitioning;
  const nextHealthState: EditorNodeData["healthState"] = isInfrastructureRunning
    ? node.data.healthState === "up"
      ? "up"
      : node.data.healthState === "down"
        ? "down"
        : "unknown"
    : "unknown";
  return {
    ...node,
    data: {
      ...node.data,
      grafanaStatus: !isInfrastructureRunning ? "disabled" : isInfrastructureTransitioning ? "checking" : grafanaStatus,
      connectionHandleEnabled: !isInfrastructureTransitioning && connectionModeEnabled,
      healthState: nextHealthState
    },
    draggable: canDragNode
  };
}

function applyRuntimeStateToNodes(
  currentNodes: FlowNode[],
  isInfrastructureRunning: boolean,
  isInfrastructureTransitioning: boolean,
  connectionModeEnabled: boolean,
  grafanaStatus: GrafanaStatus
): FlowNode[] {
  return currentNodes.map((node) =>
    applyNodeRuntimeState(node, isInfrastructureRunning, isInfrastructureTransitioning, connectionModeEnabled, grafanaStatus)
  );
}

function applyHealthSnapshotToNodes(currentNodes: FlowNode[], healthByNodeId: Record<string, boolean>): FlowNode[] {
  let changed = false;
  const nextNodes = currentNodes.map((node) => {
    const nodeId = node.data.nodeId;
    if (nodeId === null || !Number.isFinite(nodeId)) {
      return node;
    }
    const healthy = Boolean(healthByNodeId[String(nodeId)]);
    const nextHealthState: EditorNodeData["healthState"] = healthy ? "up" : "down";
    if (node.data.healthState === nextHealthState) {
      return node;
    }
    changed = true;
    return {
      ...node,
      data: {
        ...node.data,
        healthState: nextHealthState
      }
    };
  });
  return changed ? nextNodes : currentNodes;
}

export function SimulationEditor({ simulationId, ingressBaseUrl }: Props) {
  const router = useRouter();
  const nodeTypes = useMemo(() => ({ dkms: DkmsNode, sae: SaeNode }), []);
  const flowRef = useRef<ReactFlowInstance | null>(null);
  const nodesRef = useRef<FlowNode[]>([]);
  const edgesRef = useRef<FlowEdge[]>([]);
  const autosaveTimer = useRef<number | null>(null);
  const initializedRef = useRef(false);
  const connectionModeRef = useRef(true);
  const resizingInspectorRef = useRef(false);
  const inspectorDragStartRef = useRef<{ x: number; width: number } | null>(null);
  const actionPendingRef = useRef(false);
  const dkmsActionPendingRef = useRef(false);
  const infrastructureRunningRef = useRef(false);
  const infrastructureTransitioningRef = useRef(false);
  const stopNodeCallbacksRef = useRef<Map<number, () => void>>(new Map());
  const startNodeCallbacksRef = useRef<Map<number, () => void>>(new Map());
  const lastHealthSnapshotRef = useRef<string>("");
  const lastPersistSignatureRef = useRef<string | null>(null);
  const saeOverlayPositionsRef = useRef<Record<string, { x: number; y: number }>>({});

  const [loading, setLoading] = useState(true);
  const [saveState, setSaveState] = useState<"idle" | "saving" | "error">("idle");
  const [actionMessage, setActionMessage] = useState<string | null>(null);
  const [actionPending, setActionPending] = useState(false);
  const [dkmsActionPending, setDkmsActionPending] = useState(false);
  const [infrastructureTransition, setInfrastructureTransition] = useState<"starting" | "stopping" | null>(null);
  const [transitionProgress, setTransitionProgress] = useState(0);
  const [transitionDetail, setTransitionDetail] = useState<string>("Preparing operation...");
  const [runRefreshToken, setRunRefreshToken] = useState(0);

  const [simulationName, setSimulationName] = useState("");
  const [simulationDescription, setSimulationDescription] = useState("");
  const [simulationStatus, setSimulationStatus] = useState<SimulationStatus>("pending");
  const [sdnIp, setSdnIp] = useState("172.30.0.2");
  const [sdnPort, setSdnPort] = useState(3000);
  const [sdnTypeHttp, setSdnTypeHttp] = useState<"http" | "https">("http");
  const [globalQkdBufferSize, setGlobalQkdBufferSize] = useState(100);
  const [connectionModeEnabled, setConnectionModeEnabled] = useState(true);
  const [connectionLinkType, setConnectionLinkType] = useState<EditorEdgeData["linkType"]>("QKD");
  const [saeOverlayEnabled, setSaeOverlayEnabled] = useState(false);
  const [saeOverlayLoading, setSaeOverlayLoading] = useState(false);
  const [allSaes, setAllSaes] = useState<SaeAdminRecord[]>([]);
  const [dkmsRuntimeMap, setDkmsRuntimeMap] = useState<Record<string, DkmsRuntimeInfo>>({});
  const [grafanaStatus, setGrafanaStatus] = useState<GrafanaStatus>("disabled");
  const [bulkSaeActionPending, setBulkSaeActionPending] = useState(false);
  const [inspectorWidth, setInspectorWidth] = useState(440);
  const [nodes, setNodes] = useState<FlowNode[]>([]);
  const [edges, setEdges] = useState<FlowEdge[]>([]);

  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  const [selectedNodeIds, setSelectedNodeIds] = useState<string[]>([]);
  const [selectedEdgeId, setSelectedEdgeId] = useState<string | null>(null);

  // Reemplazo accesible de window.confirm para acciones destructivas. El
  // ConfirmDialog al pie del JSX consume este estado; los call-sites
  // llaman a ``requestConfirm`` en lugar de confirmar sincrónicamente.
  interface PendingConfirm {
    title: string;
    description?: string;
    confirmLabel?: string;
    destructive?: boolean;
    onConfirm: () => void | Promise<void>;
  }
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirm | null>(null);
  const [confirmLoading, setConfirmLoading] = useState(false);

  const requestConfirm = useCallback((cfg: PendingConfirm) => {
    setPendingConfirm(cfg);
  }, []);

  useEffect(() => {
    nodesRef.current = nodes;
    edgesRef.current = edges;
  }, [nodes, edges]);

  const selectedNode = useMemo(
    () => nodes.find((node) => node.id === selectedNodeId) ?? null,
    [nodes, selectedNodeId]
  );
  const selectedEdge = useMemo(
    () => edges.find((edge) => edge.id === selectedEdgeId) ?? null,
    [edges, selectedEdgeId]
  );
  const selectedNodes = useMemo(() => {
    if (selectedNodeIds.length === 0) {
      return [] as FlowNode[];
    }
    const selectedIds = new Set(selectedNodeIds);
    return nodes.filter((node) => selectedIds.has(node.id));
  }, [nodes, selectedNodeIds]);
  const selectedSaeIds = useMemo(() => {
    const selected = new Set<string>();
    for (const node of selectedNodes) {
      if (node.data.nodeType !== "SAE") {
        continue;
      }
      const saeId = String(node.data.saeId ?? "").trim();
      if (!saeId) {
        continue;
      }
      selected.add(saeId);
    }
    return Array.from(selected.values()).sort((a, b) => a.localeCompare(b));
  }, [selectedNodes]);
  const selectedDkmsNodeIds = useMemo(() => {
    const selected = new Set<number>();
    for (const node of selectedNodes) {
      if (node.data.nodeType !== "DKMS") {
        continue;
      }
      const nodeId = Number(node.data.nodeId);
      if (!Number.isFinite(nodeId) || nodeId <= 0) {
        continue;
      }
      selected.add(Math.trunc(nodeId));
    }
    return Array.from(selected.values()).sort((a, b) => a - b);
  }, [selectedNodes]);
  const activeSaeCount = allSaes.filter((sae) => sae.status === "active").length;
  const isInfrastructureRunning = simulationStatus === "running";
  const isInfrastructureTransitioning = infrastructureTransition !== null;
  const isStartingTransition = infrastructureTransition === "starting";
  const isStoppingTransition = infrastructureTransition === "stopping";
  const isTopologyLocked = isInfrastructureTransitioning;
  const isEditorLocked = isInfrastructureTransitioning;
  const canMoveNodes = !isInfrastructureRunning && !isInfrastructureTransitioning;
  const canEditSimulationMeta = !isInfrastructureTransitioning;
  const nodeCount = nodes.filter((node) => node.data.nodeType === "DKMS").length;
  const linkCount = edges.filter((edge) => !edge.data?.transient).length;
  const selectionLabel =
    selectedNodeIds.length > 1
      ? `${selectedNodeIds.length} nodes selected`
      : selectedNode
        ? "Node selected"
        : selectedEdge
          ? "Link selected"
          : "No selection";
  const selectionDetail =
    selectedNodeIds.length > 1
      ? `${selectedDkmsNodeIds.length} DKMS · ${selectedSaeIds.length} SAE`
      : selectedNode
        ? `${selectedNode.data.label} (${selectedNode.data.nodeType})`
        : selectedEdge
          ? `${selectedEdge.source} → ${selectedEdge.target}`
          : "Click a node or link to edit it in the right panel.";
  const statusChipVariant: "success" | "destructive" | "muted" =
    simulationStatus === "running"
      ? "success"
      : simulationStatus === "finished" || simulationStatus === "error"
        ? "destructive"
        : "muted";

  useEffect(() => {
    connectionModeRef.current = connectionModeEnabled;
  }, [connectionModeEnabled]);

  const clampInspectorWidth = useCallback((nextWidth: number): number => {
    if (typeof window === "undefined") {
      return Math.max(320, Math.min(760, nextWidth));
    }
    const min = 320;
    const max = Math.max(420, Math.min(760, Math.floor(window.innerWidth * 0.56)));
    return Math.max(min, Math.min(max, nextWidth));
  }, []);

  useEffect(() => {
    const handleMouseMove = (event: MouseEvent) => {
      if (!resizingInspectorRef.current || !inspectorDragStartRef.current) {
        return;
      }
      const delta = inspectorDragStartRef.current.x - event.clientX;
      const nextWidth = inspectorDragStartRef.current.width + delta;
      setInspectorWidth(clampInspectorWidth(nextWidth));
    };

    const handleMouseUp = () => {
      if (!resizingInspectorRef.current) {
        return;
      }
      resizingInspectorRef.current = false;
      inspectorDragStartRef.current = null;
      document.body.classList.remove("cursor-col-resize", "select-none");
    };

    window.addEventListener("mousemove", handleMouseMove);
    window.addEventListener("mouseup", handleMouseUp);
    return () => {
      window.removeEventListener("mousemove", handleMouseMove);
      window.removeEventListener("mouseup", handleMouseUp);
    };
  }, [clampInspectorWidth]);

  useEffect(() => {
    actionPendingRef.current = actionPending;
    dkmsActionPendingRef.current = dkmsActionPending;
    infrastructureRunningRef.current = isInfrastructureRunning;
    infrastructureTransitioningRef.current = isInfrastructureTransitioning;
  }, [actionPending, dkmsActionPending, isInfrastructureRunning, isInfrastructureTransitioning]);

  const loadSimulation = useCallback(async () => {
    setLoading(true);
    setActionMessage(null);

    try {
      const response = await fetch(apiPath(`/api/simulations/${simulationId}`), { cache: "no-store" });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(payload?.error || "Failed to load simulation");
      }

      const simulation: SimulationDTO = payload.simulation;
      const flow = simulationToFlow(simulation, simulationId, ingressBaseUrl);
      const initialPersistPayload = {
        name: simulation.name,
        description: simulation.description ?? "",
        sdn: {
          ip: simulation.sdn.ip,
          port: simulation.sdn.port,
          typeHttp: simulation.sdn.typeHttp
        },
        nodes: flowNodesToInput(flow.nodes),
        links: flowEdgesToInput(flow.edges)
      };
      lastPersistSignatureRef.current = JSON.stringify(initialPersistPayload);
      const firstQkdLink = simulation.links.find((link) => link.linkType === "QKD" || link.linkType === "HYBRID");
      const initialGlobalQkdBuffer = normalizeQudittoMaxBufferSize(firstQkdLink?.qudittoMaxBufferSize ?? 100);

      setSimulationName(simulation.name);
      setSimulationDescription(simulation.description ?? "");
      setSimulationStatus(simulation.status);
      setGrafanaStatus(simulation.status === "running" ? "checking" : "disabled");
      setSdnIp(simulation.sdn.ip);
      setSdnPort(simulation.sdn.port);
      setSdnTypeHttp(simulation.sdn.typeHttp);
      setGlobalQkdBufferSize(initialGlobalQkdBuffer);
      setNodes(
        applyRuntimeStateToNodes(
          flow.nodes,
          simulation.status === "running",
          false,
          connectionModeRef.current,
          simulation.status === "running" ? "checking" : "disabled"
        )
      );
      setEdges(flow.edges);
      setSelectedNodeId(null);
      setSelectedNodeIds([]);
      setSelectedEdgeId(null);
      initializedRef.current = true;
    } catch (error) {
      setActionMessage(error instanceof Error ? error.message : "Unexpected error");
    } finally {
      setLoading(false);
    }
  }, [simulationId, ingressBaseUrl]);

  useEffect(() => {
    if (isTopologyLocked && connectionModeEnabled) {
      setConnectionModeEnabled(false);
    }
  }, [isTopologyLocked, connectionModeEnabled]);

  useEffect(() => {
    setNodes((current) =>
      applyRuntimeStateToNodes(
        current,
        isInfrastructureRunning,
        isInfrastructureTransitioning,
        connectionModeEnabled,
        grafanaStatus
      )
    );
  }, [isInfrastructureRunning, isInfrastructureTransitioning, connectionModeEnabled, grafanaStatus]);

  const fetchGrafanaStatus = useCallback(async (): Promise<GrafanaStatus> => {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      return "disabled";
    }
    const grafanaUrl = buildGrafanaUrl(ingressBaseUrl, simulationId);
    if (!grafanaUrl) {
      return "unavailable";
    }
    try {
      const response = await fetch(`${grafanaUrl}/api/health`, { cache: "no-store" });
      if (!response.ok) {
        return "unavailable";
      }
      const payload = await response.json().catch(() => null);
      const database = payload && typeof payload === "object" ? (payload as { database?: unknown }).database : null;
      if (typeof database === "string" && database.toLowerCase() !== "ok") {
        return "unavailable";
      }
      return "available";
    } catch {
      return "unavailable";
    }
  }, [simulationId, ingressBaseUrl, isInfrastructureRunning, isInfrastructureTransitioning]);

  useEffect(() => {
    void loadSimulation();
  }, [loadSimulation]);

  useEffect(() => {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      setGrafanaStatus("disabled");
      return;
    }
    let cancelled = false;
    let timer: number | null = null;

    setGrafanaStatus((current) => (current === "available" ? current : "checking"));

    const poll = async () => {
      const nextStatus = await fetchGrafanaStatus();
      if (cancelled) {
        return;
      }
      setGrafanaStatus(nextStatus);
      timer = window.setTimeout(() => {
        void poll();
      }, nextStatus === "available" ? 15000 : 5000);
    };

    void poll();

    return () => {
      cancelled = true;
      if (timer) {
        window.clearTimeout(timer);
      }
    };
  }, [isInfrastructureRunning, isInfrastructureTransitioning, fetchGrafanaStatus]);

  const applySaeOverlayToGraph = useCallback(
    (
      sourceNodes: FlowNode[],
      sourceEdges: FlowEdge[],
      saeRecords: SaeAdminRecord[],
      runtimeByNode: Record<string, DkmsRuntimeInfo>
    ): { nodes: FlowNode[]; edges: FlowEdge[] } => {
      const topologyNodes = sourceNodes.filter((node) => node.data.nodeType === "DKMS");
      const topologyEdges = sourceEdges.filter((edge) => !edge.data?.transient);

      if (!saeOverlayEnabled) {
        return { nodes: topologyNodes, edges: topologyEdges };
      }

      const groupedByDkms = new Map<number, SaeAdminRecord[]>();
      for (const sae of saeRecords) {
        const nodeId = resolveSaeNodeId(sae.dkmsId, runtimeByNode);
        if (!nodeId || nodeId <= 0) {
          continue;
        }
        const current = groupedByDkms.get(nodeId) ?? [];
        current.push(sae);
        groupedByDkms.set(nodeId, current);
      }

      const overlayNodes: FlowNode[] = [];
      const overlayEdges: FlowEdge[] = [];
      const overlayPositions = saeOverlayPositionsRef.current;
      for (const dkmsNode of topologyNodes) {
        const dkmsId = Number(dkmsNode.data.nodeId);
        if (!Number.isFinite(dkmsId) || dkmsId <= 0) {
          continue;
        }
        const dkmsSaes = groupedByDkms.get(dkmsId) ?? [];
        for (const [index, sae] of dkmsSaes.entries()) {
          const nodeId = `sae-${sae.saeId}`;
          const existingPosition = overlayPositions[sae.saeId];
          const fallbackPosition = {
            x: dkmsNode.position.x + 240 + (index % 2) * 175,
            y: dkmsNode.position.y - 28 + Math.floor(index / 2) * 92
          };
          overlayNodes.push({
            id: nodeId,
            type: "sae",
            position: existingPosition ?? fallbackPosition,
            draggable: !isInfrastructureTransitioning,
            data: {
              label: sae.displayName || `SAE ${sae.saeId}`,
              nodeType: "SAE",
              nodeId: null,
              saeId: sae.saeId,
              saeDisplayName: sae.displayName,
              saeStatus: sae.status,
              saeCertFingerprint: sae.certFingerprint,
              saeCertNotAfter: sae.certNotAfter,
              parentDkmsId: dkmsId,
              runtimeBasePath: runtimeByNode[String(dkmsId)]?.runtimeBasePath ?? null
            },
            style: {
              border: "none",
              background: "transparent",
              padding: 0
            }
          });

          overlayEdges.push({
            id: `sae-link-${sae.saeId}`,
            source: nodeId,
            target: dkmsNode.id,
            data: {
              linkType: "QKD",
              distanceKm: 0,
              qudittoMaxBufferSize: 100,
              qudittoRateR0: DEFAULT_QUDITTO_RATE_R0,
              qudittoRateAlpha: DEFAULT_QUDITTO_RATE_ALPHA,
              transient: true
            },
            type: "straight",
            style: {
              stroke: "#64748b",
              strokeWidth: 1.6,
              strokeDasharray: "6 4",
              opacity: 0.9
            }
          });
        }
      }

      return {
        nodes: [...topologyNodes, ...overlayNodes],
        edges: [...topologyEdges, ...overlayEdges]
      };
    },
    [isInfrastructureTransitioning, saeOverlayEnabled]
  );

  const fetchSaeAndRuntimeData = useCallback(
    async (): Promise<{
      saes: SaeAdminRecord[];
      runtimeByNode: Record<string, DkmsRuntimeInfo>;
    }> => {
      const [saesResponse, runtimeResponse] = await Promise.all([
        fetch(apiPath(`/api/simulations/${simulationId}/saes`), {
          method: "GET",
          cache: "no-store"
        }),
        fetch(apiPath(`/api/simulations/${simulationId}/dkms-runtime`), {
          method: "GET",
          cache: "no-store"
        })
      ]);

      const saesPayload = await saesResponse.json().catch(() => ({}));
      const runtimePayload = await runtimeResponse.json().catch(() => ({}));
      let saeRecords = saesResponse.ok ? mapSaeRecords(saesPayload) : [];
      const runtimeByNode: Record<string, DkmsRuntimeInfo> = {};
      if (runtimeResponse.ok && runtimePayload?.byNodeId && typeof runtimePayload.byNodeId === "object") {
        for (const [nodeId, value] of Object.entries(runtimePayload.byNodeId as Record<string, any>)) {
          const parsedNodeId = Number.parseInt(String((value as any)?.nodeId ?? nodeId), 10);
          const ingressId = Number.parseInt(String((value as any)?.ingressId ?? 0), 10);
          const runtimeBasePath = String((value as any)?.runtimeBasePath ?? "").trim();
          if (!Number.isFinite(parsedNodeId) || parsedNodeId <= 0 || !runtimeBasePath || !Number.isFinite(ingressId)) {
            continue;
          }
          runtimeByNode[String(parsedNodeId)] = {
            nodeId: parsedNodeId,
            dkmsId: Number.parseInt(String((value as any)?.dkmsId ?? 0), 10),
            ingressId,
            runtimeBasePath
          };
        }
      }

      const topologyNodeIds = Array.from(
        new Set(
          nodesRef.current
            .filter((node) => node.data.nodeType === "DKMS")
            .map((node) => Number(node.data.nodeId))
            .filter((nodeId) => Number.isFinite(nodeId) && nodeId > 0)
            .map((nodeId) => Math.trunc(nodeId))
        )
      );
      const topologyNodeIdSet = new Set<number>(topologyNodeIds);
      const hasUnresolvedSae =
        saeRecords.length > 0 &&
        saeRecords.some((sae) => {
          const resolvedNodeId = resolveSaeNodeId(sae.dkmsId, runtimeByNode);
          return !resolvedNodeId || !topologyNodeIdSet.has(resolvedNodeId);
        });
      const shouldFallbackByNode =
        topologyNodeIds.length > 0 &&
        (saeRecords.length === 0 || Object.keys(runtimeByNode).length === 0 || hasUnresolvedSae);

      if (shouldFallbackByNode) {
        const perNodeRecords = await Promise.all(
          topologyNodeIds.map(async (nodeId) => {
            try {
              const response = await fetch(apiPath(`/api/simulations/${simulationId}/saes?dkmsId=${nodeId}`), {
                method: "GET",
                cache: "no-store"
              });
              if (!response.ok) {
                return [] as SaeAdminRecord[];
              }
              const payload = await response.json().catch(() => ({}));
              return mapSaeRecords(payload).map((item) => ({
                ...item,
                dkmsId: nodeId
              }));
            } catch {
              return [] as SaeAdminRecord[];
            }
          })
        );
        const bySaeId = new Map<string, SaeAdminRecord>();
        for (const item of perNodeRecords.flat()) {
          if (!item.saeId) {
            continue;
          }
          bySaeId.set(item.saeId, item);
        }
        if (bySaeId.size > 0) {
          saeRecords = Array.from(bySaeId.values());
        }
      }

      return { saes: saeRecords, runtimeByNode };
    },
    [simulationId]
  );

  const refreshSaeOverlayData = useCallback(async () => {
    try {
      setSaeOverlayLoading(true);
      const { saes: saeRecords, runtimeByNode } = await fetchSaeAndRuntimeData();

      setAllSaes(saeRecords);
      setDkmsRuntimeMap(runtimeByNode);
      const nextGraph = applySaeOverlayToGraph(nodesRef.current, edgesRef.current, saeRecords, runtimeByNode);
      setNodes(nextGraph.nodes);
      setEdges(nextGraph.edges);
    } catch {
      if (!saeOverlayEnabled) {
        return;
      }
      setActionMessage("Could not load SAE overlay data.");
    } finally {
      setSaeOverlayLoading(false);
    }
  }, [applySaeOverlayToGraph, fetchSaeAndRuntimeData, saeOverlayEnabled]);

  useEffect(() => {
    if (!saeOverlayEnabled) {
      setNodes((current) => current.filter((node) => node.data.nodeType === "DKMS"));
      setEdges((current) => current.filter((edge) => !edge.data?.transient));
      setSelectedNodeIds((current) =>
        current.filter((id) => {
          const node = nodesRef.current.find((candidate) => candidate.id === id);
          return node?.data.nodeType === "DKMS";
        })
      );
      setSelectedNodeId((current) => (current?.startsWith("sae-") ? null : current));
      return;
    }
    void refreshSaeOverlayData();
  }, [saeOverlayEnabled]);

  useEffect(() => {
    if (selectedNodeIds.length === 0) {
      return;
    }
    const existingNodeIds = new Set(nodes.map((node) => node.id));
    setSelectedNodeIds((current) => {
      const next = current.filter((id) => existingNodeIds.has(id));
      return next.length === current.length ? current : next;
    });
  }, [nodes, selectedNodeIds.length]);

  useEffect(() => {
    let alive = true;
    const preload = async () => {
      try {
        const { saes, runtimeByNode } = await fetchSaeAndRuntimeData();
        if (!alive) {
          return;
        }
        setAllSaes(saes);
        setDkmsRuntimeMap(runtimeByNode);
      } catch {
        // non-blocking preload
      }
    };
    void preload();
    return () => {
      alive = false;
    };
  }, [fetchSaeAndRuntimeData]);

  const fetchDkmsHealth = useCallback(
    async (): Promise<{ available: boolean; totalNodes: number; healthByNodeId: Record<string, boolean> }> => {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      return { available: false, totalNodes: 0, healthByNodeId: {} };
    }
    const response = await fetch(apiPath(`/api/simulations/${simulationId}/dkms-health`), { cache: "no-store" });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) {
      return { available: false, totalNodes: 0, healthByNodeId: {} };
    }
    const rawMap = payload?.healthByNodeId;
    const healthByNodeId: Record<string, boolean> = {};
    if (rawMap && typeof rawMap === "object") {
      for (const [key, value] of Object.entries(rawMap as Record<string, unknown>)) {
        healthByNodeId[String(key)] = Boolean(value);
      }
    }
    const totalNodes = Number.isFinite(Number(payload?.totalNodes)) ? Number(payload.totalNodes) : 0;
    return { available: true, totalNodes, healthByNodeId };
    },
    [simulationId, isInfrastructureRunning, isInfrastructureTransitioning]
  );

  useEffect(() => {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      lastHealthSnapshotRef.current = "";
      return;
    }
    let cancelled = false;
    let timer: number | null = null;

    const poll = async () => {
      const healthSnapshot = await fetchDkmsHealth();
      if (cancelled) {
        return;
      }
      if (healthSnapshot.available) {
        const snapshotKey = JSON.stringify(
          Object.entries(healthSnapshot.healthByNodeId)
            .sort(([a], [b]) => a.localeCompare(b))
        );
        if (lastHealthSnapshotRef.current === snapshotKey) {
          timer = window.setTimeout(() => {
            void poll();
          }, 3000);
          return;
        }
        lastHealthSnapshotRef.current = snapshotKey;
        setNodes((current) => applyHealthSnapshotToNodes(current, healthSnapshot.healthByNodeId));
      }
      timer = window.setTimeout(() => {
        void poll();
      }, 3000);
    };

    void poll();

    return () => {
      cancelled = true;
      if (timer) {
        window.clearTimeout(timer);
      }
    };
  }, [isInfrastructureRunning, isInfrastructureTransitioning, fetchDkmsHealth]);

  const persistGraph = useCallback(async (silent = true): Promise<{ ok: boolean; error?: string }> => {
    if (!initializedRef.current) return { ok: true };
    if (isInfrastructureTransitioning) {
      setSaveState("idle");
      return { ok: true };
    }

    const persistPayload = {
      name: simulationName,
      description: simulationDescription,
      sdn: {
        ip: sdnIp,
        port: sdnPort,
        typeHttp: sdnTypeHttp
      },
      nodes: flowNodesToInput(nodes),
      links: flowEdgesToInput(edges)
    };
    const persistSignature = JSON.stringify(persistPayload);
    if (lastPersistSignatureRef.current === persistSignature) {
      setSaveState("idle");
      return { ok: true };
    }

    setSaveState("saving");

    try {
      const response = await fetch(apiPath(`/api/simulations/${simulationId}`), {
        method: "PATCH",
        headers: {
          "Content-Type": "application/json"
        },
        body: JSON.stringify(persistPayload)
      });

      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(payload?.error || "Save failed");
      }

      lastPersistSignatureRef.current = persistSignature;
      setSaveState("idle");
      return { ok: true };
    } catch (error) {
      setSaveState("error");
      const message = error instanceof Error ? error.message : "Save failed";
      if (!silent) {
        setActionMessage(message);
      }
      return { ok: false, error: message };
    }
  }, [
    edges,
    isInfrastructureTransitioning,
    nodes,
    sdnIp,
    sdnPort,
    sdnTypeHttp,
    simulationDescription,
    simulationId,
    simulationName
  ]);

  useEffect(() => {
    if (!initializedRef.current || loading || isInfrastructureTransitioning) return;
    if (autosaveTimer.current) {
      window.clearTimeout(autosaveTimer.current);
    }

    autosaveTimer.current = window.setTimeout(() => {
      void persistGraph(true);
    }, 850);

    return () => {
      if (autosaveTimer.current) {
        window.clearTimeout(autosaveTimer.current);
      }
    };
  }, [
    simulationName,
    simulationDescription,
    sdnIp,
    sdnPort,
    sdnTypeHttp,
    nodes,
    edges,
    loading,
    persistGraph,
    isInfrastructureTransitioning
  ]);

  const onNodesChange = useCallback((changes: NodeChange[]) => {
    if (isInfrastructureTransitioning) {
      return;
    }
    setNodes((current) => {
      const allowedChanges = isInfrastructureRunning
        ? changes.filter((change) => {
            if (!("id" in change)) {
              return false;
            }
            const targetNode = current.find((node) => node.id === change.id);
            if (!targetNode || targetNode.data.nodeType !== "SAE") {
              return false;
            }
            return change.type === "position" || change.type === "select";
          })
        : changes;
      if (allowedChanges.length === 0) {
        return current;
      }
      return applyNodeChanges(allowedChanges, current);
    });
  }, [isInfrastructureRunning, isInfrastructureTransitioning]);

  const onEdgesChange = useCallback((changes: EdgeChange[]) => {
    if (isInfrastructureTransitioning) {
      return;
    }
    if (!isInfrastructureRunning) {
      setEdges((current) => {
        const allowed = changes.filter((change) => {
          if (change.type !== "remove") {
            return true;
          }
          const edge = current.find((candidate) => candidate.id === change.id);
          return !edge?.data?.transient;
        });
        return applyEdgeChanges(allowed, current);
      });
      return;
    }

    let blockedDeletion = false;
    setEdges((current) => {
      const allowed = changes.filter((change) => {
        if (change.type !== "remove") {
          return true;
        }
        const edge = current.find((candidate) => candidate.id === change.id);
        if (edge?.data?.transient) {
          return false;
        }
        const linkType = edge?.data?.linkType ?? "QKD";
        if (linkType === "PQC") {
          return true;
        }
        blockedDeletion = true;
        return false;
      });
      return applyEdgeChanges(allowed, current);
    });
    if (blockedDeletion) {
      setActionMessage("Running mode only allows deleting PQC links.");
    }
  }, [isInfrastructureRunning, isInfrastructureTransitioning]);

  useEffect(() => {
    if (!saeOverlayEnabled) {
      return;
    }
    let changed = false;
    const next = { ...saeOverlayPositionsRef.current };
    for (const node of nodes) {
      if (node.data.nodeType !== "SAE") {
        continue;
      }
      const saeId = String(node.data.saeId ?? "").trim();
      if (!saeId) {
        continue;
      }
      const prev = next[saeId];
      if (!prev || prev.x !== node.position.x || prev.y !== node.position.y) {
        next[saeId] = { x: node.position.x, y: node.position.y };
        changed = true;
      }
    }
    if (changed) {
      saeOverlayPositionsRef.current = next;
    }
  }, [nodes, saeOverlayEnabled]);

  const onConnect = useCallback(
    (connection: Connection) => {
      if (isTopologyLocked || !connectionModeEnabled) {
        return;
      }
      const source = connection.source;
      const target = connection.target;
      if (!source || !target) {
        return;
      }
      const sourceNode = nodes.find((node) => node.id === source);
      const targetNode = nodes.find((node) => node.id === target);
      if (sourceNode?.data.nodeType !== "DKMS" || targetNode?.data.nodeType !== "DKMS") {
        return;
      }
      if (hasEdgeBetween(edges, source, target)) {
        setActionMessage("Only one link is allowed between the same two nodes.");
        return;
      }

      const linkType: EditorEdgeData["linkType"] = isInfrastructureRunning ? "PQC" : connectionLinkType;
      const edgeData: EditorEdgeData =
        linkType === "PQC"
          ? {
              linkType,
              distanceKm: 0,
              qudittoMaxBufferSize: 100,
              qudittoRateR0: DEFAULT_QUDITTO_RATE_R0,
              qudittoRateAlpha: DEFAULT_QUDITTO_RATE_ALPHA
            }
          : {
              linkType,
              distanceKm: 0,
              qudittoMaxBufferSize: globalQkdBufferSize,
              qudittoRateR0: DEFAULT_QUDITTO_RATE_R0,
              qudittoRateAlpha: DEFAULT_QUDITTO_RATE_ALPHA
            };
      setEdges((current) =>
        addEdge(
          {
            id: randomId("edge"),
            source,
            target,
            label: edgeLabel(edgeData),
            data: edgeData,
            ...edgeVisualProps(linkType)
          },
          current
        )
      );
    },
    [connectionLinkType, connectionModeEnabled, edges, globalQkdBufferSize, isInfrastructureRunning, isTopologyLocked]
  );

  function applyGlobalQkdBufferToAllLinks() {
    if (isInfrastructureRunning || isInfrastructureTransitioning) {
      setActionMessage("QKD parameters cannot be modified while infrastructure is running or transitioning.");
      return;
    }
    setEdges((current) =>
      current.map((edge) => {
        const linkType = edge.data?.linkType ?? "QKD";
        if (linkType !== "QKD" && linkType !== "HYBRID") {
          return edge;
        }
        const distanceKm = normalizeDistanceKm(edge.data?.distanceKm);
        const qudittoMaxBufferSize = normalizeQudittoMaxBufferSize(globalQkdBufferSize);
        const qudittoRateR0 = normalizeQudittoRateR0(edge.data?.qudittoRateR0);
        const qudittoRateAlpha = normalizeQudittoRateAlpha(edge.data?.qudittoRateAlpha);
        return {
          ...edge,
          data: { linkType, distanceKm, qudittoMaxBufferSize, qudittoRateR0, qudittoRateAlpha },
          label: edgeLabel({ linkType, distanceKm, qudittoMaxBufferSize, qudittoRateR0, qudittoRateAlpha }),
          ...edgeVisualProps(linkType)
        };
      })
    );
    setActionMessage(`Applied global QKD buffer (${globalQkdBufferSize}) to all QKD/HYBRID links.`);
  }

  function addNodeAtPosition(position?: { x: number; y: number }) {
    if (isInfrastructureRunning || isInfrastructureTransitioning) {
      setActionMessage("Infrastructure is locked while running or transitioning.");
      return;
    }
    const dkmsIds = nodes
      .map((node) => node.data.nodeId)
      .filter((value): value is number => typeof value === "number");
    const nextDkmsId = dkmsIds.length > 0 ? Math.max(...dkmsIds) + 1 : 1;

    const id = randomId("node");
    const defaultPosition = {
      x: 120 + Math.random() * 260,
      y: 80 + Math.random() * 220
    };
    const nextNode: FlowNode = {
      id,
      type: "dkms",
      position: position ?? defaultPosition,
      data: {
        label: `DKMS ${nextDkmsId}`,
        nodeType: "DKMS",
        nodeId: nextDkmsId,
        grafanaUrl: buildGrafanaUrl(ingressBaseUrl, simulationId)
      },
      style: nodeStyleByType()
    };

    setNodes((current) => [
      ...current,
      applyNodeRuntimeState(
        nextNode,
        isInfrastructureRunning,
        isInfrastructureTransitioning,
        connectionModeEnabled,
        grafanaStatus
      )
    ]);
    setSelectedNodeId(id);
    setSelectedEdgeId(null);
  }

  function addNode() {
    addNodeAtPosition();
  }

  function removeEdgeById(edgeId: string): boolean {
    const edge = edges.find((item) => item.id === edgeId);
    if (edge?.data?.transient) {
      return false;
    }
    if (isInfrastructureRunning) {
      if ((edge?.data?.linkType ?? "QKD") !== "PQC") {
        setActionMessage("Running mode only allows deleting PQC links.");
        return false;
      }
    }
    setEdges((current) => current.filter((edge) => edge.id !== edgeId));
    return true;
  }

  async function performDeleteSae(normalizedSaeId: string) {
    try {
      const response = await fetch(apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(normalizedSaeId)}`), {
        method: "DELETE"
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        setActionMessage(String(payload?.error ?? "Failed to delete SAE"));
        return;
      }
      setSelectedNodeId(null);
      setSelectedNodeIds([]);
      setSelectedEdgeId(null);
      await refreshSaeOverlayData();
      setActionMessage(`SAE ${normalizedSaeId} eliminado.`);
    } catch {
      setActionMessage("Failed to delete SAE");
    }
  }

  function deleteSaeById(saeId: string) {
    const normalizedSaeId = saeId.trim();
    if (!normalizedSaeId) {
      return;
    }
    requestConfirm({
      title: `Eliminar SAE ${normalizedSaeId}`,
      description: "Esta acción no se puede deshacer y el SAE se perderá definitivamente.",
      confirmLabel: "Eliminar",
      destructive: true,
      onConfirm: () => performDeleteSae(normalizedSaeId)
    });
  }

  function deleteSelection() {
    if (isTopologyLocked) {
      setActionMessage("Infrastructure is locked while running or transitioning.");
      return;
    }
    if (selectedNodeId) {
      const selected = nodes.find((node) => node.id === selectedNodeId);
      if (selected?.data.nodeType === "SAE") {
        void deleteSaeById(String(selected.data.saeId ?? ""));
        return;
      }
      if (isInfrastructureRunning) {
        setActionMessage("Running mode does not allow deleting DKMS nodes.");
        return;
      }
      setNodes((current) => current.filter((node) => node.id !== selectedNodeId));
      setEdges((current) =>
        current.filter((edge) => edge.source !== selectedNodeId && edge.target !== selectedNodeId)
      );
      setSelectedNodeId(null);
      setSelectedNodeIds([]);
      return;
    }

    if (selectedEdgeId) {
      if (removeEdgeById(selectedEdgeId)) {
        setSelectedEdgeId(null);
      }
    }
  }

  function updateSelectedNode(nextNode: FlowNode) {
    if (isInfrastructureRunning || isInfrastructureTransitioning) {
      setActionMessage("Infrastructure is locked while running or transitioning.");
      return;
    }
    setNodes((current) =>
      current.map((node) =>
        node.id === nextNode.id
          ? {
              ...applyNodeRuntimeState(
                nextNode,
                isInfrastructureRunning,
                isInfrastructureTransitioning,
                connectionModeEnabled,
                grafanaStatus
              ),
              style: nodeStyleByType()
            }
          : node
      )
    );
  }

  function updateSelectedEdge(nextEdge: FlowEdge) {
    if (isInfrastructureTransitioning) {
      setActionMessage("Infrastructure is locked while running or transitioning.");
      return;
    }
    const currentEdge = edges.find((edge) => edge.id === nextEdge.id);
    if (!currentEdge) {
      return;
    }

    const currentType = (currentEdge.data?.linkType ?? "QKD") as EditorEdgeData["linkType"];
    const currentDistanceKm = normalizeDistanceKm(currentEdge.data?.distanceKm);
    const currentQudittoMaxBufferSize = normalizeQudittoMaxBufferSize(currentEdge.data?.qudittoMaxBufferSize);
    const currentQudittoRateR0 = normalizeQudittoRateR0(currentEdge.data?.qudittoRateR0);
    const currentQudittoRateAlpha = normalizeQudittoRateAlpha(currentEdge.data?.qudittoRateAlpha);
    let nextType = (nextEdge.data?.linkType ?? currentType) as EditorEdgeData["linkType"];
    let nextDistanceKm = normalizeDistanceKm(nextEdge.data?.distanceKm);
    let nextQudittoMaxBufferSize = normalizeQudittoMaxBufferSize(nextEdge.data?.qudittoMaxBufferSize);
    let nextQudittoRateR0 = normalizeQudittoRateR0(nextEdge.data?.qudittoRateR0);
    let nextQudittoRateAlpha = normalizeQudittoRateAlpha(nextEdge.data?.qudittoRateAlpha);
    if (hasEdgeBetween(edges, nextEdge.source, nextEdge.target, nextEdge.id)) {
      setActionMessage("Only one link is allowed between the same two nodes.");
      return;
    }

    if (isInfrastructureRunning) {
      if (currentType === "PQC") {
        if (nextType !== "PQC") {
          setActionMessage("Running mode does not allow converting PQC links.");
          return;
        }
        nextDistanceKm = 0;
        nextQudittoMaxBufferSize = 100;
        nextQudittoRateR0 = DEFAULT_QUDITTO_RATE_R0;
        nextQudittoRateAlpha = DEFAULT_QUDITTO_RATE_ALPHA;
      } else if (currentType === "QKD") {
        if (nextType !== "QKD" && nextType !== "HYBRID") {
          setActionMessage("Running mode only allows converting QKD links to HYBRID.");
          return;
        }
        nextDistanceKm = currentDistanceKm;
        nextQudittoMaxBufferSize = currentQudittoMaxBufferSize;
        nextQudittoRateR0 = currentQudittoRateR0;
        nextQudittoRateAlpha = currentQudittoRateAlpha;
      } else if (currentType === "HYBRID") {
        if (nextType !== "HYBRID" && nextType !== "QKD") {
          setActionMessage("Running mode only allows converting HYBRID links back to QKD.");
          return;
        }
        nextDistanceKm = currentDistanceKm;
        nextQudittoMaxBufferSize = currentQudittoMaxBufferSize;
        nextQudittoRateR0 = currentQudittoRateR0;
        nextQudittoRateAlpha = currentQudittoRateAlpha;
      }
    } else if (nextType === "PQC") {
      nextDistanceKm = 0;
      nextQudittoMaxBufferSize = 100;
      nextQudittoRateR0 = DEFAULT_QUDITTO_RATE_R0;
      nextQudittoRateAlpha = DEFAULT_QUDITTO_RATE_ALPHA;
    }

    setEdges((current) =>
      current.map((edge) =>
        edge.id === nextEdge.id
          ? {
              ...currentEdge,
              data: {
                linkType: nextType,
                distanceKm: nextDistanceKm,
                qudittoMaxBufferSize: nextQudittoMaxBufferSize,
                qudittoRateR0: nextQudittoRateR0,
                qudittoRateAlpha: nextQudittoRateAlpha
              },
              label: edgeLabel({
                linkType: nextType,
                distanceKm: nextDistanceKm,
                qudittoMaxBufferSize: nextQudittoMaxBufferSize,
                qudittoRateR0: nextQudittoRateR0,
                qudittoRateAlpha: nextQudittoRateAlpha
              }),
              ...edgeVisualProps(nextType)
            }
          : edge
      )
    );
  }

  function quickEditEdgeDistance(edge: Edge) {
    if (isInfrastructureRunning || isTopologyLocked) {
      setActionMessage("Infrastructure is locked while running or transitioning.");
      return;
    }
    const targetEdge = edges.find((current) => current.id === edge.id);
    if (!targetEdge) {
      return;
    }
    if (targetEdge.data?.transient) {
      return;
    }
    const currentDistance =
      Number.isFinite(Number(targetEdge.data?.distanceKm)) && Number(targetEdge.data?.distanceKm) >= 0
        ? Math.trunc(Number(targetEdge.data?.distanceKm))
        : 0;
    const raw = window.prompt("Set link distance_km", String(currentDistance));
    if (raw === null) {
      return;
    }
    const nextDistance = normalizeDistanceKm(raw);
    updateSelectedEdge({
      ...targetEdge,
      data: {
        linkType: (targetEdge.data?.linkType ?? "QKD") as EditorEdgeData["linkType"],
        distanceKm: nextDistance,
        qudittoMaxBufferSize: normalizeQudittoMaxBufferSize(targetEdge.data?.qudittoMaxBufferSize),
        qudittoRateR0: normalizeQudittoRateR0(targetEdge.data?.qudittoRateR0),
        qudittoRateAlpha: normalizeQudittoRateAlpha(targetEdge.data?.qudittoRateAlpha)
      }
    });
    setSelectedEdgeId(targetEdge.id);
    setSelectedNodeId(null);
    setActionMessage(`Link distance updated to ${nextDistance} km.`);
  }

  async function handleExport() {
    setActionMessage(null);
    const response = await fetch(apiPath(`/api/simulations/${simulationId}/export`), { cache: "no-store" });
    const payload = await response.json().catch(() => null);
    if (!response.ok || !payload) {
      setActionMessage("Export failed");
      return;
    }

    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `simulation-${simulationId}.topology.json`;
    document.body.appendChild(link);
    link.click();
    link.remove();
    URL.revokeObjectURL(url);
  }

  async function handleImport(filePayload: unknown) {
    if (isInfrastructureRunning || isTopologyLocked) {
      setActionMessage("Infrastructure is locked while running or transitioning.");
      return;
    }
    setActionMessage(null);
    const response = await fetch(apiPath(`/api/simulations/${simulationId}/import`), {
      method: "POST",
      headers: {
        "Content-Type": "application/json"
      },
      body: JSON.stringify({ payload: filePayload })
    });

    const payload = await response.json().catch(() => ({}));
    if (!response.ok) {
      setActionMessage(payload?.error || "Import failed");
      return;
    }

    await loadSimulation();
    setActionMessage("Import applied");
  }

  function handleFloatingImportFile(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    if (!file) {
      return;
    }

    const reader = new FileReader();
    reader.onload = () => {
      try {
        const parsed = JSON.parse(String(reader.result ?? "{}"));
        void handleImport(parsed);
      } catch {
        setActionMessage("Invalid JSON file");
      }
    };
    reader.readAsText(file);
    event.target.value = "";
  }

  function handlePaneDoubleClick(event: ReactMouseEvent<Element, MouseEvent>) {
    if (isInfrastructureRunning || isInfrastructureTransitioning) {
      return;
    }
    const instance = flowRef.current;
    if (!instance) {
      addNodeAtPosition();
      return;
    }
    const position = instance.screenToFlowPosition({
      x: event.clientX,
      y: event.clientY
    });
    addNodeAtPosition({
      x: Math.trunc(position.x),
      y: Math.trunc(position.y)
    });
  }

  const handleEdgeContextMenu: EdgeMouseHandler = (event, edge) => {
    event.preventDefault();
    if (edge.data?.transient) {
      return;
    }
    setSelectedEdgeId(edge.id);
    setSelectedNodeId(null);
    requestConfirm({
      title: `Eliminar enlace ${edge.id}`,
      description: "El enlace se eliminará del grafo local. Los cambios se aplican al guardar.",
      confirmLabel: "Eliminar",
      destructive: true,
      onConfirm: () => {
        removeEdgeById(edge.id);
      }
    });
  };

  async function handleActivateAllSaes() {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      setActionMessage("Activate all SAEs is only available while infrastructure is running.");
      return;
    }
    setBulkSaeActionPending(true);
    setActionMessage("Activating SAEs...");
    try {
      const { saes: saeRecords, runtimeByNode } = await fetchSaeAndRuntimeData();
      setAllSaes(saeRecords);
      setDkmsRuntimeMap(runtimeByNode);
      const pending = saeRecords.filter((sae) => sae.status !== "active");
      if (pending.length === 0) {
        setActionMessage("All SAEs are already active.");
        return;
      }

      let activated = 0;
      let failed = 0;
      for (const sae of pending) {
        const response = await fetch(
          apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(sae.saeId)}/issue`),
          {
            method: "POST",
            headers: {
              "Content-Type": "application/json"
            },
            body: JSON.stringify({
              keyType: "ec-p256",
              daysValid: 90,
              bundleFormat: "pem"
            })
          }
        );
        if (response.ok) {
          activated += 1;
        } else {
          failed += 1;
        }
      }

      await refreshSaeOverlayData();
      if (failed > 0) {
        setActionMessage(`SAE activation completed: ${activated} activated, ${failed} failed.`);
      } else {
        setActionMessage(`SAE activation completed: ${activated} activated.`);
      }
    } catch {
      setActionMessage("Failed to activate SAEs.");
    } finally {
      setBulkSaeActionPending(false);
    }
  }

  async function performStopAllSaes() {
    setBulkSaeActionPending(true);
    setActionMessage("Stopping SAEs...");
    try {
      const { saes: saeRecords, runtimeByNode } = await fetchSaeAndRuntimeData();
      setAllSaes(saeRecords);
      setDkmsRuntimeMap(runtimeByNode);
      const active = saeRecords.filter((sae) => sae.status === "active");
      if (active.length === 0) {
        setActionMessage("No active SAEs to stop.");
        return;
      }

      let stopped = 0;
      let failed = 0;
      for (const sae of active) {
        const response = await fetch(
          apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(sae.saeId)}/revoke`),
          {
            method: "POST",
            headers: {
              "Content-Type": "application/json"
            },
            body: JSON.stringify({
              reason: "Bulk stop from editor"
            })
          }
        );
        if (response.ok) {
          stopped += 1;
        } else {
          failed += 1;
        }
      }

      await refreshSaeOverlayData();
      if (failed > 0) {
        setActionMessage(`SAE stop completed: ${stopped} stopped, ${failed} failed.`);
      } else {
        setActionMessage(`SAE stop completed: ${stopped} stopped.`);
      }
    } catch {
      setActionMessage("Failed to stop SAEs.");
    } finally {
      setBulkSaeActionPending(false);
    }
  }

  function handleStopAllSaes() {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      setActionMessage("Stop all SAEs is only available while infrastructure is running.");
      return;
    }
    requestConfirm({
      title: "Revocar todos los SAEs activos",
      description:
        "Se revocarán los certificados de todos los SAEs activos. Los SAEs tendrán que reactivarse manualmente.",
      confirmLabel: "Revocar todos",
      destructive: true,
      onConfirm: () => performStopAllSaes()
    });
  }

  async function handleActivateSelectedSaes() {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      setActionMessage("Activate selected SAEs is only available while infrastructure is running.");
      return;
    }
    if (selectedSaeIds.length === 0) {
      setActionMessage("Select SAE nodes with Ctrl/Cmd first.");
      return;
    }

    setBulkSaeActionPending(true);
    setActionMessage("Activating selected SAEs...");
    try {
      const { saes: saeRecords, runtimeByNode } = await fetchSaeAndRuntimeData();
      setAllSaes(saeRecords);
      setDkmsRuntimeMap(runtimeByNode);
      const bySaeId = new Map(saeRecords.map((item) => [item.saeId, item] as const));
      const pending = selectedSaeIds.filter((saeId) => {
        const record = bySaeId.get(saeId);
        return record && record.status !== "active";
      });
      if (pending.length === 0) {
        setActionMessage("All selected SAEs are already active.");
        return;
      }

      let activated = 0;
      let failed = 0;
      for (const saeId of pending) {
        const response = await fetch(
          apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(saeId)}/issue`),
          {
            method: "POST",
            headers: {
              "Content-Type": "application/json"
            },
            body: JSON.stringify({
              keyType: "ec-p256",
              daysValid: 90,
              bundleFormat: "pem"
            })
          }
        );
        if (response.ok) {
          activated += 1;
        } else {
          failed += 1;
        }
      }

      await refreshSaeOverlayData();
      if (failed > 0) {
        setActionMessage(`Selected SAE activation completed: ${activated} activated, ${failed} failed.`);
      } else {
        setActionMessage(`Selected SAE activation completed: ${activated} activated.`);
      }
    } catch {
      setActionMessage("Failed to activate selected SAEs.");
    } finally {
      setBulkSaeActionPending(false);
    }
  }

  async function handleStopSelectedSaes() {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      setActionMessage("Stop selected SAEs is only available while infrastructure is running.");
      return;
    }
    if (selectedSaeIds.length === 0) {
      setActionMessage("Select SAE nodes with Ctrl/Cmd first.");
      return;
    }

    setBulkSaeActionPending(true);
    setActionMessage("Stopping selected SAEs...");
    try {
      const { saes: saeRecords, runtimeByNode } = await fetchSaeAndRuntimeData();
      setAllSaes(saeRecords);
      setDkmsRuntimeMap(runtimeByNode);
      const bySaeId = new Map(saeRecords.map((item) => [item.saeId, item] as const));
      const active = selectedSaeIds.filter((saeId) => bySaeId.get(saeId)?.status === "active");
      if (active.length === 0) {
        setActionMessage("No selected SAE is active.");
        return;
      }

      let stopped = 0;
      let failed = 0;
      for (const saeId of active) {
        const response = await fetch(
          apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(saeId)}/revoke`),
          {
            method: "POST",
            headers: {
              "Content-Type": "application/json"
            },
            body: JSON.stringify({
              reason: "Bulk stop selected from editor"
            })
          }
        );
        if (response.ok) {
          stopped += 1;
        } else {
          failed += 1;
        }
      }

      await refreshSaeOverlayData();
      if (failed > 0) {
        setActionMessage(`Selected SAE stop completed: ${stopped} stopped, ${failed} failed.`);
      } else {
        setActionMessage(`Selected SAE stop completed: ${stopped} stopped.`);
      }
    } catch {
      setActionMessage("Failed to stop selected SAEs.");
    } finally {
      setBulkSaeActionPending(false);
    }
  }

  async function handleDkmsBatchAction(action: "start" | "stop") {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      setActionMessage("Selected DKMS actions are only available while infrastructure is running.");
      return;
    }
    if (selectedDkmsNodeIds.length === 0) {
      setActionMessage("Select DKMS nodes with Ctrl/Cmd first.");
      return;
    }
    if (actionPending || dkmsActionPending) {
      return;
    }

    const actionVerb = action === "start" ? "Starting" : "Stopping";
    const actionPast = action === "start" ? "started" : "stopped";
    const nextHealthState: EditorNodeData["healthState"] = action === "start" ? "up" : "down";
    setDkmsActionPending(true);
    setActionMessage(`${actionVerb} selected DKMS nodes...`);
    try {
      let successful = 0;
      let failed = 0;
      const successNodeIds: number[] = [];
      for (const nodeId of selectedDkmsNodeIds) {
        const response = await fetch(apiPath(`/api/simulations/${simulationId}/dkms/${nodeId}/${action}`), {
          method: "POST"
        });
        if (response.ok) {
          successful += 1;
          successNodeIds.push(nodeId);
        } else {
          failed += 1;
        }
      }

      if (successNodeIds.length > 0) {
        const successSet = new Set(successNodeIds);
        setNodes((current) =>
          current.map((node) => {
            const nodeId = Number(node.data.nodeId);
            if (node.data.nodeType !== "DKMS" || !Number.isFinite(nodeId) || !successSet.has(Math.trunc(nodeId))) {
              return node;
            }
            if (node.data.healthState === nextHealthState) {
              return node;
            }
            return {
              ...node,
              data: {
                ...node.data,
                healthState: nextHealthState
              }
            };
          })
        );
      }

      if (failed > 0) {
        setActionMessage(`Selected DKMS ${actionPast}: ${successful} ok, ${failed} failed.`);
      } else {
        setActionMessage(`Selected DKMS ${actionPast}: ${successful}.`);
      }
    } catch {
      setActionMessage(`Failed to ${action} selected DKMS nodes.`);
    } finally {
      setDkmsActionPending(false);
    }
  }

  async function handleDownloadAllActiveSaesZip() {
    if (!isInfrastructureRunning || isInfrastructureTransitioning) {
      setActionMessage("The mTLS ZIP export is only available while infrastructure is running.");
      return;
    }
    setBulkSaeActionPending(true);
    setActionMessage("Building mTLS kit archive for active SAEs...");
    try {
      const { saes: saeRecords, runtimeByNode } = await fetchSaeAndRuntimeData();
      setAllSaes(saeRecords);
      setDkmsRuntimeMap(runtimeByNode);

      const activeSaes = saeRecords.filter((sae) => sae.status === "active");
      if (activeSaes.length === 0) {
        setActionMessage("No active SAEs found.");
        return;
      }

      const JSZip = (await import("jszip")).default;
      const zip = new JSZip();
      let written = 0;
      let skipped = 0;

      for (const sae of activeSaes) {
        const dkmsId = Number(sae.dkmsId);
        const runtimeBasePath = Number.isFinite(dkmsId) ? runtimeByNode[String(dkmsId)]?.runtimeBasePath ?? "" : "";
        if (!runtimeBasePath) {
          skipped += 1;
          continue;
        }

        const bundleResponse = await fetch(
          apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(sae.saeId)}/bundle?format=pem`),
          {
            method: "GET",
            cache: "no-store"
          }
        );
        const bundlePayload = await bundleResponse.json().catch(() => ({}));
        if (!bundleResponse.ok) {
          skipped += 1;
          continue;
        }

        const bundle = bundlePayload?.bundle ?? {};
        const certPem = String(bundle?.certificatePem ?? "").trim();
        const keyPem = String(bundle?.privateKeyPem ?? "").trim();
        const caPem = String(bundle?.caChainPem ?? "").trim();
        if (!certPem || !keyPem || !caPem) {
          skipped += 1;
          continue;
        }

        const saeStem = sanitizeFileStem(sae.saeId);
        const certFilename = `${saeStem}.client.crt.pem`;
        const keyFilename = `${saeStem}.client.key.pem`;
        const caFilename = `${saeStem}.ca.crt.pem`;
        const scriptFilename = `${saeStem}.etsi-curl.sh`;
        const folderPath = `dkms_${dkmsId}/sae_${sae.saeId}`;
        const folder = zip.folder(folderPath);
        if (!folder) {
          skipped += 1;
          continue;
        }
        folder.file(certFilename, certPem);
        folder.file(keyFilename, keyPem);
        folder.file(caFilename, caPem);
        folder.file(
          scriptFilename,
          buildMtlsScriptContent({
            runtimeBasePath,
            callerSaeId: sae.saeId,
            certFilename,
            keyFilename,
            caFilename
          })
        );
        written += 1;
      }

      if (written === 0) {
        setActionMessage("No active SAE kits could be packaged.");
        return;
      }

      const zipBlob = await zip.generateAsync({ type: "blob" });
      downloadBlob(`simulation-${simulationId}-active-sae-kits.zip`, zipBlob);
      if (skipped > 0) {
        setActionMessage(`mTLS ZIP generated with ${written} SAEs (${skipped} skipped).`);
      } else {
        setActionMessage(`mTLS ZIP generated with ${written} active SAEs.`);
      }
    } catch {
      setActionMessage("Failed to build SAE mTLS ZIP.");
    } finally {
      setBulkSaeActionPending(false);
    }
  }

  async function handleRunStop() {
    if (isStoppingTransition) {
      setActionMessage(
        "No se puede ejecutar la infraestructura todavía: la ejecución actual aún se está deteniendo. Espera a que termine por completo."
      );
      return;
    }
    if (actionPending || isStartingTransition) {
      return;
    }
    const stopRequested = simulationStatus === "running";
    if (!stopRequested) {
      const dkmsCount = nodes.filter((node) => node.data.nodeType === "DKMS").length;
      if (dkmsCount < 1) {
        setActionMessage("Add at least one DKMS node before running infrastructure.");
        return;
      }
      if (autosaveTimer.current) {
        window.clearTimeout(autosaveTimer.current);
        autosaveTimer.current = null;
      }
      const saveResult = await persistGraph(false);
      if (!saveResult.ok) {
        setActionMessage(
          `Could not save topology before run. ${saveResult.error ? `Reason: ${saveResult.error}` : ""}`.trim()
        );
        return;
      }
      const persistedResponse = await fetch(apiPath(`/api/simulations/${simulationId}`), { cache: "no-store" });
      const persistedPayload = await persistedResponse.json().catch(() => ({}));
      const persistedNodes = Array.isArray(persistedPayload?.simulation?.nodes)
        ? persistedPayload.simulation.nodes.length
        : 0;
      if (!persistedResponse.ok || persistedNodes < 1) {
        setActionMessage(
          "Topology was not persisted in backend (0 DKMS). Save again before running infrastructure."
        );
        return;
      }
    }
    setActionPending(true);
    setInfrastructureTransition(stopRequested ? "stopping" : "starting");
    setTransitionProgress(stopRequested ? 10 : 8);
    setTransitionDetail(stopRequested ? "Sending stop request..." : "Sending run request...");
    setActionMessage(null);
    const endpoint = apiPath(
      stopRequested
        ? `/api/simulations/${simulationId}/stop`
        : `/api/simulations/${simulationId}/run`
    );
    try {
      const response = await fetch(endpoint, { method: "POST" });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        setActionMessage(payload?.error || "Action failed");
        await loadSimulation();
        return;
      }

      setRunRefreshToken((current) => current + 1);
      if (stopRequested) {
        setActionMessage("Stopping infrastructure. UI locked until completion...");
        setTransitionProgress(20);
        setTransitionDetail("Waiting for infrastructure status to leave RUNNING...");
        while (true) {
          const statusResponse = await fetch(apiPath(`/api/simulations/${simulationId}`), { cache: "no-store" });
          const statusPayload = await statusResponse.json().catch(() => ({}));
          if (statusResponse.ok && statusPayload?.simulation?.status) {
            const currentStatus = String(statusPayload.simulation.status).toLowerCase();
            if (currentStatus !== "running") {
              setTransitionProgress(95);
              setTransitionDetail(`Infrastructure state is now ${currentStatus.toUpperCase()}. Finalizing...`);
              break;
            }
          }
          setTransitionProgress((current) => Math.min(88, current + 3));
          await delay(1200);
        }
        await loadSimulation();
        setTransitionProgress(100);
        setTransitionDetail("Infrastructure fully stopped.");
        setActionMessage("Infrastructure stopped.");
      } else {
        setActionMessage("Deploying infrastructure. Verifying launch state...");
        setTransitionProgress(18);
        setTransitionDetail("Waiting for simulation status RUNNING...");
        let runningReached = false;
        const launchDeadline = Date.now() + 60_000;
        while (true) {
          const statusResponse = await fetch(apiPath(`/api/simulations/${simulationId}`), { cache: "no-store" });
          const statusPayload = await statusResponse.json().catch(() => ({}));
          const currentStatus = statusResponse.ok ? String(statusPayload?.simulation?.status ?? "").toLowerCase() : "";
          if (currentStatus === "running") {
            runningReached = true;
            setTransitionProgress(96);
            setTransitionDetail("Infrastructure RUNNING. DKMS health checks continue in background.");
            break;
          }
          if (currentStatus === "error") {
            setTransitionProgress(96);
            setTransitionDetail("Infrastructure launch failed.");
            break;
          }
          if (Date.now() >= launchDeadline) {
            setTransitionProgress(96);
            setTransitionDetail("Launch still in progress. Continuing checks in background.");
            break;
          }
          if (currentStatus) {
            setTransitionDetail(`Current status: ${currentStatus.toUpperCase()}`);
          } else {
            setTransitionDetail("Infrastructure still provisioning...");
          }
          setTransitionProgress((current) => Math.min(88, current + 4));
          await delay(1200);
        }
        await loadSimulation();
        setTransitionProgress(100);
        if (runningReached) {
          setTransitionDetail("Infrastructure launched.");
          setActionMessage(
            `Infrastructure run #${payload?.run?.id ?? "?"} started. DKMS health is checked in background.`
          );
        } else {
          const runStatus = String(payload?.run?.status ?? "").toUpperCase();
          if (runStatus) {
            setActionMessage(
              `Run #${payload?.run?.id ?? "?"} accepted (${runStatus}). DKMS health/status will update in background.`
            );
          } else {
            setActionMessage("Run request accepted. DKMS health/status will update in background.");
          }
        }
      }
    } finally {
      setActionPending(false);
      setInfrastructureTransition(null);
    }
  }

  const handleDkmsActionByNodeId = useCallback(
    async (nodeId: number, action: "start" | "stop") => {
      if (!Number.isFinite(nodeId) || nodeId <= 0) {
        setActionMessage("Select a valid DKMS node first.");
        return;
      }
      if (!infrastructureRunningRef.current || infrastructureTransitioningRef.current) {
        setActionMessage("DKMS actions are only available while infrastructure is running.");
        return;
      }
      if (actionPendingRef.current || dkmsActionPendingRef.current) {
        return;
      }

      const actionLabel = action === "start" ? "Starting" : "Stopping";
      setDkmsActionPending(true);
      setActionMessage(`${actionLabel} DKMS ${nodeId}...`);
      try {
        const response = await fetch(apiPath(`/api/simulations/${simulationId}/dkms/${nodeId}/${action}`), {
          method: "POST"
        });
        const payload = await response.json().catch(() => ({}));
        if (!response.ok) {
          setActionMessage(payload?.error || `Failed to ${action} DKMS ${nodeId}`);
          return;
        }

        const nextHealthState: EditorNodeData["healthState"] = action === "start" ? "up" : "down";
        setNodes((current) =>
          current.map((currentNode) => {
            if (currentNode.data.nodeType !== "DKMS" || currentNode.data.nodeId !== nodeId) {
              return currentNode;
            }
            if (currentNode.data.healthState === nextHealthState) {
              return currentNode;
            }
            return {
              ...currentNode,
              data: {
                ...currentNode.data,
                healthState: nextHealthState
              }
            };
          })
        );

        if (action === "start") {
          setActionMessage(`DKMS ${nodeId} started. Traffic and key operations can resume through this node.`);
        } else {
          setActionMessage(`DKMS ${nodeId} stopped. Relay and key operations through this node are unavailable.`);
        }
      } finally {
        setDkmsActionPending(false);
      }
    },
    [simulationId]
  );

  const handleStopDkmsByNodeId = useCallback(
    async (nodeId: number) => {
      await handleDkmsActionByNodeId(nodeId, "stop");
    },
    [handleDkmsActionByNodeId]
  );

  const handleStartDkmsByNodeId = useCallback(
    async (nodeId: number) => {
      await handleDkmsActionByNodeId(nodeId, "start");
    },
    [handleDkmsActionByNodeId]
  );

  async function handleStopSelectedDkms() {
    const node = selectedNode;
    const rawNodeId = node?.data.nodeId;
    const nodeId = Number.isFinite(Number(rawNodeId)) ? Math.trunc(Number(rawNodeId)) : 0;
    if (!node || node.data.nodeType !== "DKMS" || nodeId <= 0) {
      setActionMessage("Select a valid DKMS node first.");
      return;
    }
    await handleStopDkmsByNodeId(nodeId);
  }

  async function handleStartSelectedDkms() {
    const node = selectedNode;
    const rawNodeId = node?.data.nodeId;
    const nodeId = Number.isFinite(Number(rawNodeId)) ? Math.trunc(Number(rawNodeId)) : 0;
    if (!node || node.data.nodeType !== "DKMS" || nodeId <= 0) {
      setActionMessage("Select a valid DKMS node first.");
      return;
    }
    await handleStartDkmsByNodeId(nodeId);
  }

  function handleInspectorResizeStart(event: ReactMouseEvent<HTMLButtonElement>) {
    if (typeof window === "undefined") {
      return;
    }
    event.preventDefault();
    resizingInspectorRef.current = true;
    inspectorDragStartRef.current = {
      x: event.clientX,
      width: inspectorWidth
    };
    document.body.classList.add("cursor-col-resize", "select-none");
  }

  const handleCanvasSelectionChange = useCallback(
    ({ nodes: selectedNodes, edges: selectedEdges }: { nodes: FlowNode[]; edges: FlowEdge[] }) => {
      if (selectedNodes.length > 0) {
        const nextNodeIds = selectedNodes.map((node) => node.id);
        const nextPrimaryNodeId = selectedNodes[0]?.id ?? null;
        setSelectedNodeIds((current) => (sameStringArray(current, nextNodeIds) ? current : nextNodeIds));
        setSelectedNodeId((current) => (current === nextPrimaryNodeId ? current : nextPrimaryNodeId));
        setSelectedEdgeId((current) => (current === null ? current : null));
        return;
      }

      const nonTransientSelectedEdges = selectedEdges.filter((edge) => !edge.data?.transient);
      if (nonTransientSelectedEdges.length > 0) {
        const nextPrimaryEdgeId = nonTransientSelectedEdges[0]?.id ?? null;
        setSelectedEdgeId((current) => (current === nextPrimaryEdgeId ? current : nextPrimaryEdgeId));
        setSelectedNodeId((current) => (current === null ? current : null));
        setSelectedNodeIds((current) => (current.length === 0 ? current : []));
        return;
      }

      setSelectedEdgeId((current) => (current === null ? current : null));
      setSelectedNodeId((current) => (current === null ? current : null));
      setSelectedNodeIds((current) => (current.length === 0 ? current : []));
    },
    []
  );

  useEffect(() => {
    const showDkmsControls = isInfrastructureRunning && !isInfrastructureTransitioning;
    const actionPendingNow = actionPending || dkmsActionPending;
    setNodes((current) => {
      let changed = false;
      const nextNodes = current.map((node) => {
        const rawNodeId = node.data.nodeId;
        const nodeId = Number.isFinite(Number(rawNodeId)) ? Math.trunc(Number(rawNodeId)) : 0;
        const controlsVisible = showDkmsControls && node.data.nodeType === "DKMS" && nodeId > 0;
        const stopVisible = controlsVisible;
        const startVisible = controlsVisible;
        const stopPending = controlsVisible ? actionPendingNow : false;
        const startPending = controlsVisible ? actionPendingNow : false;

        let nextStopCallback: (() => void) | null = null;
        let nextStartCallback: (() => void) | null = null;
        if (controlsVisible) {
          const existingStop = stopNodeCallbacksRef.current.get(nodeId);
          if (existingStop) {
            nextStopCallback = existingStop;
          } else {
            nextStopCallback = () => {
              void handleStopDkmsByNodeId(nodeId);
            };
            stopNodeCallbacksRef.current.set(nodeId, nextStopCallback);
          }

          const existingStart = startNodeCallbacksRef.current.get(nodeId);
          if (existingStart) {
            nextStartCallback = existingStart;
          } else {
            nextStartCallback = () => {
              void handleStartDkmsByNodeId(nodeId);
            };
            startNodeCallbacksRef.current.set(nodeId, nextStartCallback);
          }
        }

        if (
          node.data.stopDkmsVisible === stopVisible &&
          node.data.startDkmsVisible === startVisible &&
          node.data.stopDkmsPending === stopPending &&
          node.data.startDkmsPending === startPending &&
          node.data.onStopDkms === nextStopCallback &&
          node.data.onStartDkms === nextStartCallback
        ) {
          return node;
        }

        changed = true;
        return {
          ...node,
          data: {
            ...node.data,
            stopDkmsVisible: stopVisible,
            startDkmsVisible: startVisible,
            stopDkmsPending: stopPending,
            startDkmsPending: startPending,
            onStopDkms: nextStopCallback,
            onStartDkms: nextStartCallback
          }
        };
      });
      return changed ? nextNodes : current;
    });
  }, [
    actionPending,
    dkmsActionPending,
    handleStartDkmsByNodeId,
    handleStopDkmsByNodeId,
    isInfrastructureRunning,
    isInfrastructureTransitioning
  ]);

  if (loading) {
    return (
      <main
        id="main-content"
        className="grid min-h-screen place-items-center p-6"
        aria-busy="true"
      >
        <p className="text-sm text-muted-foreground">Cargando editor…</p>
      </main>
    );
  }

  return (
    <main
      id="main-content"
      className="w-full space-y-4 bg-gradient-to-b from-muted/30 via-background to-muted/20 p-4 pb-24 md:p-6"
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <img
            src={withBasePath("/icon.png")}
            alt=""
            width={64}
            height={64}
            className="rounded-md border border-border p-1 sm:h-20 sm:w-20"
            aria-hidden="true"
          />
          <div className="min-w-0">
            <Button variant="ghost" onClick={() => router.push("/simulations")}>
              <ArrowLeft aria-hidden="true" />
              Volver a simulaciones
            </Button>
            <h1 className="font-serif text-2xl md:text-3xl lg:text-4xl">Editor de simulación</h1>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <ThemeToggle />
          <Link
            href="/simulations"
            className="text-sm text-primary hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          >
            Lista de simulaciones
          </Link>
        </div>
      </div>

      <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-4">
        <div className="rounded-lg border border-border bg-card p-3 shadow-sm">
          <p className="text-[11px] uppercase tracking-wide text-muted-foreground">DKMS nodes</p>
          <p className="mt-1 text-2xl font-semibold text-foreground">{nodeCount}</p>
        </div>
        <div className="rounded-lg border border-border bg-card p-3 shadow-sm">
          <p className="text-[11px] uppercase tracking-wide text-muted-foreground">Links</p>
          <p className="mt-1 text-2xl font-semibold text-foreground">{linkCount}</p>
        </div>
        <div className="rounded-lg border border-border bg-card p-3 shadow-sm">
          <p className="text-[11px] uppercase tracking-wide text-muted-foreground">{selectionLabel}</p>
          <p className="mt-1 truncate text-sm font-semibold text-foreground">{selectionDetail}</p>
        </div>
        <div className="rounded-lg border border-border bg-card p-3 shadow-sm">
          <p className="text-[11px] uppercase tracking-wide text-muted-foreground">Current mode</p>
          <p className="mt-1 text-sm font-semibold text-foreground">
            {isStartingTransition
              ? "Locked during transition"
              : isInfrastructureRunning
                ? "Running: create/delete PQC and convert QKD ↔ HYBRID"
                : "Editable topology"}
            {` · Active SAEs ${activeSaeCount}/${allSaes.length}`}
          </p>
        </div>
      </div>

      <EditorToolbox
        onAddDkms={addNode}
        addDkmsDisabled={isInfrastructureRunning}
        connectionModeEnabled={connectionModeEnabled}
        onConnectionModeChange={setConnectionModeEnabled}
        connectionLinkType={connectionLinkType}
        onConnectionLinkTypeChange={setConnectionLinkType}
        saeOverlayEnabled={saeOverlayEnabled}
        onSaeOverlayToggle={() => setSaeOverlayEnabled((current) => !current)}
        onDeleteSelection={deleteSelection}
        onFitView={() => flowRef.current?.fitView({ padding: 0.25 })}
        readOnly={isTopologyLocked}
      />

      <div
        className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_12px_var(--inspector-width)]"
        style={{ "--inspector-width": `${inspectorWidth}px` } as CSSProperties}
      >
        <div className="space-y-4">
          {saeOverlayLoading ? (
            <div className="flex justify-end">
              <span className="rounded-full border border-blue-200 bg-blue-50 px-2 py-0.5 text-xs text-blue-700">
                Loading SAE overlay...
              </span>
            </div>
          ) : null}

          <EditorCanvas
            nodes={nodes}
            edges={edges}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onConnect={onConnect}
            nodeTypes={nodeTypes}
            connectionModeEnabled={connectionModeEnabled}
            readOnly={isTopologyLocked}
            allowNodeDrag={!isInfrastructureTransitioning && (canMoveNodes || saeOverlayEnabled)}
            onSelectionChange={handleCanvasSelectionChange}
            onNodeClick={(nodeId) => {
              setSelectedNodeId(nodeId);
              setSelectedEdgeId(null);
            }}
            onEdgeClick={(edgeId) => {
              const edge = edges.find((candidate) => candidate.id === edgeId);
              if (edge?.data?.transient) {
                return;
              }
              setSelectedEdgeId(edgeId);
              setSelectedNodeId(null);
              setSelectedNodeIds([]);
            }}
            onPaneClick={() => {
              setSelectedNodeId(null);
              setSelectedNodeIds([]);
              setSelectedEdgeId(null);
            }}
            onPaneDoubleClick={handlePaneDoubleClick}
            onEdgeDoubleClick={(_, edge) => quickEditEdgeDistance(edge)}
            onEdgeContextMenu={handleEdgeContextMenu}
            onInit={(instance) => {
              flowRef.current = instance;
            }}
          />

          <div className="rounded-xl border border-border bg-card p-4 shadow-sm">
            <div className="grid gap-3">
              <div className="space-y-1.5">
                <Label htmlFor="simulation-name-main" className="text-xs uppercase tracking-wide text-muted-foreground">
                  Simulation name
                </Label>
                <Input
                  id="simulation-name-main"
                  value={simulationName}
                  onChange={(event) => setSimulationName(event.target.value)}
                  disabled={!canEditSimulationMeta}
                />
              </div>
              <div className="space-y-1.5">
                <Label
                  htmlFor="simulation-description-main"
                  className="text-xs uppercase tracking-wide text-muted-foreground"
                >
                  Description
                </Label>
                <Textarea
                  id="simulation-description-main"
                  value={simulationDescription}
                  onChange={(event) => setSimulationDescription(event.target.value)}
                  disabled={!canEditSimulationMeta}
                  className="min-h-[90px]"
                />
              </div>
              <div className="space-y-2 rounded-md border border-border bg-muted/20 p-3">
                <Label>Default QKD buffer (global)</Label>
                <Input
                  type="range"
                  min={1}
                  max={2000}
                  step={1}
                  value={globalQkdBufferSize}
                  onChange={(e) => setGlobalQkdBufferSize(parsePositiveInt(e.target.value))}
                  disabled={isEditorLocked || isInfrastructureRunning}
                />
                <Input
                  type="number"
                  min={1}
                  step={1}
                  value={globalQkdBufferSize}
                  onChange={(e) => setGlobalQkdBufferSize(parsePositiveInt(e.target.value))}
                  disabled={isEditorLocked || isInfrastructureRunning}
                />
                <Button
                  variant="outline"
                  size="sm"
                  onClick={applyGlobalQkdBufferToAllLinks}
                  disabled={isEditorLocked || isInfrastructureRunning}
                >
                  Apply to all QKD links
                </Button>
                <p className="text-xs text-muted-foreground">
                  {isInfrastructureRunning
                    ? "Locked while running: QKD/HYBRID parameters cannot be modified."
                    : "You can still override buffer individually per QKD/HYBRID link."}
                </p>
              </div>
            </div>
          </div>
          <RunsPanel simulationId={simulationId} refreshToken={runRefreshToken} />
        </div>

        <div className="hidden lg:flex items-stretch justify-center">
          <button
            type="button"
            aria-label="Resize right panel"
            className="h-full w-2 cursor-col-resize rounded-full bg-border transition hover:bg-muted-foreground/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            onMouseDown={handleInspectorResizeStart}
          />
        </div>

        <div className="lg:sticky lg:top-4 lg:max-h-[calc(100vh-2rem)] lg:overflow-y-auto lg:pr-1">
          <Inspector
            simulationId={simulationId}
            simulationStatus={simulationStatus}
            selectedNode={selectedNode}
            selectedEdge={selectedEdge}
            onSelectedNodeChange={updateSelectedNode}
            onSelectedEdgeChange={updateSelectedEdge}
            onDeleteSelection={deleteSelection}
            deleteSelectionDisabled={isTopologyLocked || (!selectedNode && !selectedEdge)}
            onStartSelectedDkms={() => void handleStartSelectedDkms()}
            onStopSelectedDkms={() => void handleStopSelectedDkms()}
            startSelectedDkmsDisabled={
              dkmsActionPending ||
              actionPending ||
              !isInfrastructureRunning ||
              isInfrastructureTransitioning ||
              !selectedNode ||
              selectedNode.data.nodeType !== "DKMS" ||
              !Number.isFinite(Number(selectedNode.data.nodeId)) ||
              Number(selectedNode.data.nodeId) <= 0 ||
              selectedNode.data.healthState === "up"
            }
            stopSelectedDkmsDisabled={
              dkmsActionPending ||
              actionPending ||
              !isInfrastructureRunning ||
              isInfrastructureTransitioning ||
              !selectedNode ||
              selectedNode.data.nodeType !== "DKMS" ||
              !Number.isFinite(Number(selectedNode.data.nodeId)) ||
              Number(selectedNode.data.nodeId) <= 0 ||
              selectedNode.data.healthState === "down"
            }
            savingState={saveState}
            actionMessage={actionMessage}
            readOnly={isEditorLocked}
            onSaeUpdated={() => void refreshSaeOverlayData()}
          />
        </div>
      </div>
      <div className="fixed bottom-4 right-4 z-30 flex items-end gap-2">
        <details className="group relative">
          <summary className="inline-flex min-h-[44px] list-none items-center rounded-md border border-border bg-card px-4 text-sm font-medium shadow-md hover:bg-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background">
            Options
          </summary>
          <div className="absolute bottom-full right-0 z-20 mb-2 w-[min(calc(100vw-2rem),11rem)] rounded-md border border-border bg-card p-2 shadow-xl">
            <div className="flex flex-col gap-2">
              <Button type="button" variant="outline" size="sm" onClick={() => void handleExport()}>
                Export
              </Button>
              <label
                className={
                  "inline-flex h-9 items-center justify-center rounded-md border px-3 text-sm font-medium " +
                  (isEditorLocked || isInfrastructureRunning
                    ? "cursor-not-allowed border-border bg-muted text-muted-foreground/60"
                    : "cursor-pointer border-border bg-card hover:bg-muted")
                }
              >
                Import
                <input
                  type="file"
                  className="hidden"
                  accept="application/json"
                  onChange={handleFloatingImportFile}
                  disabled={isEditorLocked || isInfrastructureRunning}
                />
              </label>
            </div>
          </div>
        </details>
        {isInfrastructureRunning ? (
          <>
            <details className="group relative">
              <summary className="inline-flex min-h-[44px] list-none items-center rounded-md border border-border bg-card px-4 text-sm font-medium shadow-md hover:bg-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background">
                Options DKMS
              </summary>
              <div className="absolute bottom-full right-0 z-20 mb-2 w-[min(calc(100vw-2rem),20rem)] rounded-md border border-border bg-card p-2 shadow-xl">
                <div className="flex flex-col gap-2">
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => void handleDkmsBatchAction("start")}
                    disabled={
                      isInfrastructureTransitioning ||
                      actionPending ||
                      dkmsActionPending ||
                      selectedDkmsNodeIds.length === 0
                    }
                    className="justify-start"
                  >
                    {`Start Selected DKMS (${selectedDkmsNodeIds.length})`}
                  </Button>
                  <Button
                    type="button"
                    variant="destructive"
                    size="sm"
                    onClick={() => void handleDkmsBatchAction("stop")}
                    disabled={
                      isInfrastructureTransitioning ||
                      actionPending ||
                      dkmsActionPending ||
                      selectedDkmsNodeIds.length === 0
                    }
                    className="justify-start"
                  >
                    {`Stop Selected DKMS (${selectedDkmsNodeIds.length})`}
                  </Button>
                </div>
              </div>
            </details>
            <details className="group relative">
              <summary className="inline-flex min-h-[44px] list-none items-center rounded-md border border-border bg-card px-4 text-sm font-medium shadow-md hover:bg-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background">
                Options SAEs
              </summary>
              <div className="absolute bottom-full right-0 z-20 mb-2 w-[min(calc(100vw-2rem),20rem)] rounded-md border border-border bg-card p-2 shadow-xl">
                <div className="flex flex-col gap-2">
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => void handleActivateSelectedSaes()}
                    disabled={isInfrastructureTransitioning || bulkSaeActionPending || selectedSaeIds.length === 0}
                    className="justify-start"
                  >
                    {`Activate Selected SAEs (${selectedSaeIds.length})`}
                  </Button>
                  <Button
                    type="button"
                    variant="destructive"
                    size="sm"
                    onClick={() => void handleStopSelectedSaes()}
                    disabled={isInfrastructureTransitioning || bulkSaeActionPending || selectedSaeIds.length === 0}
                    className="justify-start"
                  >
                    {`Stop Selected SAEs (${selectedSaeIds.length})`}
                  </Button>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => void handleActivateAllSaes()}
                    disabled={isInfrastructureTransitioning || bulkSaeActionPending}
                    className="justify-start"
                  >
                    {bulkSaeActionPending ? "Activating SAEs..." : "Activate All SAEs"}
                  </Button>
                  <Button
                    type="button"
                    variant="destructive"
                    size="sm"
                    onClick={() => void handleStopAllSaes()}
                    disabled={isInfrastructureTransitioning || bulkSaeActionPending}
                    className="justify-start"
                  >
                    Stop All SAEs
                  </Button>
                  <Button
                    type="button"
                    variant="secondary"
                    size="sm"
                    onClick={() => void handleDownloadAllActiveSaesZip()}
                    disabled={isInfrastructureTransitioning || bulkSaeActionPending}
                    className="justify-start"
                  >
                    Download Active SAE Kits (.zip)
                  </Button>
                </div>
              </div>
            </details>
          </>
        ) : null}
        <Button
          type="button"
          size="default"
          variant={simulationStatus === "running" ? "destructive" : "default"}
          onClick={() => void handleRunStop()}
          disabled={isStartingTransition}
          className="h-11 whitespace-nowrap px-4 shadow-lg"
        >
          {isStoppingTransition
            ? "Stopping Infrastructure..."
            : simulationStatus === "running"
              ? "Stop Infrastructure"
              : "Run Infrastructure"}
        </Button>
        <Badge variant={statusChipVariant} className="min-h-[36px] px-4 text-sm">
          {simulationStatus === "running" ? "RUNNING" : "STOP"}
        </Badge>
      </div>
      {isInfrastructureTransitioning ? (
        <div
          className="fixed inset-0 z-40 flex items-center justify-center p-4 backdrop-blur-sm sm:p-6"
          role="presentation"
        >
          <div
            aria-hidden="true"
            className="absolute inset-0 bg-background/70"
          />
          <div
            role="dialog"
            aria-modal="true"
            aria-labelledby="infra-transition-title"
            aria-describedby="infra-transition-detail"
            className="relative z-10 w-full max-w-xl rounded-xl border border-border bg-card px-6 py-5 text-center text-card-foreground shadow-2xl"
          >
            <p id="infra-transition-title" className="text-base font-semibold text-primary">
              {isStartingTransition
                ? "Desplegando infraestructura…"
                : "Deteniendo infraestructura…"}
            </p>
            <p className="mt-1 text-xs text-muted-foreground">
              El editor queda bloqueado hasta que termine la operación.
            </p>
            <div
              className="mt-4 h-2.5 w-full overflow-hidden rounded-full bg-muted"
              role="progressbar"
              aria-valuenow={Math.round(Math.max(0, Math.min(100, transitionProgress)))}
              aria-valuemin={0}
              aria-valuemax={100}
            >
              <div
                className="h-full rounded-full bg-primary transition-[width] duration-500 motion-reduce:transition-none"
                style={{ width: `${Math.max(4, Math.min(100, transitionProgress))}%` }}
              />
            </div>
            <div
              id="infra-transition-detail"
              className="mt-2 flex items-center justify-between text-[11px] text-muted-foreground"
            >
              <span>{transitionDetail}</span>
              <span>{Math.round(Math.max(0, Math.min(100, transitionProgress)))}%</span>
            </div>
          </div>
        </div>
      ) : null}

      <ConfirmDialog
        open={pendingConfirm !== null}
        onOpenChange={(open) => {
          if (!open && !confirmLoading) setPendingConfirm(null);
        }}
        title={pendingConfirm?.title ?? ""}
        description={pendingConfirm?.description}
        confirmLabel={pendingConfirm?.confirmLabel ?? "Confirmar"}
        destructive={pendingConfirm?.destructive ?? false}
        loading={confirmLoading}
        onConfirm={async () => {
          if (!pendingConfirm) return;
          setConfirmLoading(true);
          try {
            await pendingConfirm.onConfirm();
          } finally {
            setConfirmLoading(false);
            setPendingConfirm(null);
          }
        }}
      />
    </main>
  );
}
