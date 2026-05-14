"use client";

import { Eye, Info, Link2, Plus, Trash2, WandSparkles } from "lucide-react";

import { Button } from "@/components/ui/button";
import type { LinkType } from "@/lib/topology/types";

interface Props {
  onAddDkms: () => void;
  addDkmsDisabled?: boolean;
  connectionModeEnabled: boolean;
  onConnectionModeChange: (enabled: boolean) => void;
  connectionLinkType: LinkType;
  onConnectionLinkTypeChange: (linkType: LinkType) => void;
  saeOverlayEnabled: boolean;
  onSaeOverlayToggle: () => void;
  onDeleteSelection: () => void;
  onFitView: () => void;
  readOnly?: boolean;
}

export function EditorToolbox({
  onAddDkms,
  addDkmsDisabled = false,
  connectionModeEnabled,
  onConnectionModeChange,
  connectionLinkType,
  onConnectionLinkTypeChange,
  saeOverlayEnabled,
  onSaeOverlayToggle,
  onDeleteSelection,
  onFitView,
  readOnly = false
}: Props) {
  return (
    <div className="rounded-xl border border-border bg-card/95 p-3 shadow-sm backdrop-blur">
      <div className="flex flex-wrap items-center gap-2">
        <Button
          variant="secondary"
          size="sm"
          onClick={onAddDkms}
          disabled={readOnly || addDkmsDisabled}
        >
          <Plus aria-hidden="true" /> Añadir DKMS
        </Button>
        <Button
          variant={connectionModeEnabled ? "default" : "outline"}
          size="sm"
          onClick={() => onConnectionModeChange(!connectionModeEnabled)}
          disabled={readOnly}
          aria-pressed={connectionModeEnabled}
        >
          <Link2 aria-hidden="true" />
          {connectionModeEnabled ? "Modo enlace ON" : "Modo enlace OFF"}
        </Button>
        <Button variant="outline" size="sm" onClick={onFitView}>
          <WandSparkles aria-hidden="true" /> Ajustar vista
        </Button>
        <Button
          variant={saeOverlayEnabled ? "default" : "outline"}
          size="sm"
          onClick={onSaeOverlayToggle}
          disabled={readOnly}
          aria-pressed={saeOverlayEnabled}
        >
          <Eye aria-hidden="true" />
          {saeOverlayEnabled ? "Overlay SAE ON" : "Mostrar SAEs"}
        </Button>
        <Button variant="destructive" size="sm" onClick={onDeleteSelection} disabled={readOnly}>
          <Trash2 aria-hidden="true" /> Eliminar selección
        </Button>
      </div>

      {connectionModeEnabled ? (
        <div
          className="mt-3 flex flex-wrap items-center gap-2 rounded-lg border border-border bg-muted/60 p-2"
          role="group"
          aria-label="Tipo del nuevo enlace"
        >
          <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
            Tipo de enlace nuevo
          </span>
          <Button
            type="button"
            size="sm"
            variant={connectionLinkType === "QKD" ? "default" : "outline"}
            onClick={() => onConnectionLinkTypeChange("QKD")}
            disabled={readOnly}
            aria-pressed={connectionLinkType === "QKD"}
          >
            QKD
          </Button>
          <Button
            type="button"
            size="sm"
            variant={connectionLinkType === "PQC" ? "default" : "outline"}
            onClick={() => onConnectionLinkTypeChange("PQC")}
            disabled={readOnly}
            aria-pressed={connectionLinkType === "PQC"}
          >
            PQC
          </Button>
          <Button
            type="button"
            size="sm"
            variant={connectionLinkType === "HYBRID" ? "default" : "outline"}
            onClick={() => onConnectionLinkTypeChange("HYBRID")}
            disabled={readOnly}
            aria-pressed={connectionLinkType === "HYBRID"}
          >
            HYBRID
          </Button>
        </div>
      ) : null}

      <div className="mt-3 flex flex-wrap items-center gap-2 text-xs">
        <span className="inline-flex items-center gap-1 rounded-full border border-border bg-muted/60 px-2 py-0.5 text-muted-foreground">
          <Info aria-hidden="true" className="size-3.5" />
          {readOnly
            ? "Transición de infraestructura en curso: acciones bloqueadas."
            : connectionModeEnabled
            ? "Arrastra del centro de un DKMS a otro para crear un enlace."
            : "Activa Modo enlace para dibujar conexiones."}
        </span>
        <span className="rounded-full border border-border bg-muted/60 px-2 py-0.5 text-muted-foreground">
          Alt + rueda del ratón: zoom
        </span>
        <span className="hidden rounded-full border border-border bg-muted/60 px-2 py-0.5 text-muted-foreground sm:inline-flex">
          Rueda del ratón: scroll
        </span>
      </div>
    </div>
  );
}
