"use client";

import { memo } from "react";
import { Handle, Position, type NodeProps } from "reactflow";
import { Play, Square } from "lucide-react";

import type { EditorNodeData } from "@/components/editor/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

function DkmsNodeBase({ data, selected }: NodeProps<EditorNodeData>) {
  const grafanaStatus = data.grafanaStatus ?? "disabled";
  const connectionHandleEnabled = Boolean(data.connectionHandleEnabled);
  const healthState = data.healthState ?? "unknown";
  const canOpenGrafana = Boolean(
    data.grafanaUrl && grafanaStatus === "available" && healthState === "up"
  );
  const canStopDkmsFromNode = Boolean(data.stopDkmsVisible && data.onStopDkms);
  const canStartDkmsFromNode = Boolean(data.startDkmsVisible && data.onStartDkms);
  const stopDkmsPending = Boolean(data.stopDkmsPending);
  const startDkmsPending = Boolean(data.startDkmsPending);
  const stopDisabled = stopDkmsPending || healthState === "down";
  const startDisabled = startDkmsPending || healthState === "up";

  const frameClass =
    healthState === "up"
      ? selected
        ? "border-success ring-2 ring-success/30"
        : "border-success/70"
      : healthState === "down"
      ? selected
        ? "border-warning ring-2 ring-warning/30"
        : "border-warning/70"
      : selected
      ? "border-destructive ring-2 ring-destructive/30"
      : "border-destructive/70";

  const grafanaLabel =
    grafanaStatus === "available"
      ? "Grafana esperando DKMS"
      : grafanaStatus === "checking"
      ? "Comprobando Grafana…"
      : grafanaStatus === "unavailable"
      ? "Grafana no disponible"
      : "Grafana deshabilitado";

  return (
    <div className="relative">
      <div
        className={
          "relative min-w-[220px] overflow-hidden rounded-xl border-2 px-4 py-3 text-center shadow-lg transition-shadow bg-card/95 backdrop-blur " +
          frameClass
        }
      >
        <Badge
          variant="default"
          className="pointer-events-none absolute right-2 top-2 !text-[10px] font-semibold uppercase tracking-wider"
        >
          DKMS
        </Badge>
        <div className="text-base font-semibold text-card-foreground">{data.label}</div>
        <div className="mt-1 text-xs font-medium text-muted-foreground">
          node_id: {data.nodeId ?? "-"}
        </div>

        {canOpenGrafana ? (
          <a
            href={data.grafanaUrl ?? undefined}
            target="_blank"
            rel="noreferrer"
            className="mt-2 inline-block rounded-md border border-success/30 bg-success/10 px-2 py-1 text-xs font-semibold text-success transition-colors hover:bg-success/20 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
            onClick={(event) => event.stopPropagation()}
          >
            Abrir Grafana
          </a>
        ) : (
          <span className="mt-2 inline-block rounded-md border border-border bg-muted px-2 py-1 text-xs font-medium text-muted-foreground">
            {grafanaLabel}
          </span>
        )}

        {canStartDkmsFromNode || canStopDkmsFromNode ? (
          <div className="mt-2 grid grid-cols-2 gap-1">
            <Button
              type="button"
              variant="success"
              size="sm"
              className="h-8 px-2 text-xs"
              disabled={startDisabled}
              loading={startDkmsPending}
              onClick={(event) => {
                event.stopPropagation();
                data.onStartDkms?.();
              }}
            >
              {startDkmsPending ? null : <Play aria-hidden="true" className="!size-3" />}
              {startDkmsPending ? "Iniciando…" : "Iniciar"}
            </Button>
            <Button
              type="button"
              variant="destructive"
              size="sm"
              className="h-8 px-2 text-xs"
              disabled={stopDisabled}
              loading={stopDkmsPending}
              onClick={(event) => {
                event.stopPropagation();
                data.onStopDkms?.();
              }}
            >
              {stopDkmsPending ? null : <Square aria-hidden="true" className="!size-3" />}
              {stopDkmsPending ? "Deteniendo…" : "Detener"}
            </Button>
          </div>
        ) : null}
      </div>

      <Handle
        type="target"
        position={Position.Top}
        id="dkms-target"
        className="!size-3.5 !rounded-full"
        style={{
          left: "50%",
          top: "50%",
          transform: "translate(-50%, -50%)",
          border: "2px solid hsl(var(--primary))",
          background: "hsl(var(--primary) / 0.25)",
          opacity: connectionHandleEnabled ? (selected ? 1 : 0.55) : 0,
          pointerEvents: connectionHandleEnabled ? "auto" : "none"
        }}
      />
      <Handle
        type="source"
        position={Position.Bottom}
        id="dkms-source"
        className="!size-3.5 !rounded-full"
        style={{
          left: "50%",
          top: "50%",
          transform: "translate(-50%, -50%)",
          border: "2px solid hsl(var(--primary))",
          background: "hsl(var(--primary))",
          opacity: connectionHandleEnabled ? (selected ? 1 : 0.55) : 0,
          pointerEvents: connectionHandleEnabled ? "auto" : "none"
        }}
      />
    </div>
  );
}

export const DkmsNode = memo(DkmsNodeBase);
