import * as React from "react"
import { Tooltip as TooltipPrimitive } from "@base-ui/react"

import { cn } from "@/lib/utils.ts"

function TooltipProvider({ delayDuration, children, ...props }: React.ComponentProps<typeof TooltipPrimitive.Provider> & { delayDuration?: number }) {
  return (
    <TooltipPrimitive.Provider delay={delayDuration} {...props}>
      {children}
    </TooltipPrimitive.Provider>
  );
}

const Tooltip = TooltipPrimitive.Root

function TooltipTrigger({ asChild, children, ...props }: React.ComponentProps<typeof TooltipPrimitive.Trigger> & { asChild?: boolean }) {
  if (asChild) {
    return <TooltipPrimitive.Trigger render={React.Children.only(children) as React.ReactElement} {...props} />;
  }
  return <TooltipPrimitive.Trigger {...props}>{children}</TooltipPrimitive.Trigger>;
}

function TooltipContent({ className, sideOffset = 4, side, ...props }: React.ComponentProps<typeof TooltipPrimitive.Popup> & { sideOffset?: number; side?: "top" | "right" | "bottom" | "left"; className?: string }) {
  return (
    <TooltipPrimitive.Portal>
      <TooltipPrimitive.Positioner sideOffset={sideOffset} side={side}>
        <TooltipPrimitive.Popup
          className={cn(
            "z-50 overflow-hidden rounded-md bg-primary px-3 py-1.5 text-xs text-primary-foreground animate-in fade-in-0 zoom-in-95 data-closed:animate-out data-closed:fade-out-0 data-closed:zoom-out-95 origin-[--transform-origin]",
            className
          )}
          {...props}
        />
      </TooltipPrimitive.Positioner>
    </TooltipPrimitive.Portal>
  );
}

export { Tooltip, TooltipTrigger, TooltipContent, TooltipProvider }
