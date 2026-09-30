import * as React from "react"
import { PreviewCard as HoverCardPrimitive } from "@base-ui/react"

import { cn } from "@/lib/utils.ts"

interface HoverCardDelayValue {
  delay?: number;
  closeDelay?: number;
}

const HoverCardDelayContext = React.createContext<HoverCardDelayValue>({});

function HoverCard({ openDelay, closeDelay, ...props }: React.ComponentProps<typeof HoverCardPrimitive.Root> & { openDelay?: number; closeDelay?: number }) {
  const delayValue = React.useMemo(
    () => ({ delay: openDelay, closeDelay }),
    [openDelay, closeDelay]
  );
  return (
    <HoverCardDelayContext.Provider value={delayValue}>
      <HoverCardPrimitive.Root {...props} />
    </HoverCardDelayContext.Provider>
  );
}

function HoverCardTrigger({ asChild, children, delay: delayProp, closeDelay: closeDelayProp, ...props }: React.ComponentProps<typeof HoverCardPrimitive.Trigger> & { asChild?: boolean; delay?: number; closeDelay?: number }) {
  const ctx = React.useContext(HoverCardDelayContext);
  const delay = delayProp ?? ctx.delay;
  const closeDelay = closeDelayProp ?? ctx.closeDelay;
  const triggerProps: Record<string, unknown> = { ...props };
  if (delay != null) triggerProps.delay = delay;
  if (closeDelay != null) triggerProps.closeDelay = closeDelay;

  if (asChild) {
    return <HoverCardPrimitive.Trigger render={React.Children.only(children) as React.ReactElement} {...(triggerProps as React.ComponentProps<typeof HoverCardPrimitive.Trigger>)} />;
  }
  return <HoverCardPrimitive.Trigger {...(triggerProps as React.ComponentProps<typeof HoverCardPrimitive.Trigger>)}>{children}</HoverCardPrimitive.Trigger>;
}

function HoverCardContent({ className, align = "center", side, sideOffset = 4, style, ...props }: React.ComponentProps<typeof HoverCardPrimitive.Popup> & { align?: string; side?: "top" | "right" | "bottom" | "left"; sideOffset?: number; className?: string; style?: React.CSSProperties }) {
  return (
    <HoverCardPrimitive.Portal>
      <HoverCardPrimitive.Positioner align={align as "start" | "center" | "end"} side={side} sideOffset={sideOffset} style={{ zIndex: 200 }}>
        <HoverCardPrimitive.Popup
          className={cn(
            "w-64 rounded-[10px] border border-border p-4 text-popover-foreground shadow-md outline-none data-open:animate-in data-closed:animate-out data-closed:fade-out-0 data-open:fade-in-0 data-closed:zoom-out-95 data-open:zoom-in-95",
            className
          )}
          style={{ backgroundColor: 'var(--popover)', ...style }}
          {...props}
        />
      </HoverCardPrimitive.Positioner>
    </HoverCardPrimitive.Portal>
  )
}

export { HoverCard, HoverCardTrigger, HoverCardContent }
