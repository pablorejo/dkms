import { orchestratorFetch } from "@/lib/auth/orchestrator-session";
import { exportSimulationTopology } from "@/lib/topology/export";
import { importTopologyToSimulationGraph } from "@/lib/topology/import";
import type { ImportedSimulationSae } from "@/lib/topology/import";
import type {
  LinkType,
  NodeType,
  RunStatus,
  SimulationStatus,
  SimulationDTO,
  SimulationLinkInput,
  SimulationNodeInput,
  SimulationRunDTO,
  SimulationSummaryDTO,
  TopologyCanonicalV1
} from "@/lib/topology/types";
import {
  DEFAULT_QUDITTO_RATE_ALPHA as DEFAULT_QUDITTO_RATE_ALPHA_VALUE,
  DEFAULT_QUDITTO_RATE_R0 as DEFAULT_QUDITTO_RATE_R0_VALUE
} from "@/lib/topology/types";
import { validateSimulationGraph } from "@/lib/topology/validate";

const NODE_TYPES: NodeType[] = ["DKMS"];
const LINK_TYPES: LinkType[] = ["QKD", "PQC", "HYBRID"];
const RUN_STATUSES: RunStatus[] = ["QUEUED", "RUNNING", "DONE", "FAILED"];
const SIMULATION_STATUSES: SimulationStatus[] = ["pending", "running", "finished", "error"];

function normalizeNodeType(value: string): NodeType {
  const normalized = String(value ?? "").trim().toUpperCase();
  return NODE_TYPES.includes(normalized as NodeType) ? (normalized as NodeType) : "DKMS";
}

function normalizeLinkType(value: string): LinkType {
  const normalized = String(value ?? "").trim().toUpperCase();
  return LINK_TYPES.includes(normalized as LinkType) ? (normalized as LinkType) : "QKD";
}

function normalizeRunStatus(value: string): RunStatus {
  return RUN_STATUSES.includes(value as RunStatus) ? (value as RunStatus) : "FAILED";
}

function normalizeSimulationStatus(value: string): SimulationStatus {
  const normalized = String(value ?? "").trim().toLowerCase();
  return SIMULATION_STATUSES.includes(normalized as SimulationStatus)
    ? (normalized as SimulationStatus)
    : "error";
}

function normalizeSaeStatus(value: unknown): SaeAdminStatus {
  const normalized = String(value ?? "").trim().toLowerCase();
  if (normalized === "active" || normalized === "revoked" || normalized === "expired") {
    return normalized;
  }
  return "pending_cert";
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
    return DEFAULT_QUDITTO_RATE_R0_VALUE;
  }
  return parsed;
}

function normalizeQudittoRateAlpha(value: unknown): number {
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return DEFAULT_QUDITTO_RATE_ALPHA_VALUE;
  }
  return parsed;
}

interface RemoteSdn {
  ip: string;
  port: number;
  type_http: "http" | "https";
}

interface RemoteNode {
  uid: string;
  node_id: number;
  label: string;
  x: number;
  y: number;
}

interface RemoteLink {
  uid: string;
  source_uid: string;
  target_uid: string;
  link_type: "QKD" | "PQC" | "HYBRID";
  distance_km?: number | null;
  quditto_max_buffer_size?: number | null;
  quditto_rate_r0?: number | null;
  quditto_rate_alpha?: number | null;
}

interface RemoteSimulation {
  id: number;
  name: string;
  description: string | null;
  status: string;
  sdn: RemoteSdn;
  created_at: string;
  updated_at: string;
  nodes: RemoteNode[];
  links: RemoteLink[];
}

interface RemoteSimulationSummary {
  id: number;
  name: string;
  description: string | null;
  status: string;
  created_at: string;
  updated_at: string;
  node_count: number;
  link_count: number;
  sae_count?: number;
}

interface RemoteRun {
  id: number;
  simulation_id: number;
  status: string;
  message: string | null;
  queued_at: string | null;
  started_at: string | null;
  finished_at: string | null;
  created_at: string;
}

interface RemoteActionResponse {
  status: string;
  simulation_id: number;
  action: string;
}

type RemoteSaeStatus = "pending_cert" | "active" | "revoked" | "expired";

interface RemoteAdminSae {
  id: number;
  sae_id: string;
  display_name: string | null;
  owner_user_id: number | null;
  simulation_id: number | null;
  dkms_id: number | null;
  status: RemoteSaeStatus | string;
  cert_serial: string | null;
  cert_fingerprint: string | null;
  cert_subject: string | null;
  cert_not_before: string | null;
  cert_not_after: string | null;
  revoked_at: string | null;
  revocation_reason: string | null;
  created_at: string | null;
  updated_at: string | null;
}

interface RemoteSaeIssueResponse {
  sae: RemoteAdminSae;
  certificate_pem: string;
  ca_chain_pem: string;
  private_key_pem: string | null;
  bundle_pkcs12_base64: string | null;
}

