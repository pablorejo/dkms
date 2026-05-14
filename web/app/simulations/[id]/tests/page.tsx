import { notFound } from "next/navigation";
import { SimulationTestsPanel } from "@/components/simulations/tests-panel";
import { requireUser } from "@/lib/auth/session";

interface Props {
  params: Promise<{ id: string }>;
}

export default async function SimulationTestsPage({ params }: Props) {
  await requireUser();
  const resolved = await params;
  const simulationId = Number.parseInt(resolved.id, 10);
  if (!Number.isFinite(simulationId) || simulationId <= 0) {
    notFound();
  }
  return <SimulationTestsPanel simulationId={simulationId} />;
}
