"use client";

import * as React from "react";
import { useTheme } from "next-themes";
import { Moon, Sun, Monitor } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export function ThemeToggle({ className }: { className?: string }) {
  const { theme, setTheme, resolvedTheme } = useTheme();
  const [mounted, setMounted] = React.useState(false);

  React.useEffect(() => {
    setMounted(true);
  }, []);

  const current = theme ?? "system";
  const isDark = mounted ? resolvedTheme === "dark" : false;

  const cycle = () => {
    const order: Array<"light" | "dark" | "system"> = ["light", "dark", "system"];
    const idx = order.indexOf(current as "light" | "dark" | "system");
    const next = order[(idx + 1) % order.length];
    setTheme(next);
  };

  const label =
    current === "system"
      ? "Tema: sistema"
      : isDark
      ? "Tema: oscuro"
      : "Tema: claro";

  const Icon = current === "system" ? Monitor : isDark ? Moon : Sun;

  return (
    <Button
      type="button"
      variant="ghost"
      size="icon"
      className={cn("touch-target", className)}
      onClick={cycle}
      aria-label={label}
      title={label}
    >
      <Icon aria-hidden="true" />
    </Button>
  );
}
