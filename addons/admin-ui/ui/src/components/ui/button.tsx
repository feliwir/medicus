import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";
import * as React from "react";
import { cn } from "@/lib/utils";

const buttonVariants = cva(
  // Material 3 common buttons: 40dp high, fully rounded, label large (14/20, medium); disabled
  // buttons use 12% of the text colour as container and 38% as content.
  "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-full text-sm font-medium transition-colors disabled:pointer-events-none [&_svg]:size-[18px] [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        default:
          "bg-primary text-primary-foreground hover:bg-primary/90 disabled:bg-foreground/12 disabled:text-foreground/38",
        destructive:
          "bg-destructive text-destructive-foreground hover:bg-destructive/90 disabled:bg-foreground/12 disabled:text-foreground/38",
        outline:
          "border border-input text-primary hover:bg-primary/8 disabled:border-foreground/12 disabled:text-foreground/38",
        secondary: "bg-accent text-accent-foreground hover:bg-accent/80 disabled:bg-foreground/12 disabled:text-foreground/38",
        ghost: "text-primary hover:bg-primary/8 disabled:text-foreground/38",
        link: "text-primary underline-offset-4 hover:underline disabled:text-foreground/38",
      },
      size: {
        default: "h-10 px-6",
        sm: "h-9 px-4",
        icon: "size-10 text-muted-foreground hover:text-foreground",
      },
    },
    defaultVariants: { variant: "default", size: "default" },
  },
);

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean;
}

export function Button({ className, variant, size, asChild = false, ...props }: ButtonProps) {
  const Comp = asChild ? Slot : "button";
  return <Comp className={cn(buttonVariants({ variant, size, className }))} {...props} />;
}
