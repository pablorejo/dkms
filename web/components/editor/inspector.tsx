"use client";

import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import type { FlowEdge, FlowNode } from "@/components/editor/types";
import type { SimulationStatus } from "@/lib/topology/types";
import { apiPath } from "@/lib/app-path";

interface Props {
  simulationId: number;
  simulationStatus: SimulationStatus;
  selectedNode: FlowNode | null;
  selectedEdge: FlowEdge | null;
  onSelectedNodeChange: (next: FlowNode) => void;
  onSelectedEdgeChange: (next: FlowEdge) => void;
  onDeleteSelection: () => void;
  deleteSelectionDisabled: boolean;
  onStartSelectedDkms: () => void;
  startSelectedDkmsDisabled: boolean;
  onStopSelectedDkms: () => void;
  stopSelectedDkmsDisabled: boolean;
  savingState: "idle" | "saving" | "error";
  actionMessage: string | null;
  readOnly: boolean;
  onSaeUpdated?: () => void;
}

type LinkChannel = "QKD" | "PQC" | "HYBRID";
type SaeAdminStatus = "pending_cert" | "active" | "revoked" | "expired";

interface LinkConfig {
  linkType: LinkChannel;
  distanceKm: number;
  qudittoMaxBufferSize: number;
  qudittoRateR0: number;
  qudittoRateAlpha: number;
}

interface SaeRecord {
  id: number;
  saeId: string;
  displayName: string | null;
  dkmsId: number | null;
  status: SaeAdminStatus;
  certFingerprint: string | null;
  certNotAfter: string | null;
  certSubject: string | null;
  certSerial: string | null;
  revokedAt: string | null;
}

interface DkmsRuntimeInfo {
  nodeId: number;
  ingressId: number;
  runtimeBasePath: string;
}

interface LinkConfigEditorProps {
  value: LinkConfig;
  disabled: boolean;
  onChange: (next: LinkConfig) => void;
}

