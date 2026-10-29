import * as React from "react";
import { cn } from "@/lib/utils";

/** A checkbox with its label; the whole row is the target (Material 3: 40dp). */
export function Checkbox({
  label,
  className,
  ...props
}: Omit<React.InputHTMLAttributes<HTMLInputElement>, "type"> & { label: React.ReactNode }) {
  return (
    <label className={cn("flex min-h-10 cursor-pointer items-center gap-3 text-sm", className)}>
      <input type="checkbox" className="size-[18px] accent-primary" {...props} />
      <span>{label}</span>
    </label>
  );
}
