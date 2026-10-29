import * as React from "react";
import { cn } from "@/lib/utils";

export function Input({ className, ...props }: React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        // Material 3 outlined text field at its densest (40dp), body large (16/24) input text.
        "flex h-10 w-full rounded-xs border border-input bg-card px-3 text-base placeholder:text-muted-foreground hover:border-foreground/60 focus-visible:border-primary disabled:cursor-not-allowed disabled:opacity-38 aria-invalid:border-destructive",
        className,
      )}
      {...props}
    />
  );
}
