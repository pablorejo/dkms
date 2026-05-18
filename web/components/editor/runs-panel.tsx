"use client";

import { useEffect, useState } from "react";
import { AlertCircle } from "lucide-react";

import { Badge, type BadgeProps } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { apiPath } from "@/lib/app-path";
import type { SimulationRunDTO } from "@/lib/topology/types";

function statusVariant(status: SimulationRunDTO["status"]): BadgeProps["variant"] {
  if (status === "DONE") return "success";
  if (status === "FAILED") return "destructive";
  if (status === "RUNNING") return "warning";
  return "default";
}

interface Props {
  simulationId: number;
  refreshToken: number;
  paused?: boolean;
}

export function RunsPanel({ simulationId, refreshToken, paused = false }: Props) {
  const [runs, setRuns] = useState<SimulationRunDTO[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  async function loadRuns() {
    try {
      setError(null);
      const response = await fetch(apiPath(`/api/simulations/${simulationId}/runs`), {
        cache: "no-store"
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        setError(payload?.error || "No se pudo cargar el historial de runs");
        return;
      }
      setRuns(payload.runs ?? []);
    } catch {
      setError("No se pudo obtener el historial de runs");
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void loadRuns();
    if (paused) {
      return;
    }
    const timer = window.setInterval(() => {
      void loadRuns();
    }, 3000);
    return () => window.clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [simulationId, refreshToken, paused]);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Runs</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3 text-sm">
        {loading ? (
          <div className="space-y-2" aria-busy="true" aria-live="polite">
            <Skeleton className="h-14 w-full" />
            <Skeleton className="h-14 w-full" />
          </div>
        ) : null}
        {error ? (
          <div
            role="alert"
            className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive"
          >
            <AlertCircle aria-hidden="true" className="mt-0.5 size-3.5 shrink-0" />
            <span>{error}</span>
          </div>
        ) : null}
        {!loading && runs.length === 0 ? (
          <p className="text-muted-foreground">Todavía no hay runs.</p>
        ) : null}
        {runs.map((run) => (
          <div
            key={run.id}
            className="rounded-md border border-border bg-card p-3 transition-shadow hover:shadow-sm"
          >
            <div className="mb-1 flex items-center justify-between gap-2">
              <span className="font-medium text-foreground">Run #{run.id}</span>
              <Badge variant={statusVariant(run.status)}>{run.status}</Badge>
            </div>
            <p className="text-xs text-muted-foreground">
              Creado: {new Date(run.createdAt).toLocaleString()}
            </p>
            {run.message ? <p className="mt-1 text-xs text-foreground">{run.message}</p> : null}
          </div>
        ))}
      </CardContent>
    </Card>
  );
}
