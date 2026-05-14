import { redirect } from "next/navigation";
import { LoginForm } from "@/components/auth/login-form";
import { validateRequest } from "@/lib/auth/session";

export default async function LoginPage() {
  const { user } = await validateRequest();
  if (user) {
    redirect("/simulations");
  }

  return (
    <main id="main-content" className="grid min-h-screen place-items-center p-4 sm:p-6">
      <LoginForm />
    </main>
  );
}
