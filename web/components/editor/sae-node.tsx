"use client";

import { memo } from "react";
import type { NodeProps } from "reactflow";

import type { EditorNodeData } from "@/components/editor/types";
import { Badge } from "@/components/ui/badge";

type SaeStatus = "active" | "revoked" | "expired" | string | undefined;

function statusVariant(
  status: SaeStatus
): "success" | "destructive" | "warning" | "muted" {
  if (status === "active") return "success";
  if (status === "revoked") return "destructive";
  if (status === "expired") return "warning";
  return "muted";
}

function SaeNodeBase({ data, selected }: NodeProps<EditorNodeData>) {
  const saeId = data.saeId ?? "-";
  return (
    <div
      className={
        "min-w-[150px] rounded-lg border bg-card/95 px-3 py-2 text-xs text-card-foreground shadow-sm transition-shadow backdrop-blur " +
        (selected ? "border-primary ring-2 ring-primary/30" : "border-border")
      }
    >
      <div className="mb-1 flex items-center justify-between gap-2">
        <span className="font-semibold text-foreground">SAE {saeId}</span>
        <Badge variant={statusVariant(data.saeStatus)} className="!text-[10px]">
          {data.saeStatus ?? "pending_cert"}
        </Badge>
      </div>
      <p className="truncate text-xs text-muted-foreground">
        {data.saeDisplayName || data.label}
      </p>
      <p className="mt-1 text-[10px] text-muted-foreground/80">
        DKMS {data.parentDkmsId ?? "-"}
      </p>
    </div>
  );
}

export const SaeNode = memo(SaeNodeBase);
