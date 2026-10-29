import * as React from "react";
import { cn } from "@/lib/utils";

export function Textarea({ className, ...props }: React.TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return (
    <textarea
      className={cn(
        "flex min-h-24 w-full rounded-xs border border-input bg-card px-3 py-2 text-sm placeholder:text-muted-foreground hover:border-foreground/60 focus-visible:border-primary disabled:opacity-38 aria-invalid:border-destructive",
        className,
      )}
      {...props}
    />
  );
}
