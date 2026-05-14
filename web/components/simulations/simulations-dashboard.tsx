"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { type FormEvent, useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { AlertCircle, LogOut, Play, Square, Sparkles, Trash2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle
} from "@/components/ui/card";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { ThemeToggle } from "@/components/ui/theme-toggle";
import { apiPath, withBasePath } from "@/lib/app-path";
import type { SimulationSummaryDTO } from "@/lib/topology/types";
import { cn } from "@/lib/utils";

interface Props {
  username: string;
}

type StatusVariant = "success" | "destructive" | "muted";

function statusVariant(status: SimulationSummaryDTO["status"]): StatusVariant {
  if (status === "running") return "success";
  if (status === "error") return "destructive";
  return "muted";
}

export function SimulationsDashboard({ username }: Props) {
  const router = useRouter();
  const [simulations, setSimulations] = useState<SimulationSummaryDTO[]>([]);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [creating, setCreating] = useState(false);
  const [simulationAction, setSimulationAction] = useState<Record<number, "run" | "stop" | null>>(
    {}
  );
  const [pendingDelete, setPendingDelete] = useState<SimulationSummaryDTO | null>(null);
  const [deleting, setDeleting] = useState(false);

  async function loadSimulations() {
    setLoading(true);
    setError(null);

    try {
      const response = await fetch(apiPath("/api/simulations"), { cache: "no-store" });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(payload?.error || "No se pudieron cargar las simulaciones");
      }
      setSimulations(payload.simulations ?? []);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Error inesperado");
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void loadSimulations();
  }, []);

  async function handleCreate(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setCreating(true);
    setError(null);

    try {
      const response = await fetch(apiPath("/api/simulations"), {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name, description })
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(payload?.error || "No se pudo crear la simulación");
      }
      setName("");
      setDescription("");
      await loadSimulations();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Error inesperado");
    } finally {
      setCreating(false);
    }
  }

  async function performDelete() {
    if (!pendingDelete) return;
    setDeleting(true);
    setError(null);
    try {
      const response = await fetch(apiPath(`/api/simulations/${pendingDelete.id}`), {
        method: "DELETE"
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(payload?.error || "No se pudo eliminar la simulación");
      }
      setPendingDelete(null);
      await loadSimulations();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Error inesperado");
    } finally {
      setDeleting(false);
    }
  }

  async function handleRunStop(simulation: SimulationSummaryDTO) {
    const isRunning = simulation.status === "running";
    const action: "run" | "stop" = isRunning ? "stop" : "run";
    setSimulationAction((current) => ({ ...current, [simulation.id]: action }));
    setError(null);

    try {
      const response = await fetch(
        apiPath(`/api/simulations/${simulation.id}/${isRunning ? "stop" : "run"}`),
        { method: "POST" }
      );
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(payload?.error || `No se pudo ${action === "run" ? "iniciar" : "detener"} la simulación`);
      }
      await loadSimulations();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Error inesperado");
    } finally {
      setSimulationAction((current) => ({ ...current, [simulation.id]: null }));
    }
  }

  async function handleLogout() {
    await fetch(apiPath("/api/auth/logout"), { method: "POST" });
    router.push("/login");
    router.refresh();
  }

  return (
    <>
      <main id="main-content" className="mx-auto w-full max-w-6xl space-y-6 p-4 sm:p-6 lg:p-8">
        <motion.section
          initial={{ opacity: 0, y: 12 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.3, ease: "easeOut" }}
          className="flex flex-col gap-4 rounded-xl border border-border bg-card/90 p-5 shadow-sm backdrop-blur sm:flex-row sm:items-center sm:justify-between"
        >
          <div className="flex items-center gap-3">
            <img
              src={withBasePath("/icon.png")}
              alt=""
              width={40}
              height={40}
              className="rounded-md border border-border p-1"
              aria-hidden="true"
            />
            <div>
              <h1 className="font-serif text-2xl sm:text-3xl lg:text-4xl">DKMS Simulations</h1>
              <p className="text-sm text-muted-foreground">
                Hola, <span className="font-medium text-foreground">{username}</span>. Crea y administra tus simulaciones DKMS.
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2 self-end sm:self-auto">
            <ThemeToggle />
            <Button variant="outline" onClick={handleLogout} className="touch-manipulation">
              <LogOut aria-hidden="true" />
              <span>Cerrar sesión</span>
            </Button>
          </div>
        </motion.section>

        <section className="grid gap-6 lg:grid-cols-3">
          <motion.div
            initial={{ opacity: 0, y: 16 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.3, ease: "easeOut", delay: 0.05 }}
            className="lg:col-span-1"
          >
            <Card>
              <CardHeader>
                <CardTitle className="flex items-center gap-2">
                  <Sparkles aria-hidden="true" className="size-5 text-primary" />
                  Crear simulación
                </CardTitle>
                <CardDescription>
                  Define los metadatos antes de abrir el editor visual.
                </CardDescription>
              </CardHeader>
              <CardContent>
                <form className="space-y-4" onSubmit={handleCreate} noValidate>
                  <div className="space-y-2">
                    <Label htmlFor="sim-name">Nombre</Label>
                    <Input
                      id="sim-name"
                      value={name}
                      onChange={(e) => setName(e.target.value)}
                      required
                      placeholder="mi-simulacion"
                    />
                  </div>
                  <div className="space-y-2">
                    <Label htmlFor="sim-description">Descripción</Label>
                    <Textarea
                      id="sim-description"
                      value={description}
                      onChange={(e) => setDescription(e.target.value)}
                      placeholder="Opcional — notas sobre la topología, experimento, etc."
                    />
                  </div>
                  <Button type="submit" className="w-full" loading={creating}>
                    {creating ? "Creando…" : "Crear simulación"}
                  </Button>
                </form>
                {error ? (
                  <div
                    role="alert"
                    className="mt-3 flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive"
                  >
                    <AlertCircle aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
                    <span>{error}</span>
                  </div>
                ) : null}
              </CardContent>
            </Card>
          </motion.div>

          <motion.div
            initial={{ opacity: 0, y: 16 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.3, ease: "easeOut", delay: 0.1 }}
            className="lg:col-span-2"
          >
            <Card>
              <CardHeader>
                <CardTitle>Tus simulaciones</CardTitle>
                <CardDescription>Selecciona una para abrir el editor tipo GNS3.</CardDescription>
              </CardHeader>
              <CardContent>
                {loading ? (
                  <div className="space-y-3" aria-busy="true" aria-live="polite">
                    {[0, 1, 2].map((i) => (
                      <div
                        key={i}
                        className="flex flex-col gap-3 rounded-md border border-border p-4 sm:flex-row sm:items-center sm:justify-between"
                      >
                        <div className="flex-1 space-y-2">
                          <Skeleton className="h-5 w-40" />
                          <Skeleton className="h-4 w-64" />
                          <Skeleton className="h-3 w-52" />
                        </div>
                        <div className="flex gap-2">
                          <Skeleton className="h-10 w-20" />
                          <Skeleton className="h-10 w-28" />
                          <Skeleton className="h-10 w-20" />
                        </div>
                      </div>
                    ))}
                  </div>
                ) : null}

                {!loading && simulations.length === 0 ? (
                  <div className="flex flex-col items-center gap-3 rounded-md border border-dashed border-border py-12 text-center">
                    <Sparkles aria-hidden="true" className="size-8 text-muted-foreground" />
                    <p className="text-sm font-medium text-foreground">
                      Aún no hay simulaciones
                    </p>
                    <p className="max-w-sm text-sm text-muted-foreground">
                      Usa el formulario a la izquierda para crear tu primera topología DKMS.
                    </p>
                  </div>
                ) : null}

                <AnimatePresence initial={false}>
                  <div className="space-y-3">
                    {simulations.map((simulation, idx) => {
                      const variant = statusVariant(simulation.status);
                      const actionState = simulationAction[simulation.id];
                      const isRunning = simulation.status === "running";
                      return (
                        <motion.div
                          key={simulation.id}
                          layout
                          initial={{ opacity: 0, y: 10 }}
                          animate={{ opacity: 1, y: 0 }}
                          exit={{ opacity: 0, y: -10 }}
                          transition={{ duration: 0.25, delay: idx * 0.03 }}
                          className="flex flex-col gap-3 rounded-md border border-border bg-card p-4 transition-shadow hover:shadow-md sm:flex-row sm:items-center sm:justify-between"
                        >
                          <div className="min-w-0 flex-1 space-y-1">
                            <div className="flex flex-wrap items-center gap-2">
                              <h3 className="truncate font-semibold text-foreground">
                                {simulation.name}
                              </h3>
                              <Badge variant={variant}>
                                {simulation.status.toUpperCase()}
                              </Badge>
                            </div>
                            <p className="truncate text-sm text-muted-foreground">
                              {simulation.description || "Sin descripción"}
                            </p>
                            <p className="text-xs text-muted-foreground">
                              Nodos: {simulation.nodeCount} · Enlaces: {simulation.linkCount} · SAEs:{" "}
                              {simulation.saeCount}
                            </p>
                          </div>
                          <div className="flex flex-wrap gap-2 sm:flex-nowrap">
                            <Button
                              variant={isRunning ? "secondary" : "success"}
                              onClick={() => void handleRunStop(simulation)}
                              loading={Boolean(actionState)}
                              disabled={Boolean(actionState)}
                              className="min-h-[44px] touch-manipulation"
                            >
                              {isRunning ? (
                                <Square aria-hidden="true" />
                              ) : (
                                <Play aria-hidden="true" />
                              )}
                              <span>
                                {actionState === "run"
                                  ? "Iniciando…"
                                  : actionState === "stop"
                                  ? "Deteniendo…"
                                  : isRunning
                                  ? "Detener"
                                  : "Ejecutar"}
                              </span>
                            </Button>
                            <Link
                              href={`/simulations/${simulation.id}/editor`}
                              className={cn(
                                buttonVariants({ variant: "default" }),
                                "min-h-[44px] touch-manipulation"
                              )}
                            >
                              Abrir editor
                            </Link>
                            <Link
                              href={`/simulations/${simulation.id}/tests`}
                              className={cn(
                                buttonVariants({ variant: "secondary" }),
                                "min-h-[44px] touch-manipulation"
                              )}
                            >
                              Tests
                            </Link>
                            <Button
                              variant="destructive"
                              onClick={() => setPendingDelete(simulation)}
                              className="min-h-[44px] touch-manipulation"
                              aria-label={`Eliminar simulación ${simulation.name}`}
                            >
                              <Trash2 aria-hidden="true" />
                              <span className="sr-only sm:not-sr-only">Eliminar</span>
                            </Button>
                          </div>
                        </motion.div>
                      );
                    })}
                  </div>
                </AnimatePresence>
              </CardContent>
            </Card>
          </motion.div>
        </section>
      </main>

      <ConfirmDialog
        open={Boolean(pendingDelete)}
        onOpenChange={(open) => {
          if (!open) setPendingDelete(null);
        }}
        title="Eliminar simulación"
        description={
          pendingDelete ? (
            <>
              Vas a eliminar <strong>{pendingDelete.name}</strong>. Esta acción no se puede
              deshacer y perderás la topología, SAEs y el histórico de runs asociados.
            </>
          ) : null
        }
        confirmLabel="Eliminar"
        cancelLabel="Cancelar"
        destructive
        loading={deleting}
        onConfirm={performDelete}
      />
    </>
  );
}
