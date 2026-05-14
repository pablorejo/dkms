import * as React from "react";
import { cn } from "@/lib/utils";

function Skeleton({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      role="presentation"
      aria-hidden="true"
      className={cn(
        "skeleton-shimmer rounded-md motion-reduce:animate-none",
        className
      )}
      {...props}
    />
  );
}

export { Skeleton };
