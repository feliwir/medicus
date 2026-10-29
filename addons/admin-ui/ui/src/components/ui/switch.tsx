import { cn } from "@/lib/utils";

/** An on/off toggle (a button with the `switch` role), sized as a Material 3 switch: a 52×32
 * track; the handle grows from 16 to 24 when on. */
export function Switch({
  checked,
  onCheckedChange,
  id,
  disabled,
  "aria-label": ariaLabel,
}: {
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  id?: string;
  disabled?: boolean;
  "aria-label"?: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      id={id}
      aria-checked={checked}
      aria-label={ariaLabel}
      disabled={disabled}
      onClick={() => onCheckedChange(!checked)}
      className={cn(
        // The track is 52×32; the target extends to 48dp around it.
        "relative inline-flex h-8 w-13 shrink-0 items-center rounded-full border-2 transition-colors before:absolute before:-inset-x-0.5 before:-inset-y-2 disabled:opacity-38",
        checked ? "border-primary bg-primary" : "border-muted-foreground bg-muted",
      )}
    >
      <span
        className={cn(
          "rounded-full transition-all",
          checked ? "ml-[20px] size-6 bg-primary-foreground" : "ml-[6px] size-4 bg-muted-foreground",
        )}
      />
    </button>
  );
}
