import { redirect } from "next/navigation";
import { validateRequest } from "@/lib/auth/session";

export default async function HomePage() {
  const { user } = await validateRequest();
  if (user) {
    redirect("/simulations");
  }
  redirect("/login");
}
