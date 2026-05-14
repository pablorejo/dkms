"use client";

import * as React from "react";
import Link from "next/link";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { apiPath } from "@/lib/app-path";
import type { LoadTestDTO } from "@/api/simulations";

interface Props {
  simulationId: number;
}

interface FormState {
  startSaes: number;
  endSaes: number;
  stepSaes: number;
  intervalSeconds: number;
  offsetMultiplier: number;
  warmupSeconds: number;
  keySizeBits: number;
  perSaeLambda: number;
}

const DEFAULTS: FormState = {
  startSaes: 5,
  endSaes: 40,
  stepSaes: 5,
  intervalSeconds: 15,
  offsetMultiplier: 2,
  warmupSeconds: 30,
  keySizeBits: 256,
  perSaeLambda: 0.5
};

function numInput(
  id: keyof FormState,
  label: string,
  state: FormState,
  setState: React.Dispatch<React.SetStateAction<FormState>>,
  step = "1",
  min = "0"
) {
  return (
    <div className="flex flex-col gap-1">
      <Label htmlFor={id}>{label}</Label>
      <Input
        id={id}
        type="number"
        step={step}
        min={min}
        value={String(state[id] ?? "")}
        onChange={(e) =>
          setState((s) => ({ ...s, [id]: Number(e.target.value) }))
        }
      />
    </div>
  );
}

export function SimulationTestsPanel({ simulationId }: Props) {
  const [form, setForm] = React.useState<FormState>(DEFAULTS);
  const [tests, setTests] = React.useState<LoadTestDTO[]>([]);
  const [loading, setLoading] = React.useState(false);
  const [submitting, setSubmitting] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  const refresh = React.useCallback(async () => {
    setLoading(true);
    try {
      const res = await fetch(apiPath(`/simulations/${simulationId}/tests`), {
        cache: "no-store"
      });
      const data = await res.json();
      if (!res.ok) throw new Error(data?.error || "Failed to list tests");
      setTests((data?.tests || []) as LoadTestDTO[]);
      setError(null);
    } catch (e: unknown) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  }, [simulationId]);

  React.useEffect(() => {
    void refresh();
    const h = window.setInterval(() => void refresh(), 5000);
    return () => window.clearInterval(h);
  }, [refresh]);

  async function onSubmit(ev: React.FormEvent) {
    ev.preventDefault();
    if (form.endSaes < form.startSaes) {
      setError("end_saes must be >= start_saes");
      return;
    }
    setSubmitting(true);
    try {
      const offsetSeconds = form.offsetMultiplier * form.intervalSeconds;
      const res = await fetch(apiPath(`/simulations/${simulationId}/tests`), {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          startSaes: form.startSaes,
          endSaes: form.endSaes,
          stepSaes: form.stepSaes,
          intervalSeconds: form.intervalSeconds,
          offsetSeconds,
          warmupSeconds: form.warmupSeconds,
          keySizeBits: form.keySizeBits,
          perSaeLambda: form.perSaeLambda
        })
      });
      const data = await res.json();
      if (!res.ok) throw new Error(data?.error || "Failed to start test");
      setError(null);
      void refresh();
    } catch (e: unknown) {
      setError((e as Error).message);
    } finally {
      setSubmitting(false);
    }
  }

  async function onStop(testId: string) {
    if (!window.confirm(`Stop test ${testId}?`)) return;
    try {
      const res = await fetch(
        apiPath(`/simulations/${simulationId}/tests/${encodeURIComponent(testId)}`),
        { method: "DELETE" }
      );
      if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        throw new Error((data as { error?: string })?.error || "Failed to stop");
      }
      void refresh();
    } catch (e: unknown) {
      setError((e as Error).message);
    }
  }

  return (
    <div className="mx-auto flex max-w-6xl flex-col gap-6 p-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Load Tests — simulation #{simulationId}</h1>
          <p className="text-sm text-muted-foreground">
            Ramped ETSI enc_keys load generator. Each test runs inside the simulation namespace
            and exposes its own Grafana dashboard.
          </p>
        </div>
        <Link href={`/simulations/${simulationId}/editor`}>
          <Button variant="outline">Back to editor</Button>
        </Link>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>New ramp</CardTitle>
        </CardHeader>
        <CardContent>
          <form className="grid grid-cols-1 gap-4 md:grid-cols-4" onSubmit={onSubmit}>
            {numInput("startSaes", "Start SAEs (X)", form, setForm)}
            {numInput("endSaes", "End SAEs (Y)", form, setForm)}
            {numInput("stepSaes", "Step (S)", form, setForm)}
            {numInput("intervalSeconds", "Interval s (T)", form, setForm, "0.5")}
            {numInput("offsetMultiplier", "Hold = offset×T", form, setForm, "1")}
            {numInput("warmupSeconds", "Warmup s", form, setForm, "0.5")}
            {numInput("keySizeBits", "Key bits", form, setForm, "64", "64")}
            {numInput("perSaeLambda", "λ req/s/SAE", form, setForm, "0.05", "0.05")}
            <div className="md:col-span-4 flex items-center justify-between">
              <p className="text-xs text-muted-foreground">
                Total hold ≈ {Math.max(0, form.offsetMultiplier * form.intervalSeconds)}s after
                ramp reaches {form.endSaes} SAEs. You can stop a test at any time.
              </p>
              <Button type="submit" disabled={submitting}>
                {submitting ? "Launching…" : "Launch ramp"}
              </Button>
            </div>
          </form>
        </CardContent>
      </Card>

      {error ? (
        <div className="rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
          {error}
        </div>
      ) : null}

      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle>Running / recent tests</CardTitle>
          <Button variant="outline" size="sm" onClick={() => void refresh()} disabled={loading}>
            {loading ? "Refreshing…" : "Refresh"}
          </Button>
        </CardHeader>
        <CardContent>
          {tests.length === 0 ? (
            <p className="text-sm text-muted-foreground">No active or recent tests.</p>
          ) : (
            <div className="flex flex-col gap-3">
              {tests.map((t) => (
                <div
                  key={t.testId}
                  className="flex flex-col gap-2 rounded-md border border-border p-3 md:flex-row md:items-center md:justify-between"
                >
                  <div className="flex flex-col">
                    <span className="font-mono text-sm">{t.testId}</span>
                    <span className="text-xs text-muted-foreground">
                      deploy={t.deploymentName} · replicas={t.readyReplicas}/{t.replicas}
                      {t.createdAt ? ` · created ${new Date(t.createdAt).toLocaleString()}` : ""}
                    </span>
                  </div>
                  <div className="flex items-center gap-2">
                    <Badge variant={t.readyReplicas > 0 ? "default" : "secondary"}>
                      {t.readyReplicas > 0 ? "running" : "starting"}
                    </Badge>
                    <a href={t.grafanaUrl} target="_blank" rel="noreferrer">
                      <Button variant="outline" size="sm">
                        Open Grafana
                      </Button>
                    </a>
                    <Button variant="destructive" size="sm" onClick={() => void onStop(t.testId)}>
                      Stop
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