interface RemoteSaeBundleResponse {
  sae: RemoteAdminSae;
  format: "pem" | "pkcs12";
  certificate_pem: string | null;
  ca_chain_pem: string | null;
  private_key_pem: string | null;
  bundle_pkcs12_base64: string | null;
}

export type SaeAdminStatus = "pending_cert" | "active" | "revoked" | "expired";

export interface SaeAdminDTO {
  id: number;
  saeId: string;
  displayName: string | null;
  ownerUserId: number | null;
  simulationId: number | null;
  dkmsId: number | null;
  status: SaeAdminStatus;
  certSerial: string | null;
  certFingerprint: string | null;
  certSubject: string | null;
  certNotBefore: string | null;
  certNotAfter: string | null;
  revokedAt: string | null;
  revocationReason: string | null;
  createdAt: string | null;
  updatedAt: string | null;
}

export interface SaeIssueDTO {
  sae: SaeAdminDTO;
  certificatePem: string;
  caChainPem: string;
  privateKeyPem: string | null;
  bundlePkcs12Base64: string | null;
}

export interface SaeBundleDTO {
  sae: SaeAdminDTO;
  format: "pem" | "pkcs12";
  certificatePem: string | null;
  caChainPem: string | null;
  privateKeyPem: string | null;
  bundlePkcs12Base64: string | null;
}

function formatRemoteErrorDetail(detail: unknown): string | null {
  if (typeof detail === "string" && detail.trim()) {
    return detail.trim();
  }
  if (Array.isArray(detail)) {
    const fragments = detail
      .map((item) => {
        if (typeof item === "string") {
          return item.trim();
        }
        if (item && typeof item === "object") {
          const itemDetail = (item as { msg?: unknown }).msg;
          if (typeof itemDetail === "string" && itemDetail.trim()) {
            return itemDetail.trim();
          }
        }
        return "";
      })
      .filter((item) => item.length > 0);
    if (fragments.length > 0) {
      return fragments.join("; ");
    }
  }
  return null;
}

function parseRemoteError(payload: unknown, fallback: string): string {
  if (typeof payload === "string") {
    const normalized = payload.replace(/\s+/g, " ").trim();
    if (normalized) {
      return normalized.slice(0, 280);
    }
  }
  if (payload && typeof payload === "object") {
    const detail = (payload as { detail?: unknown }).detail;
    const normalizedDetail = formatRemoteErrorDetail(detail);
    if (normalizedDetail) {
      return normalizedDetail;
    }
    const error = (payload as { error?: unknown }).error;
    if (typeof error === "string" && error.trim()) {
      return error;
    }
  }
  return fallback;
}

async function readResponsePayload(response: Response): Promise<unknown> {
  const raw = await response.text().catch(() => "");
  const trimmed = raw.trim();
  if (!trimmed) {
    return {};
  }
  try {
    return JSON.parse(trimmed);
  } catch {
    return trimmed;
  }
}

async function callOrchestrator(
  path: string,
  token: string,
  userId: string | number,
  init: RequestInit = {},
  fallbackMessage = "Upstream request failed",
  simulationId?: number
) {
  const headers = new Headers(init.headers ?? {});
  headers.set("X-User-Id", String(userId));
  if (typeof simulationId === "number" && Number.isFinite(simulationId)) {
    headers.set("X-Simulation-Id", String(simulationId));
  }

  const response = await orchestratorFetch(path, { ...init, headers }, token);
  const payload = await readResponsePayload(response);
  if (!response.ok) {
    throw new SimulationApiError(response.status, parseRemoteError(payload, fallbackMessage));
  }
  return payload;
}

function toSimulationDTO(remote: RemoteSimulation): SimulationDTO {
  return {
    id: remote.id,
    name: remote.name,
    description: remote.description,
    status: normalizeSimulationStatus(remote.status),
    sdn: {
      ip: remote.sdn.ip,
      port: remote.sdn.port,
      typeHttp: remote.sdn.type_http
    },
    createdAt: remote.created_at,
    updatedAt: remote.updated_at,
    nodes: (remote.nodes ?? []).map((node) => ({
      uid: node.uid,
      type: "DKMS",
      nodeId: Number.isFinite(Number(node.node_id)) ? Number(node.node_id) : null,
      label: node.label,
      x: node.x,
      y: node.y
    })),
    links: (remote.links ?? []).map((link) => ({
      uid: link.uid,
      sourceUid: link.source_uid,
      targetUid: link.target_uid,
      linkType: normalizeLinkType(link.link_type),
      distanceKm: normalizeLinkType(link.link_type) === "PQC" ? 0 : normalizeDistanceKm(link.distance_km),
      qudittoMaxBufferSize:
        normalizeLinkType(link.link_type) === "PQC"
          ? 100
          : normalizeQudittoMaxBufferSize(link.quditto_max_buffer_size),
      qudittoRateR0:
        normalizeLinkType(link.link_type) === "PQC"
          ? DEFAULT_QUDITTO_RATE_R0_VALUE
          : normalizeQudittoRateR0(link.quditto_rate_r0),
      qudittoRateAlpha:
        normalizeLinkType(link.link_type) === "PQC"
          ? DEFAULT_QUDITTO_RATE_ALPHA_VALUE
          : normalizeQudittoRateAlpha(link.quditto_rate_alpha)
    }))
  };
}

