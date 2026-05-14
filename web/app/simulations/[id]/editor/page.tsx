import { headers } from "next/headers";
import { notFound } from "next/navigation";
import { SimulationEditor } from "@/components/editor/simulation-editor";
import { requireUser } from "@/lib/auth/session";

interface Props {
  params: Promise<{ id: string }>;
}

export default async function SimulationEditorPage({ params }: Props) {
  await requireUser();
  const resolved = await params;
  const simulationId = Number.parseInt(resolved.id, 10);
  const reqHeaders = await headers();
  const host = reqHeaders.get("x-forwarded-host") ?? reqHeaders.get("host") ?? "";
  const proto = (reqHeaders.get("x-forwarded-proto") ?? "http").split(",")[0]?.trim() || "http";
  const ingressBaseUrl = host ? `${proto}://${host}` : "";

  if (!Number.isFinite(simulationId) || simulationId <= 0) {
    notFound();
  }

  return <SimulationEditor simulationId={simulationId} ingressBaseUrl={ingressBaseUrl} />;
}
