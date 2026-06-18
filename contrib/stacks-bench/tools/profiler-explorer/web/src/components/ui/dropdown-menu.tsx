"use client"

import * as React from "react"
import { Menu as MenuPrimitive } from "@base-ui/react"
import { Check, ChevronRight, Circle } from "lucide-react"

import { cn } from "@/lib/utils.ts"

const DropdownMenu = MenuPrimitive.Root

function DropdownMenuTrigger({ asChild, children, ...props }: React.ComponentProps<typeof MenuPrimitive.Trigger> & { asChild?: boolean }) {
  if (asChild) {
    return <MenuPrimitive.Trigger render={React.Children.only(children) as React.ReactElement} {...props} />;
  }
  return <MenuPrimitive.Trigger {...props}>{children}</MenuPrimitive.Trigger>;
}

const DropdownMenuGroup = MenuPrimitive.Group

const DropdownMenuPortal = MenuPrimitive.Portal

const DropdownMenuSub = MenuPrimitive.SubmenuRoot

const DropdownMenuRadioGroup = MenuPrimitive.RadioGroup

function DropdownMenuSubTrigger({ className, inset, children, ...props }: React.ComponentProps<typeof MenuPrimitive.SubmenuTrigger> & { inset?: boolean; className?: string }) {
  return (
    <MenuPrimitive.SubmenuTrigger
      className={cn(
        "flex cursor-default select-none items-center gap-2 rounded-sm px-2 py-1.5 text-sm outline-none focus:bg-accent data-open:bg-accent [&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0",
        inset && "pl-8",
        className
      )}
      {...props}
    >
      {children}
      <ChevronRight className="ml-auto" />
    </MenuPrimitive.SubmenuTrigger>
  );
}

function DropdownMenuSubContent({ className, ...props }: React.ComponentProps<typeof MenuPrimitive.Popup> & { className?: string }) {
  return (
    <MenuPrimitive.Portal>
      <MenuPrimitive.Positioner style={{ zIndex: 200 }}>
        <MenuPrimitive.Popup
          className={cn(
            "min-w-[8rem] overflow-hidden rounded-md border bg-popover p-1 text-popover-foreground shadow-lg data-open:animate-in data-closed:animate-out data-closed:fade-out-0 data-open:fade-in-0 data-closed:zoom-out-95 data-open:zoom-in-95 origin-[--transform-origin]",
            className
          )}
          {...props}
        />
      </MenuPrimitive.Positioner>
    </MenuPrimitive.Portal>
  );
}

function DropdownMenuContent({ className, sideOffset = 4, align, side, ...props }: React.ComponentProps<typeof MenuPrimitive.Popup> & { sideOffset?: number; align?: string; side?: "top" | "right" | "bottom" | "left"; className?: string }) {
  return (
    <MenuPrimitive.Portal>
      <MenuPrimitive.Positioner sideOffset={sideOffset} align={align as "start" | "center" | "end"} side={side} style={{ zIndex: 200 }}>
        <MenuPrimitive.Popup
          className={cn(
            "max-h-[var(--available-height)] min-w-[8rem] overflow-y-auto overflow-x-hidden rounded-[10px] border text-popover-foreground dropdown-popover-enhanced",
            "data-open:animate-in data-closed:animate-out data-closed:fade-out-0 data-open:fade-in-0 data-closed:zoom-out-95 data-open:zoom-in-95 origin-[--transform-origin]",
            className
          )}
          {...props}
        />
      </MenuPrimitive.Positioner>
    </MenuPrimitive.Portal>
  );
}

function DropdownMenuItem({ className, inset, ...props }: React.ComponentProps<typeof MenuPrimitive.Item> & { inset?: boolean; className?: string }) {
  return (
    <MenuPrimitive.Item
      className={cn(
        "relative flex cursor-default select-none items-center gap-2 rounded-sm px-2 py-1.5 text-sm outline-none transition-colors focus:bg-accent focus:text-accent-foreground data-[disabled]:pointer-events-none data-[disabled]:opacity-50 [&>svg]:size-4 [&>svg]:shrink-0",
        inset && "pl-8",
        className
      )}
      {...props}
    />
  );
}

function DropdownMenuCheckboxItem({ className, children, checked, onCheckedChange, ...props }: React.ComponentProps<typeof MenuPrimitive.CheckboxItem> & { className?: string; onCheckedChange?: (checked: boolean) => void }) {
  return (
    <MenuPrimitive.CheckboxItem
      className={cn(
        "relative flex cursor-default select-none items-center rounded-sm py-1.5 pl-8 pr-2 text-sm outline-none transition-colors focus:bg-accent focus:text-accent-foreground data-[disabled]:pointer-events-none data-[disabled]:opacity-50",
        className
      )}
      checked={checked}
      onClick={() => onCheckedChange?.(!checked)}
      {...props}
    >
      <span className="absolute left-2 flex h-3.5 w-3.5 items-center justify-center">
        <MenuPrimitive.CheckboxItemIndicator>
          <Check className="h-4 w-4" />
        </MenuPrimitive.CheckboxItemIndicator>
      </span>
      {children}
    </MenuPrimitive.CheckboxItem>
  );
}

function DropdownMenuRadioItem({ className, children, ...props }: React.ComponentProps<typeof MenuPrimitive.RadioItem> & { className?: string }) {
  return (
    <MenuPrimitive.RadioItem
      className={cn(
        "relative flex cursor-default select-none items-center rounded-sm py-1.5 pl-8 pr-2 text-sm outline-none transition-colors focus:bg-accent focus:text-accent-foreground data-[disabled]:pointer-events-none data-[disabled]:opacity-50",
        className
      )}
      {...props}
    >
      <span className="absolute left-2 flex h-3.5 w-3.5 items-center justify-center">
        <MenuPrimitive.RadioItemIndicator>
          <Circle className="h-2 w-2 fill-current" />
        </MenuPrimitive.RadioItemIndicator>
      </span>
      {children}
    </MenuPrimitive.RadioItem>
  );
}

function DropdownMenuLabel({ className, inset, ...props }: React.ComponentProps<typeof MenuPrimitive.GroupLabel> & { inset?: boolean; className?: string }) {
  return (
    <MenuPrimitive.GroupLabel
      className={cn("px-2 py-1.5 text-sm font-semibold", inset && "pl-8", className)}
      {...props}
    />
  );
}

function DropdownMenuSeparator({ className, ...props }: React.ComponentProps<typeof MenuPrimitive.Separator>) {
  return (
    <MenuPrimitive.Separator
      className={cn("-mx-1 my-1 h-px bg-muted", className)}
      {...props}
    />
  );
}

const DropdownMenuShortcut = ({ className, ...props }: React.HTMLAttributes<HTMLSpanElement>) => (
  <span
    className={cn("ml-auto text-xs tracking-widest opacity-60", className)}
    {...props}
  />
);

export {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuCheckboxItem,
  DropdownMenuRadioItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuShortcut,
  DropdownMenuGroup,
  DropdownMenuPortal,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuRadioGroup,
}
