import { redirect } from "next/navigation";
import { RegisterForm } from "@/components/auth/register-form";
import { validateRequest } from "@/lib/auth/session";

export default async function RegisterPage() {
  const { user } = await validateRequest();
  if (user) {
    redirect("/simulations");
  }

  return (
    <main id="main-content" className="grid min-h-screen place-items-center p-4 sm:p-6">
      <RegisterForm />
    </main>
  );
}
