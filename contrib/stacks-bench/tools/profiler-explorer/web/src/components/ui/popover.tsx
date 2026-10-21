import * as React from "react";
import { Popover as PopoverPrimitive } from "@base-ui/react";
import { cn } from "@/lib/utils.ts";

const Popover = PopoverPrimitive.Root;

function PopoverTrigger({ asChild, children, ...props }: React.ComponentProps<typeof PopoverPrimitive.Trigger> & { asChild?: boolean }) {
  if (asChild) {
    return <PopoverPrimitive.Trigger render={React.Children.only(children) as React.ReactElement} {...props} />;
  }
  return <PopoverPrimitive.Trigger {...props}>{children}</PopoverPrimitive.Trigger>;
}

function PopoverContent({ className, align = "center", side, sideOffset = 4, ...props }: React.ComponentProps<typeof PopoverPrimitive.Popup> & { align?: string; side?: "top" | "right" | "bottom" | "left"; sideOffset?: number; className?: string }) {
  return (
    <PopoverPrimitive.Portal>
      <PopoverPrimitive.Positioner align={align as "start" | "center" | "end"} side={side} sideOffset={sideOffset} className="z-[200]">
        <PopoverPrimitive.Popup
          className={cn(
            "z-[200] w-72 rounded-md border border-border bg-popover p-0 text-popover-foreground shadow-md outline-none",
            "data-open:animate-in data-closed:animate-out data-closed:fade-out-0 data-open:fade-in-0",
            "data-closed:zoom-out-95 data-open:zoom-in-95",
            className
          )}
          {...props}
        />
      </PopoverPrimitive.Positioner>
    </PopoverPrimitive.Portal>
  );
}

export { Popover, PopoverTrigger, PopoverContent };