function toSimulationSummaryDTO(remote: RemoteSimulationSummary): SimulationSummaryDTO {
  return {
    id: remote.id,
    name: remote.name,
    description: remote.description,
    status: normalizeSimulationStatus(remote.status),
    createdAt: remote.created_at,
    updatedAt: remote.updated_at,
    nodeCount: Number(remote.node_count ?? 0),
    linkCount: Number(remote.link_count ?? 0),
    saeCount: Number(remote.sae_count ?? 0)
  };
}

function toRunDTO(remote: RemoteRun): SimulationRunDTO {
  return {
    id: remote.id,
    simulationId: remote.simulation_id,
    status: normalizeRunStatus(remote.status),
    message: remote.message,
    queuedAt: remote.queued_at,
    startedAt: remote.started_at,
    finishedAt: remote.finished_at,
    createdAt: remote.created_at
  };
}

function toSaeAdminDTO(remote: RemoteAdminSae): SaeAdminDTO {
  return {
    id: Number(remote.id),
    saeId: String(remote.sae_id ?? ""),
    displayName: remote.display_name ?? null,
    ownerUserId: remote.owner_user_id ?? null,
    simulationId: remote.simulation_id ?? null,
    dkmsId: remote.dkms_id ?? null,
    status: normalizeSaeStatus(remote.status),
    certSerial: remote.cert_serial ?? null,
    certFingerprint: remote.cert_fingerprint ?? null,
    certSubject: remote.cert_subject ?? null,
    certNotBefore: remote.cert_not_before ?? null,
    certNotAfter: remote.cert_not_after ?? null,
    revokedAt: remote.revoked_at ?? null,
    revocationReason: remote.revocation_reason ?? null,
    createdAt: remote.created_at ?? null,
    updatedAt: remote.updated_at ?? null
  };
}

function toSaeIssueDTO(remote: RemoteSaeIssueResponse): SaeIssueDTO {
  return {
    sae: toSaeAdminDTO(remote.sae),
    certificatePem: String(remote.certificate_pem ?? ""),
    caChainPem: String(remote.ca_chain_pem ?? ""),
    privateKeyPem: remote.private_key_pem ?? null,
    bundlePkcs12Base64: remote.bundle_pkcs12_base64 ?? null
  };
}

function toSaeBundleDTO(remote: RemoteSaeBundleResponse): SaeBundleDTO {
  return {
    sae: toSaeAdminDTO(remote.sae),
    format: remote.format === "pkcs12" ? "pkcs12" : "pem",
    certificatePem: remote.certificate_pem ?? null,
    caChainPem: remote.ca_chain_pem ?? null,
    privateKeyPem: remote.private_key_pem ?? null,
    bundlePkcs12Base64: remote.bundle_pkcs12_base64 ?? null
  };
}

function toRemoteUpsertPayload(payload: {
  name: string;
  description?: string | null;
  sdn?: {
    ip?: string;
    port?: number;
    typeHttp?: "http" | "https";
  };
  nodes: SimulationNodeInput[];
  links: SimulationLinkInput[];
}) {
  return {
    name: payload.name,
    description: payload.description ?? null,
    sdn: {
      ip: payload.sdn?.ip ?? "172.30.0.2",
      port: Number.isFinite(Number(payload.sdn?.port)) ? Math.max(1, Number(payload.sdn?.port)) : 3000,
      type_http: payload.sdn?.typeHttp === "https" ? "https" : "http"
    },
    nodes: payload.nodes.map((node, index) => ({
      uid: node.uid,
      node_id: Number.isFinite(Number(node.nodeId)) ? Number(node.nodeId) : index + 1,
      label: node.label,
      x: node.x,
      y: node.y
    })),
    links: payload.links.map((link) => ({
      uid: link.uid,
      source_uid: link.sourceUid,
      target_uid: link.targetUid,
      link_type: normalizeLinkType(link.linkType),
      distance_km:
        normalizeLinkType(link.linkType) === "PQC" ? 0 : normalizeDistanceKm(link.distanceKm),
      quditto_max_buffer_size:
        normalizeLinkType(link.linkType) === "PQC"
          ? 100
          : normalizeQudittoMaxBufferSize(link.qudittoMaxBufferSize),
      quditto_rate_r0:
        normalizeLinkType(link.linkType) === "PQC"
          ? DEFAULT_QUDITTO_RATE_R0_VALUE
          : normalizeQudittoRateR0(link.qudittoRateR0),
      quditto_rate_alpha:
        normalizeLinkType(link.linkType) === "PQC"
          ? DEFAULT_QUDITTO_RATE_ALPHA_VALUE
          : normalizeQudittoRateAlpha(link.qudittoRateAlpha)
    }))
  };
}