function parseNonNegativeInt(raw: string): number {
  const value = Number.parseInt(raw.trim(), 10);
  if (!Number.isFinite(value) || value < 0) {
    return 0;
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

function parsePositiveFloat(raw: string, fallback: number): number {
  const value = Number.parseFloat(raw.trim());
  if (!Number.isFinite(value) || value <= 0) {
    return fallback;
  }
  return value;
}

function parseNonNegativeFloat(raw: string, fallback: number): number {
  const value = Number.parseFloat(raw.trim());
  if (!Number.isFinite(value) || value < 0) {
    return fallback;
  }
  return value;
}

function normalizeLinkConfig(edge: FlowEdge): LinkConfig {
  const linkType = (edge.data?.linkType ?? "QKD") as LinkChannel;
  const distanceKm =
    Number.isFinite(Number(edge.data?.distanceKm)) && Number(edge.data?.distanceKm) >= 0
      ? Math.trunc(Number(edge.data?.distanceKm))
      : 0;
  const qudittoMaxBufferSize =
    Number.isFinite(Number(edge.data?.qudittoMaxBufferSize)) && Number(edge.data?.qudittoMaxBufferSize) >= 1
      ? Math.trunc(Number(edge.data?.qudittoMaxBufferSize))
      : 100;
  const qudittoRateR0 =
    Number.isFinite(Number(edge.data?.qudittoRateR0)) && Number(edge.data?.qudittoRateR0) > 0
      ? Number(edge.data?.qudittoRateR0)
      : 120;
  const qudittoRateAlpha =
    Number.isFinite(Number(edge.data?.qudittoRateAlpha)) && Number(edge.data?.qudittoRateAlpha) >= 0
      ? Number(edge.data?.qudittoRateAlpha)
      : 0.2;

  return { linkType, distanceKm, qudittoMaxBufferSize, qudittoRateR0, qudittoRateAlpha };
}

function statusBadgeClass(status: SaeAdminStatus): string {
  if (status === "active") {
    return "border-emerald-200 bg-emerald-50 text-emerald-700";
  }
  if (status === "revoked") {
    return "border-red-200 bg-red-50 text-red-700";
  }
  if (status === "expired") {
    return "border-amber-200 bg-amber-50 text-amber-700";
  }
  return "border-border bg-muted text-muted-foreground";
}

function formatDate(raw: string | null | undefined): string {
  if (!raw) {
    return "-";
  }
  const value = new Date(raw);
  if (Number.isNaN(value.getTime())) {
    return raw;
  }
  return value.toLocaleString();
}

function shortFingerprint(raw: string | null | undefined): string {
  const text = String(raw ?? "").trim();
  if (!text) {
    return "-";
  }
  if (text.length <= 24) {
    return text;
  }
  return `${text.slice(0, 12)}...${text.slice(-8)}`;
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

function sanitizeFileStem(raw: string): string {
  const value = String(raw ?? "").trim();
  if (!value) {
    return "sae";
  }
  return value.replace(/[^a-zA-Z0-9._-]+/g, "_");
}

function LinkConfigEditor({ value, disabled, onChange }: LinkConfigEditorProps) {
  const applyPreset = (preset: LinkConfig) => onChange(preset);
  const usesQkdParams = value.linkType !== "PQC";

  return (
    <div className="space-y-3 rounded-md border border-border bg-muted/20 p-3">
      <div className="space-y-1">
        <Label>Channel type</Label>
        <div className="grid grid-cols-3 gap-2">
          <Button
            type="button"
            size="sm"
            variant={value.linkType === "QKD" ? "default" : "outline"}
            onClick={() => onChange({ ...value, linkType: "QKD" })}
            disabled={disabled}
          >
            QKD
          </Button>
          <Button
            type="button"
            size="sm"
            variant={value.linkType === "PQC" ? "default" : "outline"}
            onClick={() => onChange({ ...value, linkType: "PQC" })}
            disabled={disabled}
          >
            PQC
          </Button>
          <Button
            type="button"
            size="sm"
            variant={value.linkType === "HYBRID" ? "default" : "outline"}
            onClick={() => onChange({ ...value, linkType: "HYBRID" })}
            disabled={disabled}
          >
            HYBRID
          </Button>
        </div>
      </div>

      {usesQkdParams ? (
        <>
          <div className="space-y-1">
            <div className="flex items-center justify-between">
              <Label>Distance (km)</Label>
              <span className="text-xs font-medium text-muted-foreground">{value.distanceKm}</span>
            </div>
            <Input
              type="range"
              min={0}
              max={500}
              step={1}
              value={value.distanceKm}
              onChange={(e) => onChange({ ...value, distanceKm: parseNonNegativeInt(e.target.value) })}
              disabled={disabled}
            />
            <Input
              type="number"
              min={0}
              step={1}
              value={value.distanceKm}
              onChange={(e) => onChange({ ...value, distanceKm: parseNonNegativeInt(e.target.value) })}
              disabled={disabled}
            />
          </div>

          <div className="space-y-1">
            <div className="flex items-center justify-between">
              <Label>Buffer size</Label>
              <span className="text-xs font-medium text-muted-foreground">{value.qudittoMaxBufferSize}</span>
            </div>
            <Input
              type="range"
              min={1}
              max={2000}
              step={1}
              value={value.qudittoMaxBufferSize}
              onChange={(e) => onChange({ ...value, qudittoMaxBufferSize: parsePositiveInt(e.target.value) })}
              disabled={disabled}
            />
            <Input
              type="number"
              min={1}
              step={1}
              value={value.qudittoMaxBufferSize}
              onChange={(e) => onChange({ ...value, qudittoMaxBufferSize: parsePositiveInt(e.target.value) })}
              disabled={disabled}
            />
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <Label>R0 (keys/min)</Label>
              <Input
                type="number"
                min={0.0001}
                step={0.1}
                value={value.qudittoRateR0}
                onChange={(e) =>
                  onChange({ ...value, qudittoRateR0: parsePositiveFloat(e.target.value, value.qudittoRateR0) })
                }
                disabled={disabled}
              />
            </div>

            <div className="space-y-1">
              <Label>alpha</Label>
              <Input
                type="number"
                min={0}
                step={0.01}
                value={value.qudittoRateAlpha}
                onChange={(e) =>
                  onChange({
                    ...value,
                    qudittoRateAlpha: parseNonNegativeFloat(e.target.value, value.qudittoRateAlpha)
                  })
                }
                disabled={disabled}
              />
            </div>
          </div>

          <p className="rounded-md border border-border bg-background/70 px-2 py-2 text-xs text-muted-foreground">
            Rate model: <code>R(L) = R0 * 10^(-alpha * L / 10)</code>
          </p>
        </>
      ) : (
        <p className="rounded-md border border-border bg-muted/20 px-2 py-1 text-xs text-muted-foreground">
          PQC links do not expose QKD distance, buffer or rate-model parameters.
        </p>
      )}

      <div className="space-y-1">
        <Label>Quick presets</Label>
        <div className="grid grid-cols-1 gap-2">
          <Button
            type="button"
            size="sm"
            variant="outline"
            onClick={() =>
              applyPreset({
                linkType: "QKD",
                distanceKm: 10,
                qudittoMaxBufferSize: 100,
                qudittoRateR0: 120,
                qudittoRateAlpha: 0.2
              })
            }
            disabled={disabled}
          >
            Metro QKD · 10km · R0 120
          </Button>
          <Button
            type="button"
            size="sm"
            variant="outline"
            onClick={() =>
              applyPreset({
                linkType: "QKD",
                distanceKm: 80,
                qudittoMaxBufferSize: 180,
                qudittoRateR0: 120,
                qudittoRateAlpha: 0.2
              })
            }
            disabled={disabled}
          >
            Long QKD · 80km · alpha 0.2
          </Button>
          <Button
            type="button"
            size="sm"
            variant="outline"
            onClick={() =>
              applyPreset({
                linkType: "PQC",
                distanceKm: 0,
                qudittoMaxBufferSize: 100,
                qudittoRateR0: 120,
                qudittoRateAlpha: 0.2
              })
            }
            disabled={disabled}
          >
            Backbone PQC
          </Button>
          <Button
            type="button"
            size="sm"
            variant="outline"
            onClick={() =>
              applyPreset({
                linkType: "HYBRID",
                distanceKm: 30,
                qudittoMaxBufferSize: 128,
                qudittoRateR0: 120,
                qudittoRateAlpha: 0.2
              })
            }
            disabled={disabled}
          >
            HYBRID · 30km · R0 120
          </Button>
        </div>
      </div>
    </div>
  );
}

export function Inspector({
  simulationId,
  simulationStatus,
  selectedNode,
  selectedEdge,
  onSelectedNodeChange,
  onSelectedEdgeChange,
  onDeleteSelection,
  deleteSelectionDisabled,
  onStartSelectedDkms,
  startSelectedDkmsDisabled,
  onStopSelectedDkms,
  stopSelectedDkmsDisabled,
  savingState,
  actionMessage,
  readOnly,
  onSaeUpdated
}: Props) {
  const [saes, setSaes] = useState<SaeRecord[]>([]);
  const [saeLoading, setSaeLoading] = useState(false);
  const [saeBusy, setSaeBusy] = useState(false);
  const [saeCreatePending, setSaeCreatePending] = useState(false);
  const [saeCreateStatus, setSaeCreateStatus] = useState<string | null>(null);
  const [saeError, setSaeError] = useState<string | null>(null);
  const [newSaeId, setNewSaeId] = useState("");
  const [newSaeDisplayName, setNewSaeDisplayName] = useState("");
  const [dkmsRuntimeMap, setDkmsRuntimeMap] = useState<Record<string, DkmsRuntimeInfo>>({});
  const [pendingDeleteSaeId, setPendingDeleteSaeId] = useState<string | null>(null);

  const saveLabel = savingState === "saving" ? "Autosaving..." : savingState === "error" ? "Save failed" : "Saved";
  const isRunning = simulationStatus === "running";
  const selectedSaeNode = selectedNode?.data.nodeType === "SAE" ? selectedNode : null;
  const showDkmsActions = isRunning && selectedNode?.data.nodeType === "DKMS";
  const selectedDkmsIdRaw =
    selectedNode?.data.nodeType === "DKMS"
      ? Number(selectedNode.data.nodeId)
      : selectedNode?.data.nodeType === "SAE"
        ? Number(selectedNode.data.parentDkmsId)
        : 0;
  const selectedDkmsId = Number.isFinite(selectedDkmsIdRaw) && selectedDkmsIdRaw > 0 ? selectedDkmsIdRaw : 0;
  const selectedDkmsRuntime = selectedDkmsId ? dkmsRuntimeMap[String(selectedDkmsId)] ?? null : null;
  const selectedSaeRuntimeUrl = selectedSaeNode?.data.runtimeBasePath ?? selectedDkmsRuntime?.runtimeBasePath ?? null;
  const saeControlsDisabled = saeBusy || saeCreatePending;

  const mapSaeRecords = (payload: any): SaeRecord[] =>
    Array.isArray(payload?.saes)
      ? payload.saes.map((item: any) => ({
          id: Number(item.id ?? 0),
          saeId: String(item.saeId ?? ""),
          displayName: item.displayName ?? null,
          dkmsId:
            Number.isFinite(Number(item.dkmsId)) && Number(item.dkmsId) > 0
              ? Number(item.dkmsId)
              : null,
          status: String(item.status ?? "pending_cert") as SaeAdminStatus,
          certFingerprint: item.certFingerprint ?? null,
          certNotAfter: item.certNotAfter ?? null,
          certSubject: item.certSubject ?? null,
          certSerial: item.certSerial ?? null,
          revokedAt: item.revokedAt ?? null
        }))
      : [];

  const handleCopyRuntimeUrl = async (runtimeUrl?: string | null) => {
    const url = runtimeUrl ?? selectedDkmsRuntime?.runtimeBasePath;
    if (!url) {
      return;
    }
    try {
      await navigator.clipboard.writeText(url);
      setSaeError(null);
    } catch {
      setSaeError("Could not copy runtime URL to clipboard.");
    }
  };

  useEffect(() => {
    if (selectedNode?.data.nodeType === "DKMS") {
      return;
    }
    setSaes((current) => (current.length === 0 ? current : []));
    setSaeError((current) => (current === null ? current : null));
  }, [selectedNode?.data.nodeType]);

  useEffect(() => {
    let alive = true;
    const loadRuntimeMap = async () => {
      try {
        const response = await fetch(apiPath(`/api/simulations/${simulationId}/dkms-runtime`), {
          method: "GET",
          cache: "no-store"
        });
        const payload = await response.json().catch(() => ({}));
        if (!response.ok) {
          if (!alive) {
            return;
          }
          const detail = String(payload?.error ?? "Failed to load DKMS runtime map");
          setSaeError((current) => current ?? detail);
          return;
        }
        const mapping = payload?.byNodeId;
        const parsed: Record<string, DkmsRuntimeInfo> = {};
        if (mapping && typeof mapping === "object") {
          for (const [nodeId, value] of Object.entries(mapping as Record<string, any>)) {
            const nodeIdInt = Number.parseInt(String((value as any)?.nodeId ?? nodeId), 10);
            const ingressIdInt = Number.parseInt(String((value as any)?.ingressId ?? "0"), 10);
            const runtimeBasePath = String((value as any)?.runtimeBasePath ?? "").trim();
            if (!Number.isFinite(nodeIdInt) || nodeIdInt <= 0) {
              continue;
            }
            if (!Number.isFinite(ingressIdInt) || ingressIdInt <= 0) {
              continue;
            }
            if (!runtimeBasePath) {
              continue;
            }
            parsed[String(nodeIdInt)] = {
              nodeId: nodeIdInt,
              ingressId: ingressIdInt,
              runtimeBasePath
            };
          }
        }
        if (!alive) {
          return;
        }
        setDkmsRuntimeMap(parsed);
      } catch {
        if (!alive) {
          return;
        }
        setDkmsRuntimeMap({});
      }
    };

    void loadRuntimeMap();
    return () => {
      alive = false;
    };
  }, [simulationId]);

  useEffect(() => {
    if (!selectedDkmsId || selectedNode?.data.nodeType !== "DKMS") {
      setSaes([]);
      setSaeError(null);
      setSaeLoading(false);
      return;
    }

    let alive = true;
    const controller = new AbortController();

    const loadSaes = async () => {
      try {
        setSaeLoading(true);
        setSaeError(null);
        const response = await fetch(
          apiPath(`/api/simulations/${simulationId}/saes?dkmsId=${selectedDkmsId}`),
          {
            method: "GET",
            cache: "no-store",
            signal: controller.signal
          }
        );
        const payload = await response.json().catch(() => ({}));
        if (!response.ok) {
          throw new Error(String(payload?.error ?? "Failed to load SAEs"));
        }
        const items = mapSaeRecords(payload);

        if (!alive) {
          return;
        }

        setSaes(items);
      } catch (error) {
        if (!alive || (error instanceof DOMException && error.name === "AbortError")) {
          return;
        }
        setSaeError(error instanceof Error ? error.message : "Failed to load SAEs");
      } finally {
        if (alive) {
          setSaeLoading(false);
        }
      }
    };

    void loadSaes();
    const intervalId = window.setInterval(() => {
      void loadSaes();
    }, 15000);

    return () => {
      alive = false;
      controller.abort();
      window.clearInterval(intervalId);
    };
  }, [simulationId, selectedDkmsId, selectedNode?.data.nodeType]);

  const refreshSaes = async (): Promise<SaeRecord[]> => {
    if (!selectedDkmsId) {
      return [];
    }
    const response = await fetch(apiPath(`/api/simulations/${simulationId}/saes?dkmsId=${selectedDkmsId}`), {
      method: "GET",
      cache: "no-store"
    });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) {
      throw new Error(String(payload?.error ?? "Failed to refresh SAEs"));
    }
    const items = mapSaeRecords(payload);
    setSaes(items);
    return items;
  };

  const waitForSaeVisible = async (saeId: string): Promise<void> => {
    const timeoutMs = 30000;
    const pollEveryMs = 700;
    const deadline = Date.now() + timeoutMs;

    while (Date.now() < deadline) {
      const items = await refreshSaes();
      if (items.some((item) => item.saeId === saeId)) {
        return;
      }
      await new Promise<void>((resolve) => {
        window.setTimeout(resolve, pollEveryMs);
      });
    }
    throw new Error(`SAE ${saeId} was not visible after ${Math.trunc(timeoutMs / 1000)} seconds`);
  };

  const handleCreateSae = async () => {
    if (!selectedDkmsId) {
      return;
    }
    const saeId = newSaeId.trim();
    if (!saeId) {
      setSaeError("sae_id is required");
      return;
    }

    try {
      setSaeBusy(true);
      setSaeCreatePending(true);
      setSaeCreateStatus("Creating SAE...");
      setSaeError(null);
      const response = await fetch(apiPath(`/api/simulations/${simulationId}/saes`), {
        method: "POST",
        headers: {
          "Content-Type": "application/json"
        },
        body: JSON.stringify({
          dkmsId: selectedDkmsId,
          saeId,
          displayName: newSaeDisplayName.trim() || undefined
        })
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(String(payload?.error ?? "Failed to create SAE"));
      }
      const createdSaeId = String(payload?.sae?.saeId ?? saeId);
      setSaeCreateStatus(`Finalizing SAE ${createdSaeId}...`);
      await waitForSaeVisible(createdSaeId);
      setNewSaeId("");
      setNewSaeDisplayName("");
      onSaeUpdated?.();
    } catch (error) {
      setSaeError(error instanceof Error ? error.message : "Failed to create SAE");
    } finally {
      setSaeCreatePending(false);
      setSaeCreateStatus(null);
      setSaeBusy(false);
    }
  };

  const downloadMtlsKitFiles = ({
    saeId,
    certPem,
    keyPem,
    caPem
  }: {
    saeId: string;
    certPem: string;
    keyPem: string;
    caPem: string;
  }) => {
    if (!selectedDkmsRuntime?.runtimeBasePath) {
      throw new Error("DKMS runtime URL is not available for this node");
    }
    const certText = String(certPem ?? "").trim();
    const keyText = String(keyPem ?? "").trim();
    const caText = String(caPem ?? "").trim();
    if (!certText || !keyText || !caText) {
      throw new Error("mTLS bundle requires certificate, private key and CA chain");
    }

    const stem = sanitizeFileStem(saeId);
    const certFilename = `${stem}.client.crt.pem`;
    const keyFilename = `${stem}.client.key.pem`;
    const caFilename = `${stem}.ca.crt.pem`;
    const scriptFilename = `${stem}.etsi-curl.sh`;

    const runtimeBasePath = selectedDkmsRuntime.runtimeBasePath;
    const script = [
      "#!/usr/bin/env bash",
      "set -euo pipefail",
      "",
      `RUNTIME_BASE_URL="${runtimeBasePath}"`,
      `CALLER_SAE_ID="${saeId}"`,
      `CERT_FILE="${certFilename}"`,
      `KEY_FILE="${keyFilename}"`,
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
      `# Note: ${caFilename} is the SAE/runtime CA bundle, not the public HTTPS server CA.`,
      "# Optional: add --cacert <server-ca.pem> only if your runtime HTTPS cert is not publicly trusted.",
      ""
    ].join("\n");

    downloadBlob(certFilename, new Blob([certText], { type: "application/x-pem-file" }));
    downloadBlob(keyFilename, new Blob([keyText], { type: "application/x-pem-file" }));
    downloadBlob(caFilename, new Blob([caText], { type: "application/x-pem-file" }));
    downloadBlob(scriptFilename, new Blob([script], { type: "text/x-shellscript" }));
  };

  const issueSaeCertificate = async (
    saeId: string
  ): Promise<{ certPem: string; keyPem: string; caPem: string }> => {
    const response = await fetch(apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(saeId)}/issue`), {
      method: "POST",
      headers: {
        "Content-Type": "application/json"
      },
      body: JSON.stringify({
        keyType: "ec-p256",
        daysValid: 90,
        bundleFormat: "pem"
      })
    });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) {
      throw new Error(String(payload?.error ?? "Failed to issue certificate"));
    }
    const issued = payload?.issued;
    const certPem = String(issued?.certificatePem ?? "").trim();
    const keyPem = String(issued?.privateKeyPem ?? "").trim();
    const caPem = String(issued?.caChainPem ?? "").trim();
    if (!certPem || !keyPem || !caPem) {
      throw new Error("Issue response did not include a complete mTLS bundle");
    }
    return { certPem, keyPem, caPem };
  };

  const handleActivateSae = async (saeId: string) => {
    try {
      setSaeBusy(true);
      setSaeError(null);
      await issueSaeCertificate(saeId);
      await refreshSaes();
      onSaeUpdated?.();
    } catch (error) {
      setSaeError(error instanceof Error ? error.message : "Failed to activate SAE");
    } finally {
      setSaeBusy(false);
    }
  };

  const handleRevokeSae = async (saeId: string) => {
    try {
      setSaeBusy(true);
      setSaeError(null);
      const response = await fetch(
        apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(saeId)}/revoke`),
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json"
          },
          body: JSON.stringify({})
        }
      );
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(String(payload?.error ?? "Failed to revoke certificate"));
      }
      await refreshSaes();
      onSaeUpdated?.();
    } catch (error) {
      setSaeError(error instanceof Error ? error.message : "Failed to revoke certificate");
    } finally {
      setSaeBusy(false);
    }
  };

  const performDeleteSae = async (saeId: string) => {
    try {
      setSaeBusy(true);
      setSaeError(null);
      const response = await fetch(apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(saeId)}`), {
        method: "DELETE"
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(String(payload?.error ?? "Failed to delete SAE"));
      }
      await refreshSaes();
      onSaeUpdated?.();
    } catch (error) {
      setSaeError(error instanceof Error ? error.message : "Failed to delete SAE");
    } finally {
      setSaeBusy(false);
    }
  };

  const handleDeleteSae = (saeId: string) => {
    setPendingDeleteSaeId(saeId);
  };

  const handleDownloadSaeKit = async (saeId: string) => {
    try {
      setSaeBusy(true);
      setSaeError(null);
      const response = await fetch(
        apiPath(`/api/simulations/${simulationId}/saes/${encodeURIComponent(saeId)}/bundle?format=pem`),
        {
          method: "GET",
          cache: "no-store"
        }
      );
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(String(payload?.error ?? "Failed to get SAE bundle"));
      }
      const bundle = payload?.bundle ?? {};
      let certPem = String(bundle?.certificatePem ?? "").trim();
      let keyPem = String(bundle?.privateKeyPem ?? "").trim();
      let caPem = String(bundle?.caChainPem ?? "").trim();
      if (!certPem || !keyPem || !caPem) {
        const issued = await issueSaeCertificate(saeId);
        certPem = issued.certPem;
        keyPem = issued.keyPem;
        caPem = issued.caPem;
        await refreshSaes();
        onSaeUpdated?.();
      }
      downloadMtlsKitFiles({ saeId, certPem, keyPem, caPem });
    } catch (error) {
      setSaeError(error instanceof Error ? error.message : "Failed to download mTLS kit");
    } finally {
      setSaeBusy(false);
    }
  };

  return (
    <Card className="relative h-full shadow-sm">
      {saeCreatePending ? (
        <div className="fixed inset-0 z-[120] flex items-center justify-center bg-white/75 p-4 backdrop-blur-[1px]">
          <div
            role="status"
            aria-live="polite"
            className="rounded-md border border-border bg-card px-4 py-3 text-center shadow-sm"
          >
            <p className="text-sm font-medium text-foreground">
              {saeCreateStatus ?? "Creando SAE…"}
            </p>
            <p className="mt-1 text-xs text-muted-foreground">
              Espera hasta que el SAE esté completamente disponible.
            </p>
          </div>
        </div>
      ) : null}
      <CardHeader>
        <CardTitle className="text-base">Inspector</CardTitle>
      </CardHeader>
      <CardContent className="space-y-4 text-sm">
        <div className="flex items-center justify-between rounded border border-border bg-muted/30 px-2 py-1 text-xs">
          <span>Persist</span>
          <span>{saveLabel}</span>
        </div>
        <div className="rounded border border-border bg-muted/20 px-2 py-1 text-xs text-muted-foreground">
          {readOnly
            ? "Infrastructure transition in progress: editor is temporarily locked."
            : simulationStatus === "running"
              ? "Running: you can create/delete PQC links and convert QKD <-> HYBRID."
              : "Stopped: topology and values are editable, Grafana is disabled."}
        </div>

        <Separator />

        {!selectedNode && !selectedEdge ? (
          <div className="rounded-md border border-dashed border-border bg-muted/50 p-3 text-xs text-muted-foreground">
            Select a node or link in the canvas to edit its configuration here.
          </div>
        ) : null}

        {selectedNode ? (
          <div className="space-y-2">
            <h3 className="font-semibold">Node: {selectedNode.id}</h3>
            <p className="text-xs text-muted-foreground">Type: {selectedNode.data.nodeType}</p>
            {selectedNode.data.nodeType === "DKMS" ? (
              <>
                <div className="space-y-2">
                  <Label>Label</Label>
                  <Input
                    value={selectedNode.data.label}
                    onChange={(e) => onSelectedNodeChange({ ...selectedNode, data: { ...selectedNode.data, label: e.target.value } })}
                    disabled={readOnly || isRunning}
                  />
                </div>
                <div className="space-y-2">
                  <Label>node_id</Label>
                  <Input
                    type="number"
                    value={selectedNode.data.nodeId ?? ""}
                    readOnly
                    disabled
                  />
                </div>
                {showDkmsActions ? (
                  <div className="grid grid-cols-2 gap-2">
                    <Button
                      type="button"
                      variant="secondary"
                      size="sm"
                      onClick={onStartSelectedDkms}
                      disabled={startSelectedDkmsDisabled}
                    >
                      Start DKMS
                    </Button>
                    <Button
                      type="button"
                      variant="destructive"
                      size="sm"
                      onClick={onStopSelectedDkms}
                      disabled={stopSelectedDkmsDisabled}
                    >
                      Stop DKMS
                    </Button>
                  </div>
                ) : null}

                <div className="mt-2 space-y-2">
                  {selectedDkmsRuntime?.runtimeBasePath ? (
                    <div className="rounded-md border border-border bg-muted/20 p-2 text-xs">
                      <p className="mb-1 font-medium text-foreground">DKMS Runtime URL</p>
                      <div className="flex items-center gap-2">
                        <a
                          href={selectedDkmsRuntime.runtimeBasePath}
                          target="_blank"
                          rel="noreferrer"
                          className="truncate text-blue-700 underline-offset-2 hover:underline"
                          title={selectedDkmsRuntime.runtimeBasePath}
                        >
                          {selectedDkmsRuntime.runtimeBasePath}
                        </a>
                        <Button
                          type="button"
                          size="sm"
                          variant="outline"
                          onClick={() => void handleCopyRuntimeUrl(selectedDkmsRuntime.runtimeBasePath)}
                        >
                          Copy
                        </Button>
                      </div>
                    </div>
                  ) : (
                    <p className="text-xs text-muted-foreground">DKMS Runtime URL: Unavailable</p>
                  )}
                  <div className="rounded-md border border-border bg-muted/20 p-2">
                    <Label className="text-xs">Create SAE</Label>
                    <div className="mt-2 grid grid-cols-1 gap-2">
                      <Input
                        placeholder="sae_id"
                        value={newSaeId}
                        onChange={(event) => setNewSaeId(event.target.value)}
                        disabled={saeControlsDisabled}
                      />
                      <Input
                        placeholder="Display name (optional)"
                        value={newSaeDisplayName}
                        onChange={(event) => setNewSaeDisplayName(event.target.value)}
                        disabled={saeControlsDisabled}
                      />
                      <Button
                        type="button"
                        size="sm"
                        onClick={() => void handleCreateSae()}
                        disabled={saeControlsDisabled}
                      >
                        Create SAE
                      </Button>
                    </div>
                  </div>

                  {saeLoading ? <p className="text-xs text-muted-foreground">Loading SAEs...</p> : null}

                  {saes.length === 0 && !saeLoading ? (
                    <p className="text-xs text-muted-foreground">No SAEs registered for this DKMS.</p>
                  ) : null}

                  {saes.map((sae) => (
                    <div key={sae.id} className="space-y-2 rounded-md border border-border bg-muted/10 p-2">
                      <div className="flex items-start justify-between gap-2">
                        <div>
                          <p className="font-medium">{sae.saeId}</p>
                          <p className="text-xs text-muted-foreground">{sae.displayName || "-"}</p>
                        </div>
                        <span className={`rounded border px-2 py-0.5 text-xs ${statusBadgeClass(sae.status)}`}>
                          {sae.status}
                        </span>
                      </div>
                      <p className="text-xs text-muted-foreground">
                        Expires: {formatDate(sae.certNotAfter)}
                      </p>
                      <p className="text-xs text-muted-foreground">
                        Fingerprint: {shortFingerprint(sae.certFingerprint)}
                      </p>
                      <div className="grid grid-cols-2 gap-2">
                        <Button
                          type="button"
                          size="sm"
                          onClick={() => void handleActivateSae(sae.saeId)}
                          disabled={saeControlsDisabled || sae.status === "active"}
                        >
                          Activate
                        </Button>
                        <Button
                          type="button"
                          size="sm"
                          variant="secondary"
                          onClick={() => void handleDownloadSaeKit(sae.saeId)}
                          disabled={saeControlsDisabled || !selectedDkmsRuntime?.runtimeBasePath}
                        >
                          Download mTLS Kit
                        </Button>
                        <Button
                          type="button"
                          size="sm"
                          variant="destructive"
                          onClick={() => void handleRevokeSae(sae.saeId)}
                          disabled={saeControlsDisabled || sae.status === "revoked"}
                        >
                          Revoke
                        </Button>
                        <Button
                          type="button"
                          size="sm"
                          variant="outline"
                          onClick={() => void handleDeleteSae(sae.saeId)}
                          disabled={saeControlsDisabled}
                        >
                          Delete SAE
                        </Button>
                      </div>
                    </div>
                  ))}
                </div>

                {saeError ? (
                  <p className="rounded border border-red-200 bg-red-50 px-2 py-1 text-xs text-red-700">{saeError}</p>
                ) : null}
                <Button
                  type="button"
                  variant="destructive"
                  size="sm"
                  onClick={onDeleteSelection}
                  disabled={deleteSelectionDisabled}
                >
                  Delete Selected Node
                </Button>
              </>
            ) : selectedNode.data.nodeType === "SAE" ? (
              <div className="space-y-3 rounded-md border border-border bg-muted/10 p-3">
                <div>
                  <p className="text-xs text-muted-foreground">SAE ID</p>
                  <p className="font-semibold">{selectedNode.data.saeId ?? "-"}</p>
                </div>
                <div>
                  <p className="text-xs text-muted-foreground">Display name</p>
                  <p className="font-medium">{selectedNode.data.saeDisplayName || "-"}</p>
                </div>
                <div>
                  <p className="text-xs text-muted-foreground">Status</p>
                  <span className={`rounded border px-2 py-0.5 text-xs ${statusBadgeClass((selectedNode.data.saeStatus ?? "pending_cert") as SaeAdminStatus)}`}>
                    {selectedNode.data.saeStatus ?? "pending_cert"}
                  </span>
                </div>
                <p className="text-xs text-muted-foreground">Connected DKMS: {selectedNode.data.parentDkmsId ?? "-"}</p>
                <p className="text-xs text-muted-foreground">
                  Expires: {formatDate(selectedNode.data.saeCertNotAfter ?? null)}
                </p>
                <p className="text-xs text-muted-foreground">
                  Fingerprint: {shortFingerprint(selectedNode.data.saeCertFingerprint ?? null)}
                </p>
                {selectedSaeRuntimeUrl ? (
                  <div className="rounded-md border border-border bg-muted/20 p-2 text-xs">
                    <p className="mb-1 font-medium text-foreground">Runtime URL</p>
                    <div className="flex items-center gap-2">
                      <a
                        href={selectedSaeRuntimeUrl}
                        target="_blank"
                        rel="noreferrer"
                        className="truncate text-blue-700 underline-offset-2 hover:underline"
                        title={selectedSaeRuntimeUrl}
                      >
                        {selectedSaeRuntimeUrl}
                      </a>
                      <Button type="button" size="sm" variant="outline" onClick={() => void handleCopyRuntimeUrl(selectedSaeRuntimeUrl)}>
                        Copy
                      </Button>
                    </div>
                  </div>
                ) : null}
                {isRunning ? (
                  <div className="grid grid-cols-1 gap-2">
                    <Button
                      type="button"
                      size="sm"
                      onClick={() => void handleActivateSae(String(selectedNode.data.saeId ?? ""))}
                      disabled={
                        saeControlsDisabled ||
                        !String(selectedNode.data.saeId ?? "").trim() ||
                        selectedNode.data.saeStatus === "active"
                      }
                    >
                      Activate SAE
                    </Button>
                    <Button
                      type="button"
                      size="sm"
                      variant="secondary"
                      onClick={() => void handleDownloadSaeKit(String(selectedNode.data.saeId ?? ""))}
                      disabled={saeControlsDisabled || !selectedSaeRuntimeUrl}
                    >
                      Download mTLS Kit
                    </Button>
                    <Button
                      type="button"
                      size="sm"
                      variant="destructive"
                      onClick={() => void handleRevokeSae(String(selectedNode.data.saeId ?? ""))}
                      disabled={saeControlsDisabled || selectedNode.data.saeStatus === "revoked"}
                    >
                      Revoke SAE
                    </Button>
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      onClick={() => void handleDeleteSae(String(selectedNode.data.saeId ?? ""))}
                      disabled={saeControlsDisabled}
                    >
                      Delete SAE
                    </Button>
                  </div>
                ) : (
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    onClick={() => void handleDeleteSae(String(selectedNode.data.saeId ?? ""))}
                    disabled={saeControlsDisabled}
                  >
                    Delete SAE
                  </Button>
                )}
                {saeError ? (
                  <p className="rounded border border-red-200 bg-red-50 px-2 py-1 text-xs text-red-700">{saeError}</p>
                ) : null}
              </div>
            ) : null}
          </div>
        ) : null}

        {selectedEdge ? (
          <div className="space-y-2">
            <h3 className="font-semibold">Link: {selectedEdge.id}</h3>
            <LinkConfigEditor
              value={normalizeLinkConfig(selectedEdge)}
              disabled={readOnly}
              onChange={(next) =>
                onSelectedEdgeChange({
                  ...selectedEdge,
                  data: next
                })
              }
            />
            <Button
              type="button"
              variant="destructive"
              size="sm"
              onClick={onDeleteSelection}
              disabled={deleteSelectionDisabled}
            >
              Delete Selected Link
            </Button>
          </div>
        ) : null}

        {actionMessage ? <p className="text-xs text-muted-foreground">{actionMessage}</p> : null}
      </CardContent>

      <ConfirmDialog
        open={pendingDeleteSaeId !== null}
        onOpenChange={(open) => {
          if (!open && !saeBusy) setPendingDeleteSaeId(null);
        }}
        title={pendingDeleteSaeId ? `Eliminar SAE ${pendingDeleteSaeId}` : "Eliminar SAE"}
        description="Esta acción no se puede deshacer y el SAE se perderá definitivamente."
        confirmLabel="Eliminar"
        destructive
        loading={saeBusy}
        onConfirm={async () => {
          const target = pendingDeleteSaeId;
          if (!target) return;
          await performDeleteSae(target);
          setPendingDeleteSaeId(null);
        }}
      />
    </Card>
  );
}
