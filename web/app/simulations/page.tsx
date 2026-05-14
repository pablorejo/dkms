import { SimulationsDashboard } from "@/components/simulations/simulations-dashboard";
import { requireUser } from "@/lib/auth/session";

export default async function SimulationsPage() {
  const user = await requireUser();

  return <SimulationsDashboard username={user.username} />;
}