export class SimulationApiError extends Error {
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

function validateGraphInput(nodes: SimulationNodeInput[], links: SimulationLinkInput[]) {
  const nodeUidSet = new Set<string>();
  for (const node of nodes) {
    if (!node.uid) {
      throw new SimulationApiError(400, "Each node requires a uid");
    }
    if (!NODE_TYPES.includes(node.type)) {
      throw new SimulationApiError(400, `Invalid node type: ${node.type}`);
    }
    if (nodeUidSet.has(node.uid)) {
      throw new SimulationApiError(400, `Duplicate node uid: ${node.uid}`);
    }
    nodeUidSet.add(node.uid);
  }

  const linkUidSet = new Set<string>();
  for (const link of links) {
    if (!link.uid) {
      throw new SimulationApiError(400, "Each link requires a uid");
    }
    if (linkUidSet.has(link.uid)) {
      throw new SimulationApiError(400, `Duplicate link uid: ${link.uid}`);
    }
    if (!nodeUidSet.has(link.sourceUid) || !nodeUidSet.has(link.targetUid)) {
      throw new SimulationApiError(400, `Link ${link.uid} references unknown nodes`);
    }
    if (!LINK_TYPES.includes(link.linkType)) {
      throw new SimulationApiError(400, `Link ${link.uid} has invalid link_type`);
    }
    if (link.linkType !== "PQC") {
      if (!Number.isFinite(Number(link.distanceKm)) || Number(link.distanceKm) < 0) {
        throw new SimulationApiError(400, `Link ${link.uid} has invalid distance_km`);
      }
      if (!Number.isFinite(Number(link.qudittoMaxBufferSize)) || Number(link.qudittoMaxBufferSize) < 1) {
        throw new SimulationApiError(400, `Link ${link.uid} has invalid quditto_max_buffer_size`);
      }
      if (!Number.isFinite(Number(link.qudittoRateR0)) || Number(link.qudittoRateR0) <= 0) {
        throw new SimulationApiError(400, `Link ${link.uid} has invalid quditto_rate_r0`);
      }
      if (!Number.isFinite(Number(link.qudittoRateAlpha)) || Number(link.qudittoRateAlpha) < 0) {
        throw new SimulationApiError(400, `Link ${link.uid} has invalid quditto_rate_alpha`);
      }
    }
    linkUidSet.add(link.uid);
  }

  const validation = validateSimulationGraph(nodes, links);
  if (validation.errors.length > 0) {
    throw new SimulationApiError(400, validation.errors.join(" | "));
  }
}

export async function listSimulations(_ownerId: string | number, token: string): Promise<SimulationSummaryDTO[]> {
  const payload = await callOrchestrator(
    "/orch/web/simulations",
    token,
    _ownerId,
    { method: "GET" },
    "Failed to list simulations"
  );
  const simulations = Array.isArray(payload) ? (payload as RemoteSimulationSummary[]) : [];
  return simulations.map(toSimulationSummaryDTO);
}

export async function createSimulation(
  _ownerId: string | number,
  token: string,
  payload: {
    name: string;
    description?: string | null;
    sdn?: {
      ip?: string;
      port?: number;
      typeHttp?: "http" | "https";
    };
  }
): Promise<SimulationSummaryDTO> {
  const name = payload.name.trim();
  if (!name) {
    throw new SimulationApiError(400, "Simulation name is required");
  }

  const remoteBody = toRemoteUpsertPayload({
    name,
    description: payload.description ?? null,
    sdn: payload.sdn,
    nodes: [],
    links: []
  });

  const created = (await callOrchestrator(
    "/orch/web/simulations",
    token,
    _ownerId,
    {
      method: "POST",
      body: JSON.stringify(remoteBody)
    },
    "Failed to create simulation"
  )) as RemoteSimulation;

  return toSimulationSummaryDTO({
    id: created.id,
    name: created.name,
    description: created.description,
    status: created.status,
    created_at: created.created_at,
    updated_at: created.updated_at,
    node_count: created.nodes?.length ?? 0,
    link_count: created.links?.length ?? 0,
    sae_count: 0
  });
}

export async function getSimulation(
  _ownerId: string | number,
  token: string,
  simulationId: number
): Promise<SimulationDTO> {
  const remote = (await callOrchestrator(
    `/orch/web/simulations/${simulationId}`,
    token,
    _ownerId,
    { method: "GET" },
    "Failed to load simulation",
    simulationId
  )) as RemoteSimulation;
  return toSimulationDTO(remote);
}

export async function updateSimulation(
  ownerId: string | number,
  token: string,
  simulationId: number,
  payload: {
    name?: string;
    description?: string | null;
    sdn?: {
      ip?: string;
      port?: number;
      typeHttp?: "http" | "https";
    };
    nodes?: SimulationNodeInput[];
    links?: SimulationLinkInput[];
  }
): Promise<SimulationDTO> {
  const graphUpdateRequested = payload.nodes !== undefined || payload.links !== undefined;
  if (graphUpdateRequested && (!payload.nodes || !payload.links)) {
    throw new SimulationApiError(400, "When updating graph, both nodes and links are required");
  }

  if (payload.nodes && payload.links) {
    validateGraphInput(payload.nodes, payload.links);
  }

  const current = await getSimulation(ownerId, token, simulationId);
  const mergedName = payload.name !== undefined ? payload.name.trim() || "Untitled simulation" : current.name;
  const mergedDescription = payload.description !== undefined ? (payload.description?.trim() || null) : current.description;
  const mergedSdn = payload.sdn
    ? {
        ip: payload.sdn.ip ?? current.sdn.ip,
        port: payload.sdn.port ?? current.sdn.port,
        typeHttp: payload.sdn.typeHttp ?? current.sdn.typeHttp
      }
    : current.sdn;
  const mergedNodes = payload.nodes ?? current.nodes;
  const mergedLinks = payload.links ?? current.links;

  const remoteBody = toRemoteUpsertPayload({
    name: mergedName,
    description: mergedDescription,
    sdn: mergedSdn,
    nodes: mergedNodes,
    links: mergedLinks
  });

  const remote = (await callOrchestrator(
    `/orch/web/simulations/${simulationId}`,
    token,
    ownerId,
    {
      method: "PATCH",
      body: JSON.stringify(remoteBody)
    },
    "Failed to update simulation",
    simulationId
  )) as RemoteSimulation;

  return toSimulationDTO(remote);
}

export async function deleteSimulation(_ownerId: string | number, token: string, simulationId: number): Promise<void> {
  await callOrchestrator(
    `/orch/web/simulations/${simulationId}`,
    token,
    _ownerId,
    { method: "DELETE" },
    "Failed to delete simulation",
    simulationId
  );
}

export async function validateSimulation(ownerId: string | number, token: string, simulationId: number) {
  const simulation = await getSimulation(ownerId, token, simulationId);
  return validateSimulationGraph(simulation.nodes, simulation.links);
}

async function syncImportedSimulationSaes(
  ownerId: string | number,
  token: string,
  simulationId: number,
  importedSaes: ImportedSimulationSae[],
  simulationNodes: SimulationNodeInput[]
): Promise<void> {
  if (importedSaes.length === 0) {
    return;
  }

  const validNodeIds = new Set(
    simulationNodes
      .map((node) => Number(node.nodeId))
      .filter((nodeId) => Number.isFinite(nodeId) && nodeId > 0)
      .map((nodeId) => Math.trunc(nodeId))
  );
  for (const sae of importedSaes) {
    if (!validNodeIds.has(Number(sae.dkmsId))) {
      throw new SimulationApiError(
        400,
        `Imported SAE '${sae.saeId}' references unknown dkms_id=${sae.dkmsId}`
      );
    }
  }

  const existing = await listSimulationSaes(ownerId, token, simulationId);
  const existingBySaeId = new Set(existing.map((item) => item.saeId));

  for (const sae of importedSaes) {
    if (!existingBySaeId.has(sae.saeId)) {
      await createSimulationSae(ownerId, token, {
        simulationId,
        dkmsId: sae.dkmsId,
        saeId: sae.saeId,
        displayName: sae.displayName
      });
      continue;
    }

    await callOrchestrator(
      `/orch/api/sim/${simulationId}/sdn/sae/${encodeURIComponent(sae.saeId)}`,
      token,
      ownerId,
      {
        method: "PUT",
        headers: {
          "Content-Type": "application/json"
        },
        body: JSON.stringify({
          dkms_id: String(sae.dkmsId)
        })
      },
      `Failed to update SAE '${sae.saeId}' during import`,
      simulationId
    );
  }
}

export async function importSimulationTopology(
  ownerId: string | number,
  token: string,
  simulationId: number,
  payload: unknown
): Promise<SimulationDTO> {
  const current = await getSimulation(ownerId, token, simulationId);
  const imported = importTopologyToSimulationGraph(payload);
  const updated = await updateSimulation(ownerId, token, simulationId, {
    name: imported.name ?? current.name,
    description: imported.description ?? current.description,
    sdn: imported.sdn
      ? {
          ip: imported.sdn.ip,
          port: imported.sdn.port,
          typeHttp: imported.sdn.typeHttp
        }
      : current.sdn,
    nodes: imported.nodes,
    links: imported.links
  });
  if (Array.isArray(imported.saes)) {
    await syncImportedSimulationSaes(ownerId, token, simulationId, imported.saes, updated.nodes);
  }
  return updated;
}

export async function exportSimulation(
  ownerId: string | number,
  token: string,
  simulationId: number
): Promise<TopologyCanonicalV1> {
  const simulation = await getSimulation(ownerId, token, simulationId);
  const nodeIds = simulation.nodes
    .map((node) => Number(node.nodeId))
    .filter((nodeId) => Number.isFinite(nodeId) && nodeId > 0)
    .map((nodeId) => Math.trunc(nodeId))
    .sort((a, b) => a - b);

  const saeById = new Map<string, SaeAdminDTO>();
  if (nodeIds.length > 0) {
    const perNodeSaes = await Promise.all(
      nodeIds.map(async (nodeId) => {
        try {
          const items = await listSimulationSaes(ownerId, token, simulationId, nodeId);
          return items.map((item) => ({ ...item, dkmsId: nodeId }));
        } catch {
          return [] as SaeAdminDTO[];
        }
      })
    );
    for (const item of perNodeSaes.flat()) {
      saeById.set(item.saeId, item);
    }
  } else {
    for (const item of await listSimulationSaes(ownerId, token, simulationId)) {
      saeById.set(item.saeId, item);
    }
  }

  return exportSimulationTopology(simulation, { saes: Array.from(saeById.values()) });
}

export async function createSimulationRun(
  ownerId: string | number,
  token: string,
  simulationId: number
): Promise<SimulationRunDTO> {
  const remote = (await callOrchestrator(
    `/orch/web/simulations/${simulationId}/run`,
    token,
    ownerId,
    { method: "POST" },
    "Failed to run simulation",
    simulationId
  )) as RemoteRun;
  return toRunDTO(remote);
}

export async function listSimulationRuns(
  _ownerId: string | number,
  token: string,
  simulationId: number
): Promise<SimulationRunDTO[]> {
  const payload = await callOrchestrator(
    `/orch/web/simulations/${simulationId}/runs`,
    token,
    _ownerId,
    { method: "GET" },
    "Failed to list runs",
    simulationId
  );
  const runs = Array.isArray(payload) ? (payload as RemoteRun[]) : [];
  return runs.map(toRunDTO);
}

export async function stopSimulationRun(
  ownerId: string | number,
  token: string,
  simulationId: number
): Promise<{ status: string; simulationId: number; action: string }> {
  const payload = (await callOrchestrator(
    `/orch/web/simulations/${simulationId}/stop`,
    token,
    ownerId,
    { method: "POST" },
    "Failed to stop simulation",
    simulationId
  )) as RemoteActionResponse;

  return {
    status: String(payload.status ?? "ok"),
    simulationId: Number(payload.simulation_id ?? simulationId),
    action: String(payload.action ?? "stop")
  };
}

export async function stopSimulationDkms(
  ownerId: string | number,
  token: string,
  simulationId: number,
  dkmsId: number
): Promise<{ status: string; simulationId: number; action: string }> {
  const payload = (await callOrchestrator(
    `/orch/web/simulations/${simulationId}/dkms/${dkmsId}/stop`,
    token,
    ownerId,
    { method: "POST" },
    "Failed to stop DKMS",
    simulationId
  )) as RemoteActionResponse;

  return {
    status: String(payload.status ?? "ok"),
    simulationId: Number(payload.simulation_id ?? simulationId),
    action: String(payload.action ?? `stop-dkms-${dkmsId}`)
  };
}

export async function startSimulationDkms(
  ownerId: string | number,
  token: string,
  simulationId: number,
  dkmsId: number
): Promise<{ status: string; simulationId: number; action: string }> {
  const payload = (await callOrchestrator(
    `/orch/web/simulations/${simulationId}/dkms/${dkmsId}/start`,
    token,
    ownerId,
    { method: "POST" },
    "Failed to start DKMS",
    simulationId
  )) as RemoteActionResponse;

  return {
    status: String(payload.status ?? "ok"),
    simulationId: Number(payload.simulation_id ?? simulationId),
    action: String(payload.action ?? `start-dkms-${dkmsId}`)
  };
}

export async function listSimulationSaes(
  ownerId: string | number,
  token: string,
  simulationId: number,
  dkmsId?: number
): Promise<SaeAdminDTO[]> {
  const query = new URLSearchParams();
  query.set("simulation_id", String(simulationId));
  if (typeof dkmsId === "number" && Number.isFinite(dkmsId) && dkmsId > 0) {
    query.set("dkms_id", String(dkmsId));
  }
  const payload = await callOrchestrator(
    `/orch/admin/saes?${query.toString()}`,
    token,
    ownerId,
    { method: "GET" },
    "Failed to list SAEs"
  );
  const items = Array.isArray(payload) ? (payload as RemoteAdminSae[]) : [];
  return items.map(toSaeAdminDTO);
}

export async function createSimulationSae(
  ownerId: string | number,
  token: string,
  payload: {
    simulationId: number;
    dkmsId: number;
    saeId: string;
    displayName?: string | null;
  }
): Promise<SaeAdminDTO> {
  const body = {
    simulation_id: Number(payload.simulationId),
    dkms_id: Number(payload.dkmsId),
    sae_id: String(payload.saeId ?? "").trim(),
    display_name: payload.displayName?.trim() || undefined
  };
  if (!body.sae_id) {
    throw new SimulationApiError(400, "SAE id is required");
  }
  const remote = (await callOrchestrator(
    "/orch/admin/saes",
    token,
    ownerId,
    {
      method: "POST",
      body: JSON.stringify(body)
    },
    "Failed to create SAE"
  )) as RemoteAdminSae;
  return toSaeAdminDTO(remote);
}

export async function issueSimulationSaeServerSide(
  ownerId: string | number,
  token: string,
  simulationId: number,
  saeId: string,
  payload?: {
    keyType?: "ec-p256" | "rsa-2048";
    daysValid?: number;
    bundleFormat?: "pem" | "pkcs12";
    pkcs12Password?: string;
  }
): Promise<SaeIssueDTO> {
  const query = new URLSearchParams();
  query.set("simulation_id", String(simulationId));
  const remote = (await callOrchestrator(
    `/orch/admin/saes/${encodeURIComponent(saeId)}/issue?${query.toString()}`,
    token,
    ownerId,
    {
      method: "POST",
      body: JSON.stringify({
        key_type: payload?.keyType ?? "ec-p256",
        days_valid: Number(payload?.daysValid ?? 90),
        bundle_format: payload?.bundleFormat ?? "pem",
        pkcs12_password: payload?.pkcs12Password || undefined
      })
    },
    "Failed to issue SAE certificate"
  )) as RemoteSaeIssueResponse;
  return toSaeIssueDTO(remote);
}

export async function revokeSimulationSae(
  ownerId: string | number,
  token: string,
  simulationId: number,
  saeId: string,
  reason?: string
): Promise<SaeAdminDTO> {
  const query = new URLSearchParams();
  query.set("simulation_id", String(simulationId));
  const remote = (await callOrchestrator(
    `/orch/admin/saes/${encodeURIComponent(saeId)}/revoke?${query.toString()}`,
    token,
    ownerId,
    {
      method: "POST",
      body: JSON.stringify({
        reason: reason?.trim() || undefined
      })
    },
    "Failed to revoke SAE certificate"
  )) as RemoteAdminSae;
  return toSaeAdminDTO(remote);
}

export async function deleteSimulationSae(
  ownerId: string | number,
  token: string,
  simulationId: number,
  saeId: string
): Promise<{ status: "deleted"; saeId: string }> {
  const query = new URLSearchParams();
  query.set("simulation_id", String(simulationId));
  const remote = (await callOrchestrator(
    `/orch/admin/saes/${encodeURIComponent(saeId)}?${query.toString()}`,
    token,
    ownerId,
    {
      method: "DELETE"
    },
    "Failed to delete SAE"
  )) as { status?: string; sae_id?: string };
  return {
    status: "deleted",
    saeId: String(remote?.sae_id ?? saeId)
  };
}

export async function getSimulationSaeBundle(
  ownerId: string | number,
  token: string,
  simulationId: number,
  saeId: string,
  options?: {
    format?: "pem" | "pkcs12";
    pkcs12Password?: string;
  }
): Promise<SaeBundleDTO> {
  const query = new URLSearchParams();
  query.set("simulation_id", String(simulationId));
  query.set("format", options?.format === "pkcs12" ? "pkcs12" : "pem");
  if (options?.pkcs12Password) {
    query.set("pkcs12_password", options.pkcs12Password);
  }
  const remote = (await callOrchestrator(
    `/orch/admin/saes/${encodeURIComponent(saeId)}/bundle?${query.toString()}`,
    token,
    ownerId,
    { method: "GET" },
    "Failed to get SAE bundle"
  )) as RemoteSaeBundleResponse;
  return toSaeBundleDTO(remote);
}

export interface DkmsRuntimeLocatorDTO {
  nodeId: number;
  dkmsId: number;
  ingressId: number;
  runtimeBasePath: string;
}

function inferNodeIdFromRuntimeHost(host: Record<string, unknown>): number | null {
  const rawPort = Number(host?.port);
  if (Number.isFinite(rawPort) && rawPort > 4000) {
    return Math.trunc(rawPort - 4000);
  }
  const hostIp = typeof host?.ip === "string" ? String(host.ip).trim() : "";
  const match = hostIp.match(/^127\.0\.0\.(\d+)$/);
  if (!match) {
    return null;
  }
  const octet = Number.parseInt(match[1], 10);
  if (!Number.isFinite(octet) || octet <= 100) {
    return null;
  }
  return octet - 100;
}

export async function listSimulationDkmsRuntimeLocators(
  ownerId: string | number,
  token: string,
  simulationId: number,
  runtimeBaseUrl: string
): Promise<Record<string, DkmsRuntimeLocatorDTO>> {
  const payload = await callOrchestrator(
    `/orch/api/sim/${simulationId}`,
    token,
    ownerId,
    { method: "GET" },
    "Failed to load simulation runtime topology",
    simulationId
  );
  const rawDkms = Array.isArray((payload as any)?.list_dkms) ? (payload as any).list_dkms : [];
  const out: Record<string, DkmsRuntimeLocatorDTO> = {};
  for (const item of rawDkms as Array<Record<string, unknown>>) {
    const host = (item?.host as Record<string, unknown> | undefined) ?? {};
    const rawDkmsId = Number(item?.id);
    const rawIngressId = Number(item?.id_host ?? host?.id ?? item?.id);
    if (!Number.isFinite(rawDkmsId) || rawDkmsId <= 0) {
      continue;
    }
    if (!Number.isFinite(rawIngressId) || rawIngressId <= 0) {
      continue;
    }
    const nodeId = inferNodeIdFromRuntimeHost(host);
    if (!nodeId || nodeId <= 0) {
      continue;
    }
    out[String(nodeId)] = {
      nodeId,
      dkmsId: Math.trunc(rawDkmsId),
      ingressId: Math.trunc(rawIngressId),
      runtimeBasePath: `${runtimeBaseUrl.replace(/\/+$/, "")}/api/sim/${simulationId}/dkms/${Math.trunc(rawIngressId)}`
    };
  }
  return out;
}

export interface LoadTestDTO {
  testId: string;
  deploymentName: string;
  simulationId: number;
  grafanaUrl: string;
  replicas: number;
  readyReplicas: number;
  availableReplicas: number;
  createdAt: string | null;
}

export interface LoadTestCreateInput {
  startSaes: number;
  endSaes: number;
  stepSaes: number;
  intervalSeconds: number;
  offsetSeconds?: number | null;
  warmupSeconds?: number;
  keySizeBits?: number;
  perSaeLambda?: number;
  requestTimeoutSeconds?: number;
}

interface RemoteLoadTest {
  test_id: string;
  deployment_name: string;
  simulation_id: number;
  grafana_url: string;
  replicas?: number;
  ready_replicas?: number;
  available_replicas?: number;
  created_at?: string | null;
}

function toLoadTestDTO(remote: RemoteLoadTest): LoadTestDTO {
  return {
    testId: String(remote.test_id ?? ""),
    deploymentName: String(remote.deployment_name ?? ""),
    simulationId: Number(remote.simulation_id ?? 0),
    grafanaUrl: String(remote.grafana_url ?? ""),
    replicas: Number(remote.replicas ?? 0),
    readyReplicas: Number(remote.ready_replicas ?? 0),
    availableReplicas: Number(remote.available_replicas ?? 0),
    createdAt: remote.created_at ?? null
  };
}

export async function listSimulationLoadTests(
  ownerId: string | number,
  token: string,
  simulationId: number
): Promise<LoadTestDTO[]> {
  const payload = await callOrchestrator(
    `/orch/web/simulations/${simulationId}/tests`,
    token,
    ownerId,
    { method: "GET" },
    "Failed to list load tests",
    simulationId
  );
  const items = Array.isArray(payload) ? (payload as RemoteLoadTest[]) : [];
  return items.map(toLoadTestDTO);
}

export async function startSimulationLoadTest(
  ownerId: string | number,
  token: string,
  simulationId: number,
  input: LoadTestCreateInput
): Promise<LoadTestDTO> {
  const body = {
    start_saes: Math.max(0, Math.trunc(input.startSaes)),
    end_saes: Math.max(1, Math.trunc(input.endSaes)),
    step_saes: Math.max(1, Math.trunc(input.stepSaes)),
    interval_seconds: Number(input.intervalSeconds) || 10,
    offset_seconds:
      input.offsetSeconds === undefined || input.offsetSeconds === null
        ? null
        : Number(input.offsetSeconds),
    warmup_seconds: Number(input.warmupSeconds ?? 30),
    key_size_bits: Math.max(64, Math.trunc(input.keySizeBits ?? 256)),
    per_sae_lambda: Number(input.perSaeLambda ?? 0.5),
    request_timeout_seconds: Math.max(5, Math.trunc(input.requestTimeoutSeconds ?? 60))
  };
  const payload = (await callOrchestrator(
    `/orch/web/simulations/${simulationId}/tests`,
    token,
    ownerId,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body)
    },
    "Failed to start load test",
    simulationId
  )) as RemoteLoadTest;
  return toLoadTestDTO(payload);
}

export async function stopSimulationLoadTest(
  ownerId: string | number,
  token: string,
  simulationId: number,
  testId: string
): Promise<{ status: string; simulationId: number; action: string }> {
  const payload = (await callOrchestrator(
    `/orch/web/simulations/${simulationId}/tests/${encodeURIComponent(testId)}`,
    token,
    ownerId,
    { method: "DELETE" },
    "Failed to stop load test",
    simulationId
  )) as { status?: string; simulation_id?: number; action?: string };
  return {
    status: String(payload.status ?? "ok"),
    simulationId: Number(payload.simulation_id ?? simulationId),
    action: String(payload.action ?? `stop-test-${testId}`)
  };
}

export function errorToResponse(error: unknown): { status: number; message: string } {
  if (error instanceof SimulationApiError) {
    return {
      status: error.status,
      message: error.message
    };
  }

  if (error instanceof Error) {
    const normalized = error.message?.trim() || "Upstream request failed";
    const timeoutLike =
      /aborted|timeout|timed out|aborterror|failed to fetch/i.test(normalized);
    return {
      status: timeoutLike ? 504 : 502,
      message: normalized
    };
  }

  return {
    status: 500,
    message: "Internal server error"
  };
}
